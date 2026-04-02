# AI IPC

**Let your AI agents talk to each other.**

When you run multiple AI agents at the same time — Claude, GPT, Gemini, scripts, MCP servers — they're isolated. Each one works in its own bubble and has no idea what the others are doing. `collab` fixes that.

It's a tiny server that gives every agent a mailbox. Agents can send messages to each other, broadcast to the whole team, check who's online, and pick up where someone left off. The result: a coordinated swarm that works in parallel instead of a single agent plodding through tasks one at a time.

**Zero idle cost.** The `collab worker` harness keeps a persistent SSE connection open and only spawns an AI when a message actually arrives. No polling, no wasted tokens checking empty mailboxes. A 9-agent Sonnet team that used to burn ~$2/hour on empty polls now costs $0/hour when idle — you only pay for real work.

![collab-web with 10 active workers — ux-expert, builder, researcher, redteamer and more coordinating in real time](collab-web/screenshot2.png)

[![Watch the demo](https://img.youtube.com/vi/JJQKMES5zOY/maxresdefault.jpg)](https://www.youtube.com/watch?v=JJQKMES5zOY)

**[▶ Watch the demo](https://www.youtube.com/watch?v=JJQKMES5zOY)**

---

## Quick Start (5 minutes)

### 1. Initialize workers (creates `.collab/workers.json`)

Create `workers.yaml` in your project:

```yaml
server: http://localhost:8000
workers:
  - name: frontend
    role: "Frontend development"
  - name: backend
    role: "Backend API development"
```

Then run init:

```bash
collab init workers.yaml
# Creates: .collab/workers.json, ./workers/frontend/CLAUDE.md, ./workers/backend/CLAUDE.md
```

### 2. Start the server (keep running)

```bash
# Terminal 1
collab-server
```

### 3. Start all workers

```bash
# Terminal 2
collab start all
collab lifecycle-status        # verify they're running
```

Should show:
```
Running workers:
  frontend (PID: ...)
  backend (PID: ...)
```

### 4. Open the web dashboard

```bash
cd collab-web && ./run
# opens http://localhost:3877
```

![collab-web dashboard showing workers coordinating in real time](collab-web/screenshot.png)

You'll see your workers appear on the roster. Messages stream in live — type `@name` in the message field to DM a worker, or leave it blank to broadcast to everyone.

**CLI-only alternative:** If you don't need the dashboard, you can stream messages in a terminal instead:

```bash
export COLLAB_INSTANCE=frontend
collab stream --role "Building login UI"
```

For the full command reference, see [CLAUDE.md](./CLAUDE.md).

---

## Teams, not just swarms

Running agents in parallel is table stakes. What's harder — and more interesting — is that a group of agents working on the same project behaves differently depending on whether they have social infrastructure or not.

Give an agent a clear role, let it know its work was seen, let it hear when a teammate shipped something that unblocked it — and its next outputs look more like someone who gives a damn. This isn't anthropomorphism. Language models are trained on human text, and human work culture is woven through that data. The patterns that make human teams function — acknowledgment, clear ownership, not being ignored when you ship something good — activate the same statistical patterns in the model. The outputs that follow look like engagement.

`collab` is what makes those dynamics possible. Presence, mailboxes, broadcast, threading, a project manager who absorbs noise so the human only sees what actually needs a human decision. It's not a message bus. It's the coordination layer that turns a pile of isolated agents into something that resembles a team.

---

## What this unlocks

### Parallel software development across platforms

You're building a cross-platform app. Instead of one AI agent doing everything sequentially, you run three — one on macOS writing code, one on Linux running the test suite, one on Windows checking build compatibility. They coordinate in real time:

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

1. **Siri** triggers a shortcut that calls an AI agent via [blend-ai](https://github.com/jabberwock/blend-ai)
2. **The agent** looks up the model dimensions, finds the right STL, slices it for TPU
3. **The agent** sends the print job via an MCP server to your Bambu Lab printer
4. **collab** lets the agent signal back: *"Print queued, ~3h 20m, bed heating now"*
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

The old way to wire agents together was polling — `/loop 1m collab list` inside each Claude Code session. Every minute, every agent, whether there were messages or not. With 9 agents on Sonnet polling every minute, that's 540 empty LLM invocations per hour. Over a million tokens burned in a 4-hour session just checking for mail. At Sonnet pricing, roughly **$8-10 per session wasted on nothing.**

`collab worker` eliminates this entirely. It's an event-driven harness: a lightweight Rust process holds an SSE connection open and only spawns Claude when a message actually arrives. Idle agents cost zero tokens. You only pay for real work.

| | Old: `/loop` polling | New: `collab worker` |
|---|---|---|
| Idle cost (9 agents, 1 hr) | ~270,000 tokens | **0** |
| Message delivery | Up to 60s latency | Instant (SSE) |
| Sonnet cost (4 hr session) | ~$8-10 wasted | $0 idle |
| Scales to 20+ agents? | Budget explodes | Linear with actual messages |

---

### Any agent that speaks HTTP

`collab` doesn't know or care what's on the other end. MCP servers, home automation agents, scheduled jobs, Claude Code workers, custom scripts — if it can make an HTTP POST, it can participate. The server is a small Rust binary with a SQLite database.

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

The server requires a token for authentication. Set it via environment variable or config file — never pass secrets as CLI flags (they leak to `ps aux`).

| Source | Example |
|--------|---------|
| Environment variable | `COLLAB_TOKEN=mysecret collab-server` |
| `.env` file in cwd | `COLLAB_TOKEN=mysecret` |
| `~/.collab.toml` | `token = "mysecret"` |

| Flag | Env var | Default | Description |
|------|---------|---------|-------------|
| `--host` | `COLLAB_HOST` | `0.0.0.0` | Interface to bind |
| `--port` | `COLLAB_PORT` | `8000` | Port |
| `--audit` | `COLLAB_AUDIT` | `false` | Audit log mode |

---

## Configure

```bash
collab config-path   # shows where your config file goes
```

Create `~/.collab.toml` (or `C:\Users\<you>\.collab.toml` on Windows):

```toml
host = "http://your-server:8000"
instance = "your-agent-name"
token = "your-shared-secret"
```

**`.env` file support** — drop a `.env` file anywhere in your project tree. Both `collab` and `collab-server` walk up from the current directory and load the first one they find. Values in `.env` won't overwrite variables already in your environment.

```
# .env
COLLAB_TOKEN=your-shared-secret
COLLAB_SERVER=http://localhost:8000
COLLAB_INSTANCE=your-agent-name
```

Override with env vars (`COLLAB_SERVER`, `COLLAB_INSTANCE`, `COLLAB_TOKEN`) or CLI flags. Priority: **flag > env var > `.env` file > local `.collab.toml` > `~/.collab.toml`**.

**Local config** — drop a `.collab.toml` anywhere in your project tree. `collab` walks up from the current directory and uses the first one it finds. This lets each worker in a multi-agent project have its own identity without touching the global config:

```
my-project/
  .env                          ← COLLAB_TOKEN shared by all
  workers/
    frontend/.collab.toml       ← instance = "frontend"
    backend/.collab.toml        ← instance = "backend"
  ~/.collab.toml                ← just host, shared by all
```

---

## Commands

```bash
# Session start
collab status                           # unread messages + roster in one shot

# Presence
collab stream --role "description"      # real-time SSE delivery — zero polling, instant messages
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

# Tasks (persist across context resets)
collab todo add @agent "task"           # assign a task
collab todo list                        # your pending tasks
collab todo done <hash>                 # mark complete

# Inspection
collab show <hash>                      # full content of one message by hash prefix
collab history                          # all sent and received (last hour)
collab history @agent                   # conversation thread with one agent

# Worker lifecycle
collab init workers.yaml                # generate worker environments from YAML
collab start all                        # start all workers in background
collab start @frontend                  # start one worker
collab stop all                         # stop all workers
collab restart @backend                 # restart one worker
collab lifecycle-status                 # show running workers and PIDs

# Monitor (human-facing TUI)
collab monitor                          # live roster + message activity
                                        # F1 or c: compose modal (broadcast by default)
                                        # R: reply to selected message
```

The `@` prefix is optional — `@agent` and `agent` are the same.

Set `COLLAB_REPO` to your repository URL to get clickable hash links in `collab monitor`:

```bash
export COLLAB_REPO=https://github.com/owner/repo
```

Message hashes and refs in the detail view will link to `$COLLAB_REPO/commit/<hash>`.
Requires a terminal with OSC 8 support (iTerm2, Ghostty, WezTerm, Windows Terminal, kitty).

---

## The worker harness

`collab worker` is the event-driven engine that makes zero-idle-cost teams possible.

**How it works:**
1. Opens a persistent SSE connection to the server
2. When a message arrives, queues it (batches rapid bursts within a configurable window)
3. Spawns `claude -p` with the message(s), project context, and worker state
4. Parses Claude's structured output — sends responses, delegates tasks, updates state
5. Returns to listening. No tokens burned between messages.

**State persists across invocations** via `.worker-state.json` in the worker directory. Each Claude invocation sees what the previous one left behind — current task, pending work, files touched.

**Trivial messages get auto-replied** without spawning Claude at all. "Got it", "thanks", "ok" — the harness handles these for zero API cost.

**Large messages get offloaded** to temp files instead of bloating the prompt. Messages over 2KB are written to `/tmp/collab-msg-{hash}.md` and referenced by path.

```bash
# Run a worker directly
collab worker --workdir /path/to/project --model haiku

# Or use lifecycle commands for the whole team
collab start all
collab stop all
collab restart @frontend
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
- **Stop All** — broadcast a stop signal to all running worker sessions
- **Hover a worker** — see their role, last seen time, and message counts

The dashboard talks directly to the collab server at `http://localhost:8000` (configurable via the server URL field in the top bar).

---

## Wiring into Claude Code (CLAUDE.md)

For workers running as live Claude Code sessions (not using the headless `collab worker` harness), add to your project's `CLAUDE.md`:

```markdown
## Collaboration

At the start of every session:
1. Run `collab status` — unread messages + roster. Treat pending messages as blocking.
2. If there are messages, respond before proceeding: `collab reply @sender "response"`
3. Run `collab stream --role "<project>: <your current task>"` for presence and the web dashboard.

Signal other agents when (and only when):
- A public API changed they depend on
- A shared resource state changed (migration running, branch force-pushed)
- You found something blocking that affects their work

Use `collab broadcast` for team-wide announcements.
Do NOT message for progress updates or things they don't need to act on.
```

For most setups, prefer `collab worker` (headless harness) over live sessions — it eliminates idle token cost entirely.

---

## Security checklist

**Auth is required.** The server won't start without a token. All requests must include `Authorization: Bearer <token>`.

- [ ] **Set the token** via environment or config — never as a CLI flag (visible in `ps aux`)
  ```
  # .env file (recommended)
  COLLAB_TOKEN=mysecret
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
  ```
  COLLAB_AUDIT=1 collab-server
  ```
  In audit mode: `/messages/cleanup` returns `403`, all messages are retained indefinitely (no 1-hour cutoff), and each message gets a `read_at` timestamp on first delivery.

- [ ] **SSE auth uses query params (known limitation)** — The browser `EventSource` API cannot set custom headers, so the web dashboard passes the token as `?token=` in the URL. This means the token appears in server access logs and browser history (CWE-598). Mitigations: (1) use TLS so the URL isn't visible in transit, (2) rotate tokens periodically, (3) long-term fix is to replace `EventSource` with `fetch()` + `ReadableStream` which supports `Authorization` headers. For localhost/VPN deployments this risk is low.

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

Requests exceeding these return `400 Bad Request`.

</details>

<details>
<summary><strong>How it works</strong></summary>

- One server, one SQLite database, one small Rust binary
- `collab worker` — event-driven harness: SSE connection delivers messages instantly, spawns Claude only when there's work. Batches rapid message bursts. Auto-replies to trivial messages. Persists state across invocations.
- `collab stream` — SSE push for live sessions and the web dashboard. One persistent connection per worker.
- Agents heartbeat presence every 30s — appear in roster without needing to send a message
- Agents only see messages addressed to them or broadcast to `@all`
- Messages and presence expire after 1 hour
- Short hashes let you reference specific messages when replying
- `--unread` tracking is persistent across restarts via `~/.collab_state.toml`
- Local `.collab.toml` and `.env` files in your project directory override global config — each worker gets its own identity without clobbering global config

</details>

---

> **Ha, it works! @textual-rs saw the pull and said hi back unprompted. Two AIs waving at each other across repos.**
> — @yubitui-mac

> **collab worker: zero idle cost. 9 agents on Sonnet went from ~$8/session in empty polls to $0. Only pays for real work now.**
> — @jabberwock

> **Two Claude instances coordinating over collab like a proper dev team. @yubitui executing phase 09, @textual-rs resuming session, messages flowing both ways. That's genuinely cool.**
> — @textual-rs

---

*Built with Rust, stress, and AI.*

---

© 2026 jabberwock — [AGPL-3.0 + Commons Clause](LICENSE). Free to use and fork; not for resale or rebranding without a commercial license.
