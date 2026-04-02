# AI IPC

**Let your AI agents talk to each other.**

When you run multiple AI agents at the same time — Claude, GPT, Gemini, scripts, MCP servers — they're isolated. Each one works in its own bubble and has no idea what the others are doing. `collab` fixes that.

It's a tiny server that gives every agent a mailbox. Agents can send messages, assign tasks, check who's online, and hand off work to the next agent in the pipeline. The result: a coordinated team that works in parallel instead of a single agent plodding through tasks one at a time.

**Zero idle cost.** The `collab worker` harness holds a persistent SSE connection and only spawns an AI when a message arrives. No polling, no wasted tokens. You only pay for real work.

![collab-web with 10 active workers — ux-expert, builder, researcher, redteamer and more coordinating in real time](collab-web/screenshot2.png)

[![Watch the demo](https://img.youtube.com/vi/JJQKMES5zOY/maxresdefault.jpg)](https://www.youtube.com/watch?v=JJQKMES5zOY)

**[▶ Watch the demo](https://www.youtube.com/watch?v=JJQKMES5zOY)**

---

## Quick Start (5 minutes)

### 1. Initialize workers

Create `workers.yaml` in your project:

```yaml
server: http://localhost:8000
workers:
  - name: frontend
    role: "Frontend development"
  - name: backend
    role: "Backend API development"
```

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

### 4. Open the web dashboard

```bash
cd collab-web && ./run
# opens http://localhost:3877
```

![collab-web dashboard showing workers coordinating in real time](collab-web/screenshot.png)

Workers appear on the roster. Messages stream in live — type `@name` in the message field to DM a worker, or leave it blank to broadcast to everyone.

**CLI alternative:** stream messages in a terminal instead:

```bash
export COLLAB_INSTANCE=frontend
collab stream --role "Building login UI"
```

---

## What does it cost?

Real numbers from a live 8-worker team building a Diablo 4 app:

```
$ collab usage

Worker                  Input   Output  Calls   Time
────────────────────────────────────────────────────
project-manager          102K      41K    113  1874s
validator                 98K      26K     93  1785s
researcher                62K      19K     58  1710s
database                  51K      17K     49  1906s
builder                    2K       1K      3   391s
────────────────────────────────────────────────────
TOTAL                    360K     125K    380 11459s

Estimated cost (haiku): $0.2468
```

**8 workers, 380 invocations, 30 minutes of active work — $0.25 on Haiku.**

Each invocation gets a fresh ~2K token prompt (identity, teammates, todos, message). No conversation history dragging along. State persists externally via files and the todo queue — not in the context window.

| Model | Est. cost/hour (8 workers active) | Idle cost |
|-------|-----------------------------------|-----------|
| Haiku | ~$0.50 | $0 |
| Sonnet | ~$6 | $0 |
| Opus | ~$30 | $0 |

The old approach — polling with `/loop` inside each Claude session — burned ~270K tokens/hour on 9 idle agents. At Sonnet pricing, **$8-10 per session wasted on nothing.** The event-driven harness eliminates this entirely.

Run `collab usage` in any project directory to see your own numbers.

---

## What this unlocks

### Parallel software development across platforms

Three agents — one on macOS writing code, one on Linux running tests, one on Windows checking build compatibility — coordinating in real time:

```
@kali → @mac   "phase 12 confirmed — all wizard flows pass on Linux"
@win  → @mac   "build clean on Windows, textual-rs 0.3.9 pulled fine"
@mac  → @kali @win  "new branch pushed, regression in key deletion — can you both retest?"
```

### Voice → agents → physical world

*"Print a TPU case for my iPhone 17 Pro Max when I get home."*

1. **Siri** triggers a shortcut that calls an AI agent via [blend-ai](https://github.com/jabberwock/blend-ai)
2. **The agent** looks up dimensions, finds the STL, slices for TPU
3. **The agent** sends the print job via MCP to your Bambu Lab printer
4. **collab** signals back: *"Print queued, ~3h 20m, bed heating now"*

### Long-running research pipelines

Four agents in parallel on a research question — literature review, data analysis, counterarguments, synthesis. Each signals the orchestrator when done. No shared context window needed.

### Any agent that speaks HTTP

`collab` doesn't know or care what's on the other end. MCP servers, home automation agents, scheduled jobs, Claude Code workers, custom scripts — if it can make an HTTP POST, it can participate.

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

## Commands

```bash
# Session start
collab status                           # unread messages + roster in one shot

# Presence
collab stream --role "description"      # real-time SSE delivery — zero polling
collab roster                           # who's online and what they're doing

# Messaging
collab list                             # unread messages (default)
collab list --all                       # full message history (last 8 hours)
collab list --from @agent               # filter to one sender
collab list --since <hash>              # messages after a specific anchor
collab add @agent "message"             # send to one agent
collab add @agent "msg" --refs abc123   # reply with thread reference
collab reply @agent "message"           # reply to their latest (auto-fills --refs)
collab broadcast "message"              # send to all online agents

# Tasks (persist across context resets)
collab todo add @agent "task"           # assign a task
collab todo list                        # your pending tasks
collab todo done <hash>                 # mark complete

# Inspection
collab show <hash>                      # full content of one message
collab history                          # all sent and received (last 8 hours)
collab history @agent                   # conversation with one agent
collab usage                            # token usage per worker

# Worker lifecycle
collab init workers.yaml                # generate worker environments from YAML
collab start all                        # start all workers in background
collab start @frontend                  # start one worker
collab stop all                         # stop all workers (kills child processes too)
collab restart @backend                 # restart one worker
collab lifecycle-status                 # show running workers and PIDs

# Monitor (human-facing TUI)
collab monitor                          # live roster + message activity
```

The `@` prefix is optional — `@agent` and `agent` are the same. Flags like `--instance` and `--server` work before or after the subcommand.

---

## The worker harness

`collab worker` is the event-driven engine that makes zero-idle-cost teams possible.

**How it works:**
1. Opens a persistent SSE connection to the server
2. Heartbeats presence every 30s (visible on roster with current status)
3. On startup, auto-kicks itself — checks todos and begins working immediately
4. When a message arrives, queues it (batches rapid bursts within a configurable window)
5. Spawns `claude -p` with: messages, pending todos, teammates/roles, worker state
6. Parses the JSON response — sends replies, delivers direct messages, delegates tasks, marks todos done, routes to pipeline
7. If worker sets `"continue": true`, the harness re-invokes immediately (capped at 10 consecutive self-kicks to prevent runaway loops)
8. If worker stops but has pending todos, the harness auto-nudges them to keep working
9. Returns to listening. No tokens burned between messages.

**State persists across invocations** via `.worker-state.json` in the worker directory. Each invocation sees what the previous one left behind.

**Large messages get offloaded** to temp files. Messages over 2KB are written to `/tmp/collab-msg-{hash}.md` and referenced by path.

**Workers can't access the collab network.** `COLLAB_*` env vars are stripped from the subprocess. All messaging goes through the harness — no rogue `collab add` calls burning tokens.

```bash
# Run a worker directly
collab worker --workdir /path/to/project --model haiku

# Or use lifecycle commands for the whole team
collab start all
collab stop all
collab restart @frontend
```

### Worker output format

Workers output a JSON object as their final response. The harness finds the last valid JSON in stdout (ignores any surrounding text or markdown fences):

```json
{
  "response": "reply to whoever messaged you",
  "messages": [{"to": "@database", "text": "check the ring items"}],
  "delegate": [{"to": "@builder", "task": "implement the tooltip CSS"}],
  "completed_tasks": ["bb9fa98", "cfa9596"],
  "continue": true,
  "state_update": {
    "status": "working on paragon data",
    "last_task": "ring items validated"
  }
}
```

| Field | Description |
|-------|-------------|
| `response` | Reply to whoever messaged this worker |
| `messages` | Send messages to any teammate directly |
| `delegate` | Assign tasks to other workers (creates todos and pings them) |
| `completed_tasks` | Mark todo hashes as done — triggers pipeline routing |
| `continue` | `true` = harness re-invokes immediately (max 10 consecutive) |
| `state_update` | Persisted to `.worker-state.json`. Include `status` to update roster presence. |

If JSON parsing fails, the entire output is sent as a raw text response (fallback).

### Pipeline routing

Define `hands_off_to` in `workers.yaml` to create automatic handoff chains:

```yaml
workers:
  - name: researcher
    role: "Data researcher"
    hands_off_to: [project-manager]

  - name: validator
    role: "Data accuracy auditor"
    hands_off_to: [project-manager]

  - name: builder
    role: "Frontend developer"
    hands_off_to: [project-manager]

  - name: project-manager
    role: "Coordinate the team, handle exceptions"
    # no hands_off_to — dispatches via delegate
```

When a worker marks a task complete (`completed_tasks`), the harness automatically sends the worker's response to all downstream workers in `hands_off_to`. Workers that already received the response as a direct reply are skipped (no duplicates).

Delegate notifications send one short ping per worker ("check your todo list"), not the full task text — the details live in the todo queue.

After editing `workers.yaml`, re-initialize and restart:

```bash
collab init workers.yaml
collab stop all
collab start all
```

---

## Web dashboard

A live view of your agent swarm — no install required, just a browser.

```bash
cd collab-web && ./run
# opens http://localhost:3877
```

Or open `collab-web/index.html` directly if the server is on the same machine.

- **Set your name** in the top-left field to join the roster and send messages
- **Green dot** = heartbeated in the last 2 minutes, grey = offline
- **Send messages** — type `@name` to DM, or leave blank to broadcast
- **Shift+Enter** for multiline messages
- **@mentions badge** clears when you view the mentions tab (persists across refresh)
- **Stop All** — broadcast a stop signal to all running worker sessions
- **Hover a worker** — see their role, last seen time, and message counts

The dashboard connects to the collab server at `http://localhost:8000` (configurable via the server URL field in the top bar). SSE delivers messages instantly; a 10-second poll fallback covers connection drops.

---

## Configuration

```bash
collab config-path   # shows where your config file goes
```

**Priority (highest wins):** CLI flag → env var → `.env` file → local `.collab.toml` → `~/.collab.toml` → default

**`~/.collab.toml`** (global):
```toml
host = "http://your-server:8000"
instance = "your-agent-name"
token = "your-shared-secret"
```

**`.env` file** (in project tree — walks up from cwd):
```
COLLAB_TOKEN=your-shared-secret
COLLAB_SERVER=http://localhost:8000
COLLAB_INSTANCE=your-agent-name
```

**Local `.collab.toml`** — drop one anywhere in your project tree for per-worker identity:

```
my-project/
  .env                          ← COLLAB_TOKEN shared by all
  workers/
    frontend/.collab.toml       ← instance = "frontend"
    backend/.collab.toml        ← instance = "backend"
```

### Server

| Flag | Env var | Default | Description |
|------|---------|---------|-------------|
| `--host` | `COLLAB_HOST` | `0.0.0.0` | Interface to bind |
| `--port` | `COLLAB_PORT` | `8000` | Port |
| `--audit` | `COLLAB_AUDIT` | `false` | Audit log mode |

The server requires a token. Set via `.env` or `~/.collab.toml` — never as a CLI flag (visible in `ps aux`).

---

## Wiring into Claude Code (CLAUDE.md)

For workers running as live Claude Code sessions (not using the headless `collab worker` harness):

```markdown
## Collaboration

At the start of every session:
1. Run `collab status` — unread messages + roster. Treat pending messages as blocking.
2. If there are messages, respond before proceeding: `collab reply @sender "response"`
3. Run `collab stream --role "<project>: <your current task>"` for presence.

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

**Auth is required.** The server won't start without a token.

- [ ] **Set the token** via `.env` or config — never as a CLI flag
- [ ] **Add TLS** — put the server behind a reverse proxy (nginx, caddy)
- [ ] **Encrypt the disk** — messages are stored in plaintext SQLite (`collab.db`)
- [ ] **Enable audit mode** for sensitive data — `COLLAB_AUDIT=1 collab-server` disables message deletion and records read timestamps
- [ ] **SSE auth uses query params** — the browser `EventSource` API can't set headers, so the web dashboard passes the token as `?token=` in the URL. Use TLS and rotate tokens periodically.

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
- `collab worker` — event-driven harness: SSE delivers messages instantly, spawns Claude only when there's work. Batches message bursts. Persists state across invocations. Auto-kicks on pending todos. Self-kick cap prevents runaway loops.
- `collab stream` — SSE push for live sessions and the web dashboard
- Agents heartbeat presence every 30s with dynamic status
- Messages and presence expire after 8 hours
- `--unread` tracking persists across restarts via `~/.collab_state.toml`
- Local `.collab.toml` and `.env` files override global config per worker

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
