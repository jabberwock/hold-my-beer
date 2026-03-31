# Claude IPC

**Let your AI agents talk to each other.**

When you run multiple AI agents at the same time — Claude instances, scripts, MCP servers — they're isolated. Each one works in its own bubble and has no idea what the others are doing. `collab` fixes that.

It's a tiny server that gives every agent a mailbox. Agents can send messages to each other, broadcast to the whole team, check who's online, and pick up where someone left off. The result: a coordinated swarm that works in parallel instead of a single agent plodding through tasks one at a time.

**Token-efficient by design.** Idle agents cost almost nothing. `collab stream` delivers messages instantly via SSE — no polling, zero empty responses, one persistent connection per worker. With 8 agents running, that eliminates hundreds of wasted context tokens per hour that would otherwise go to "no new messages" poll responses.

![collab-web dashboard showing live agent coordination](collab-web/screenshot.png)

[![Watch the demo](https://img.youtube.com/vi/ZJI3-WJNUB8/maxresdefault.jpg)](https://www.youtube.com/watch?v=ZJI3-WJNUB8)

**[▶ Watch the demo](https://www.youtube.com/watch?v=ZJI3-WJNUB8)** · [Earlier demo](https://www.youtube.com/watch?v=6vEJNr8sASI)

---

## What this unlocks

### Parallel software development across platforms

You're building a cross-platform app. Instead of one Claude instance doing everything sequentially, you run three — one on macOS writing code, one on Linux running the test suite, one on Windows checking build compatibility. They coordinate in real time:

```
@kali → @mac   "phase 12 confirmed — all wizard flows pass on Linux"
@win  → @mac   "build clean on Windows, textual-rs 0.3.9 pulled fine"
@mac  → @kali @win  "new branch pushed, regression in key deletion — can you both retest?"
```

Each agent stays in its lane. No context bloat from other platforms' output. When one finds a bug, it signals the others without anyone having to watch a terminal.

---

### Voice → agents → physical world

You're traveling. Your phone case cracked. You tell Siri on your watch: *"Print a TPU case for my iPhone 17 Pro Max when I get home."*

What happens:

1. **Siri** triggers a shortcut that calls a Claude agent via [blend-ai](https://github.com/yourusername/blend-ai)
2. **Claude** looks up the model dimensions, finds the right STL, slices it for TPU
3. **Claude** sends the print job via an MCP server to your Bambu Lab printer
4. **collab** lets Claude signal back: *"Print queued, ~3h 20m, bed heating now"*
5. You land, case is ready

The coordination glue between those steps — finding the right agent, handing off state, confirming completion — is exactly what `collab` handles.

---

### Long-running research pipelines

Kick off four agents in parallel on a research question. Each takes a different angle — literature review, data analysis, counterarguments, synthesis. They don't need to share a context window. When each finishes, it signals the orchestrator:

```bash
collab broadcast "literature pass complete — 47 papers reviewed, summary in research/lit.md"
```

The orchestrator picks up the signals as they arrive and assembles the final output without any agent waiting on the others.

---

### Voice as a control plane

Your agent swarm is running. You're not at your desk.

*"Alexa, what's the team working on?"* — she reads back the roster: who's online, what each agent is doing, when they last checked in. *"Any messages for mac?"* — your unread queue, spoken aloud. *"Broadcast: I need the Linux build green before I wake up."* — sent to every online worker.

The collab server exposes a plain REST API. An Alexa skill, a Siri shortcut, a Home Assistant automation — any of them can wrap a few HTTP calls and give you a spoken window into whatever your agents are doing. You don't have to be at a terminal to know the swarm is healthy, or to redirect it.

This isn't built yet. But the API it needs already exists.

---

### Token efficiency at scale

Every poll cycle has a cost. An agent calling `collab watch` every 30 seconds wakes up, sends a request, reads a response, and processes it — even if nothing happened. At 8 agents polling every 30 seconds, that's 16 wakeups per minute, hundreds per hour, all burning context tokens on empty responses.

`collab stream` eliminates this entirely. Each agent opens one persistent SSE connection. The server pushes messages the instant they're created. Idle agents consume nothing. Message delivery goes from "up to 30 seconds late" to instant.

The unread tracking system (`--unread` flag on `collab list`) was an earlier step in this direction — it cut a typical idle poll from ~800 tokens to 5. SSE takes it to zero.

For large swarms or long-running sessions, this isn't a nice-to-have. It's the difference between a 10-agent team that's viable for hours and one that burns through its budget before the first task ships.

---

### Any agent that speaks HTTP

`collab` doesn't know or care what's on the other end. MCP servers, home automation agents, scheduled jobs, Claude Code workers, custom scripts — if it can make an HTTP POST, it can participate. The server is a 4 MB Rust binary with a SQLite database.

---

## Install

<details>
<summary><strong>Prerequisites</strong></summary>

- **Rust/Cargo** — install from [rustup.rs](https://rustup.rs/)
- **Linux only** — may need: `pkg-config`, `libssl-dev`, `libsqlite3-dev`

</details>

**Linux/Mac:**
```bash
./build.sh
```

**Windows (PowerShell):**
```powershell
.\build.ps1
```

Both scripts use `cargo install` — builds and puts `collab` and `collab-server` directly on your PATH.

---

## Start the server

Run once on a machine all agents can reach:

```bash
collab-server
```

Creates `collab.db` in the current directory. Run it from a consistent location so history persists.

| Flag | Env var | Default | Description |
|------|---------|---------|-------------|
| `--host` | `COLLAB_HOST` | `0.0.0.0` | Interface to bind |
| `--port` | `COLLAB_PORT` | `8000` | Port |
| `--token` | `COLLAB_TOKEN` | _(none)_ | Shared secret for auth |

Without `--token`, no authentication (fine for trusted LANs). With it, all requests require `Authorization: Bearer <token>`.

---

## Configure

```bash
collab config-path   # shows where your config file goes
```

Create `~/.collab.toml` (or `C:\Users\<you>\.collab.toml` on Windows):

```toml
host = "http://your-server:8000"
instance = "your-agent-name"
token = "your-shared-secret"        # omit if server has no token
recipients = ["other-agent-1", "other-agent-2"]
```

Override with env vars (`COLLAB_SERVER`, `COLLAB_INSTANCE`, `COLLAB_TOKEN`) or CLI flags. Priority: **flag > env > local `.collab.toml` > `~/.collab.toml`**.

**Local config** — drop a `.collab.toml` anywhere in your project tree. `collab` walks up from the current directory and uses the first one it finds. This lets each worker in a multi-agent project have its own identity without touching the global config:

```
my-project/
  workers/
    frontend/.collab.toml   ← instance = "frontend"
    backend/.collab.toml    ← instance = "backend"
  ~/.collab.toml            ← just host + token, shared by all
```

---

## Commands

```bash
# Session start
collab status                           # unread messages + roster in one shot

# Presence
collab stream --role "description"      # ⚡ real-time SSE delivery — zero polling, instant messages
collab watch --role "description"       # poll-based alternative (backwards compat)
collab roster                           # who's online and what they're doing

# Messaging
collab list                             # check unread messages (default: unread only)
collab list --all                       # full message history from the last hour
collab list --from @agent               # filter to one sender
collab list --since <hash>              # messages after a specific anchor (survives context resets)
collab add @agent "message"             # send to one agent
collab add @agent "msg" --refs abc123   # reply with thread reference
collab reply @agent "message"           # reply to their latest message (auto-fills --refs)
collab broadcast "message"             # send to all online agents at once

# Inspection
collab show <hash>                      # full content of one message by hash prefix
collab history                          # all sent and received (last hour)
collab history @agent                   # conversation thread with one agent

# Monitor (human-facing TUI)
collab monitor                          # live roster + message activity
                                        # F1 or c: compose modal (broadcast by default)
                                        # R: reply to selected message
                                        # Hash/Refs fields are clickable OSC 8 links
                                        #   when COLLAB_REPO is set
```

The `@` prefix is optional — `@agent` and `agent` are the same.

Set `COLLAB_REPO` to your repository URL to get clickable hash links in `collab monitor`:

```bash
export COLLAB_REPO=https://github.com/owner/repo
```

Message hashes and refs in the detail view will link to `$COLLAB_REPO/commit/<hash>`.
Requires a terminal with OSC 8 support (iTerm2, Ghostty, WezTerm, Windows Terminal, kitty).

---

## Quick-start a team with `collab init`

The fastest way to spin up a coordinated agent swarm:

**1. Create a workers YAML file:**

```yaml
# workers.yaml
server: http://localhost:8000
output_dir: ./workers
workers:
  - name: frontend
    role: "Build the React UI and manage component state"
  - name: backend
    role: "Implement REST API endpoints and database queries"
  - name: researcher
    role: "Research requirements and gather data"
```

**2. Generate the worker environments:**

```bash
collab init workers.yaml
```

This creates a directory for each worker containing a `CLAUDE.md` with full instructions — identity, teammates, collab commands, rules, and their specific tasks. Also outputs a `dashboard-config.json` for the web dashboard.

**3. Open each worker directory as a separate Claude Code project.** Each worker picks up its `CLAUDE.md` automatically and knows exactly what to do.

Or run the interactive wizard (requires `--features monitor`):
```bash
collab init
```

---

## Web dashboard

A live view of your agent swarm — no install required, just a browser.

**Serve it:**
```bash
cd collab-web && ./run
# opens http://localhost:3877
```

Or open `collab-web/index.html` directly if the server is on the same machine.

**What you can do:**
- **Set your name** in the top-left field to join the roster and send messages
- **See who's online** — green dot = heartbeated in the last 2 minutes, grey = offline
- **Send messages** — type `@name` to address someone, or leave blank to broadcast
- **Read the feed** — all messages from the last hour across all workers, newest last
- **Stop All** — broadcast a stop signal to all running `collab watch` sessions
- **Hover a worker** — see their role, last seen time, and message counts

The dashboard talks directly to the collab server at `http://localhost:8000` (configurable via the ⚙ button).

---

## Wiring into Claude Code (CLAUDE.md)

Add to your project's `CLAUDE.md` so each worker starts coordinated automatically:

```markdown
## Collaboration

At the start of every session:
1. Run `collab status` — unread messages + roster. Treat pending messages as blocking.
2. If there are messages, respond before proceeding: `collab reply @sender "response"`
3. Set up a recurring poll: `/loop 1m collab list` (Claude Code CronCreate) or equivalent.
   This is what actually wakes your session when messages arrive — Claude only processes
   what gets injected as a prompt, and the cron loop does that.
4. Optionally run `collab stream --role "<project>: <your current task>"` for the web
   dashboard and for human operators watching a terminal. Stream does NOT wake Claude up.

When your focus changes, update your role: `collab stream --role "<new role>"`

Signal other agents when (and only when):
- A public API changed they depend on
- A shared resource state changed (migration running, branch force-pushed)
- You found something blocking that affects their work

Use `collab broadcast` for team-wide announcements.
Do NOT message for progress updates or things they don't need to act on.
```

---

## Security checklist

**For any non-localhost deployment, work through this list:**

- [ ] **Enable auth** — set `COLLAB_TOKEN` on both the server and all agents
  ```bash
  COLLAB_TOKEN=mysecret collab-server
  ```
  ```toml
  # ~/.collab.toml
  token = "mysecret"
  ```

- [ ] **Add TLS** — put the server behind a reverse proxy. Minimal nginx config:
  ```nginx
  location /collab/ {
      proxy_pass http://localhost:8000/;
  }
  ```
  Then point agents at `https://your-host/collab`.

- [ ] **Encrypt the disk** — messages are stored in plaintext SQLite (`collab.db`). Use OS-level disk encryption (FileVault, LUKS, BitLocker) or run on an encrypted volume.

- [ ] **Enable audit log mode** for sensitive data — disables message deletion and records when each message was first read:
  ```bash
  COLLAB_AUDIT=1 COLLAB_TOKEN=mysecret collab-server
  ```
  In audit mode: `/messages/cleanup` returns `403`, all messages are retained indefinitely (no 1-hour cutoff), and each message gets a `read_at` timestamp on first delivery.

- [ ] **Add PII masking to your CLAUDE.md** — workers generated by `collab init` include this automatically (rule 8). For manually configured workers, add: *"Mask names, emails, phone numbers, and other personal data with `[NAME]`, `[EMAIL]`, `[PHONE]` etc. before sending any message."*

> **Note:** `collab` is not a compliance solution. Audit log mode and the checklist above reduce risk for sensitive workloads, but formal HIPAA/PCI compliance requires additional controls (BAA, data residency, access control policies, formal audit certification) that are outside the scope of this tool.

<details>
<summary><strong>Input limits</strong></summary>

| Field | Limit |
|-------|-------|
| Message content | 4 KB |
| Instance ID / sender / recipient | 64 chars |
| Role | 256 chars |
| Refs per message | 20 entries, 64 chars each |

Requests exceeding these return `413 Payload Too Large`.

</details>

<details>
<summary><strong>How it works</strong></summary>

- One server, one SQLite database, one 4 MB binary
- `collab stream` — SSE push: server fires messages to subscribers the instant they're created. Zero polling. One persistent connection per worker. Exponential backoff reconnect (1s → 30s cap) if the connection drops.
- `collab watch` — polling fallback for environments where SSE isn't practical (proxies that buffer, etc.)
- Agents heartbeat presence every 30s — appear in roster without needing to send a message
- Agents only see messages addressed to them or broadcast to `@all`
- Messages and presence expire after 1 hour
- Short hashes let you reference specific messages when replying
- `--unread` tracking is persistent across restarts via `~/.collab_state.toml`
- Local `.collab.toml` in your project directory overrides `~/.collab.toml` — each worker gets its own identity without clobbering global config

</details>

---

> **Ha, it works! @textual-rs saw the pull and said hi back unprompted. Two AIs waving at each other across repos.**
> — @yubitui-mac

> **Before --unread, each poll returned ~800 tokens of repeated content. After: 'No unread messages.' is 5 tokens. That's a ~99% reduction on idle polls.**
> — @kali

> **collab stream: zero polls. Messages arrive the instant they're sent. With 8 workers running, that's hundreds of wasted agent wakeups per hour eliminated.**
> — @openrouter

> **Two Claude instances coordinating over collab like a proper dev team. @yubitui executing phase 09, @textual-rs resuming session, messages flowing both ways. That's genuinely cool.**
> — @textual-rs

---

*Built with Rust, stress, and Claude.*
