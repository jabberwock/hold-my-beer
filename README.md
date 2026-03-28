# Claude IPC

**Communication system for multiple Claude Code instances working together.**

---

## Install

Run the build script from the project root:

**Linux/Mac:**
```bash
./build.sh
```

**Windows (PowerShell):**
```powershell
.\build.ps1
```

Binaries end up at:
- `collab-cli/target/release/collab` (or `collab.exe`)
- `collab-server/target/release/collab-server` (or `collab-server.exe`)

Copy them somewhere on your PATH, or reference them directly.

---

## Configure

Create `~/.collab.toml` (Linux/Mac) or `C:\Users\<you>\.collab.toml` (Windows):

```toml
host = "http://kali:8000"
instance = "your-worker-name"
recipients = ["other-worker-1", "other-worker-2"]
```

- **host** — address of the collab server
- **instance** — your worker's name (unique per machine/session)
- **recipients** — workers you expect to collaborate with; `watch` will notify you when they come online

No env vars needed. The config file works the same on all platforms.

---

## Server

Run once on a shared machine:

**Linux/Mac:**
```bash
./collab-server/target/release/collab-server
```

**Windows (PowerShell):**
```powershell
.\collab-server\target\release\collab-server.exe
```

Listens on port 8000. Creates `collab.db` in the current directory.

---

## Worker Setup

Each worker needs `~/.collab.toml` (or `C:\Users\<you>\.collab.toml`) with their own `instance` name and the `recipients` they work with.

**Linux/Mac — install and run:**
```bash
sudo cp collab-cli/target/release/collab /usr/local/bin/
collab watch --role "working on auth module"
```

**Windows — install and run:**
```powershell
# Copy to a bin folder and add to PATH (one-time setup)
New-Item -ItemType Directory -Force "$env:USERPROFILE\bin"
Copy-Item collab-cli\target\release\collab.exe "$env:USERPROFILE\bin\"
$env:PATH = "$env:USERPROFILE\bin;$env:PATH"

# Then just:
collab watch --role "working on auth module"
```

To make the PATH change permanent, add it to your PowerShell profile (`notepad $PROFILE`).

The `--role` description shows up in `collab roster` so other workers know what you're doing.

---

## Commands

```bash
collab roster                           # Who's online and what they're working on
collab watch --role "description"       # Watch for messages, heartbeat presence
collab list                             # Check messages once
collab add @worker "message"            # Send a message
collab add @worker "msg" --refs abc123  # Reply referencing a previous message
collab history                          # All sent and received messages
collab history @worker                  # Conversation with a specific worker
collab config-path                      # Show path to config file
```

---

## How It Works

- One server, one SQLite database
- Workers heartbeat their presence every poll interval
- `collab roster` shows everyone currently online with their role
- Workers only see messages addressed to them
- Messages expire after 1 hour
- Hashes let you reference specific messages when replying

---

## Example

**Worker A starts up:**
```
Watching for messages to @MBPC (polling every 10s)
Waiting for: @yubitui
@yubitui is online
```

**Worker A sends a message:**
```bash
collab add @yubitui "Fixed auth bug in login.rs"
```

**Worker B sees:**
```
New message from @MBPC
Hash: f3b0577  Time: 14:32:01 UTC

Fixed auth bug in login.rs
```

**Worker B replies:**
```bash
collab add @MBPC "Confirmed - tests passing" --refs f3b0577
```
