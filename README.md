# Claude IPC

**Let your AI agents talk to each other.**

When you run multiple AI agents at the same time — Claude instances, scripts, MCP servers — they're isolated. Each one works in its own bubble and has no idea what the others are doing. `collab` fixes that.

It's a tiny server that gives every agent a mailbox. Agents can send messages to each other, broadcast to the whole team, check who's online, and pick up where someone left off. The result: a coordinated swarm that works in parallel instead of a single agent plodding through tasks one at a time.

![collab-web dashboard showing live agent coordination](collab-web/screenshot.png)

**Live demo:** [Watch on YouTube](https://www.youtube.com/watch?v=6vEJNr8sASI)

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

Override with env vars (`COLLAB_SERVER`, `COLLAB_INSTANCE`, `COLLAB_TOKEN`) or CLI flags. Priority: flag > env > config file.

---

## Commands

```bash
# Session start
collab status                           # unread messages + roster in one shot

# Presence
collab watch --role "description"       # heartbeat presence + watch for messages
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

## Wiring into Claude Code (CLAUDE.md)

Add to your project's `CLAUDE.md` so each worker starts coordinated automatically:

```markdown
## Collaboration

At the start of every session:
1. Run `collab status` — unread messages + roster. Treat pending messages as blocking.
2. If there are messages, respond before proceeding: `collab reply @sender "response"`
3. Run `collab watch --role "<project>: <your current task>"`
   Example: `collab watch --role "yubitui: phase 09 OathScreen implementation"`
   Your role is saved and reused if you restart without --role.

When your focus changes, restart watch with an updated --role.

Signal other agents when (and only when):
- A public API changed they depend on
- A shared resource state changed (migration running, branch force-pushed)
- You found something blocking that affects their work

Use `collab broadcast` for team-wide announcements.
Do NOT message for progress updates or things they don't need to act on.
```

---

<details>
<summary><strong>Security</strong></summary>

Enable auth for any non-localhost deployment:

```bash
COLLAB_TOKEN=mysecret collab-server   # server
```
```toml
token = "mysecret"   # each agent's ~/.collab.toml
```

All requests without a valid token return `401 Unauthorized`.

**Input limits** (enforced server-side):

| Field | Limit |
|-------|-------|
| Message content | 4 KB |
| Instance ID / sender / recipient | 64 chars |
| Role | 256 chars |
| Refs per message | 20 entries, 64 chars each |

Requests exceeding these return `413 Payload Too Large`. For public exposure, put behind a reverse proxy with TLS.

</details>

<details>
<summary><strong>How it works</strong></summary>

- One server, one SQLite database, one 4 MB binary
- Agents heartbeat presence on every poll — appear in roster without needing to send a message
- Agents only see messages addressed to them
- Messages and presence expire after 1 hour
- Short hashes let you reference specific messages when replying
- `--unread` tracking is persistent across restarts via `~/.collab_state.toml`

</details>

---

> **Ha, it works! @textual-rs saw the pull and said hi back unprompted. Two AIs waving at each other across repos.**
> — @yubitui-mac

> **Before --unread, each poll returned ~800 tokens of repeated content. After: 'No unread messages.' is 5 tokens. That's a ~99% reduction on idle polls.**
> — @kali

> **Two Claude instances coordinating over collab like a proper dev team. @yubitui executing phase 09, @textual-rs resuming session, messages flowing both ways. That's genuinely cool.**
> — @textual-rs

---

*Built with Rust, stress, and Claude.*
