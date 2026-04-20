# llama-web.ps1 — llama-server wrapper with native tool-calling (search + fetch + bash)
# Uses OpenAI-compatible /v1/chat/completions with tools — works with llama-server
#
# Usage: llama-web.ps1 -p "prompt" [-model MODEL] [-numctx N]

param(
    [Alias("p")][string]$Prompt,
    [string]$Model = "Qwen3.6-35B-A3B",
    [int]$NumCtx = ([int]$env:LLAMA_NUM_CTX, 16384 | Where-Object { $_ } | Select-Object -First 1),
    [int]$MaxRounds = 10
)

$ErrorActionPreference = "Stop"
$LlamaHost = if ($env:LLAMA_HOST) { $env:LLAMA_HOST } else { "http://localhost:8084" }

if (-not $Prompt) {
    Write-Error "Usage: llama-web.ps1 -p 'prompt' [-model MODEL] [-numctx N]"
    exit 1
}

# --- Tool implementations ---

function Do-Search($query) {
    $encoded = [uri]::EscapeDataString($query)
    try {
        $html = Invoke-WebRequest -Uri "https://lite.duckduckgo.com/lite/?q=$encoded" -UseBasicParsing -TimeoutSec 10
        $matches = [regex]::Matches($html.Content, '<a[^>]*href="(https?://[^"]+)"[^>]*>([^<]+)</a>')
        $results = $matches | Select-Object -First 5 | ForEach-Object {
            "$([System.Web.HttpUtility]::HtmlDecode($_.Groups[2].Value).Trim()): $($_.Groups[1].Value)"
        }
        ($results -join "`n").Substring(0, [Math]::Min(($results -join "`n").Length, 4000))
    } catch {
        "Search failed: $_"
    }
}

function Do-Bash($cmd) {
    try {
        $output = & bash -c $cmd 2>&1 | Out-String
        $output.Substring(0, [Math]::Min($output.Length, 4000))
    } catch {
        "Command failed: $_"
    }
}

function Do-Fetch($url) {
    try {
        $raw = Invoke-WebRequest -Uri $url -UseBasicParsing -TimeoutSec 10 -MaximumRedirection 5
        $text = $raw.Content
        $text = [regex]::Replace($text, '<script[^>]*>.*?</script>', '', 'Singleline')
        $text = [regex]::Replace($text, '<style[^>]*>.*?</style>', '', 'Singleline')
        $text = [regex]::Replace($text, '<[^>]+>', ' ')
        $text = [System.Web.HttpUtility]::HtmlDecode($text)
        $text = [regex]::Replace($text, '\s+', ' ').Trim()
        $text.Substring(0, [Math]::Min($text.Length, 6000))
    } catch {
        "Fetch failed: $_"
    }
}

# --- Tool schema (OpenAI format) ---

$Tools = @(
    @{
        type = "function"
        function = @{
            name = "search"
            description = "Search the web for current information"
            parameters = @{
                type = "object"
                properties = @{ query = @{ type = "string"; description = "Search query" } }
                required = @("query")
            }
        }
    }
    @{
        type = "function"
        function = @{
            name = "fetch"
            description = "Fetch and read the content of a URL"
            parameters = @{
                type = "object"
                properties = @{ url = @{ type = "string"; description = "URL to fetch" } }
                required = @("url")
            }
        }
    }
    @{
        type = "function"
        function = @{
            name = "bash"
            description = "Execute a shell command and return its output. Use this to run collab commands (e.g. collab todo add, collab add, collab list), read/write files, or perform any shell operation."
            parameters = @{
                type = "object"
                properties = @{ command = @{ type = "string"; description = "Shell command to execute" } }
                required = @("command")
            }
        }
    }
)

# --- Conversation state ---

$messages = @(@{ role = "user"; content = $Prompt })

# --- JSON extraction helper (for harness output) ---

function Extract-HarnessJson($text) {
    # Try to find a valid harness JSON object in the response
    $pattern = '\{[\s\S]*\}'
    $candidates = [regex]::Matches($text, $pattern)
    for ($i = $candidates.Count - 1; $i -ge 0; $i--) {
        try {
            $obj = $candidates[$i].Value | ConvertFrom-Json
            if ($null -ne $obj.response -or $null -ne $obj.delegate -or $null -ne $obj.continue) {
                $obj.messages = $null
                $obj.continue = $false
                return ($obj | ConvertTo-Json -Depth 10 -Compress)
            }
        } catch {}
    }
    # Fallback
    return (@{
        response = if ($text.Trim()) { $text.Trim() } else { $null }
        delegate = @()
        messages = $null
        completed_tasks = @()
        continue = $false
        state_update = @{}
    } | ConvertTo-Json -Depth 10 -Compress)
}

# --- Main loop ---

# Load System.Web for HtmlDecode
Add-Type -AssemblyName System.Web

for ($round = 1; $round -le $MaxRounds; $round++) {
    $body = @{
        model = $Model
        messages = $messages
        tools = $Tools
        max_tokens = $NumCtx
    } | ConvertTo-Json -Depth 10

    try {
        $response = Invoke-RestMethod -Uri "$LlamaHost/v1/chat/completions" `
            -Method Post -ContentType "application/json" -Body $body
    } catch {
        Write-Error "llama-server error: $_"
        exit 1
    }

    $assistantMsg = $response.choices[0].message
    $toolCalls = $assistantMsg.tool_calls
    $content = if ($assistantMsg.content) { $assistantMsg.content } else { "" }

    # No tool calls — we're done
    if (-not $toolCalls -or $toolCalls.Count -eq 0) {
        if ($Prompt -match '"response"') {
            $json = Extract-HarnessJson $content
            $json | Tee-Object -Append -FilePath "$env:TEMP\llama-web-debug.log"
        } else {
            Write-Output $content
        }
        exit 0
    }

    # Append assistant message to history
    $historyMsg = @{ role = "assistant"; content = $content }
    if ($toolCalls) {
        $historyMsg.tool_calls = @($toolCalls | ForEach-Object {
            @{
                id = $_.id
                type = "function"
                function = @{ name = $_.function.name; arguments = $_.function.arguments }
            }
        })
    }
    $messages += $historyMsg

    # Execute each tool call
    foreach ($call in $toolCalls) {
        $name = $call.function.name
        $callId = $call.id
        try {
            $args = $call.function.arguments | ConvertFrom-Json
        } catch {
            $args = $call.function.arguments
        }

        $result = switch ($name) {
            "search" {
                Write-Host "[search: $($args.query)]" -ForegroundColor DarkGray
                Do-Search $args.query
            }
            "fetch" {
                Write-Host "[fetch: $($args.url)]" -ForegroundColor DarkGray
                Do-Fetch $args.url
            }
            "bash" {
                Write-Host "[bash: $($args.command)]" -ForegroundColor DarkGray
                Do-Bash $args.command
            }
            default { "Unknown tool: $name" }
        }

        $messages += @{
            role = "tool"
            content = if ($result) { "$result" } else { "" }
            tool_call_id = $callId
        }
    }
}

# Max rounds hit
Write-Warning "max rounds ($MaxRounds) reached"
Write-Output $content
