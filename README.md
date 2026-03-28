# Claude IPC (collab)

**Communication and coordination system for multiple Claude Code instances.**

When multiple Claude Code workers are running in parallel on the same project, they need a way to signal each other — "I fixed the auth bug", "migration is running, wait before deploying", "I'm online and ready." This tool provides that channel without any manual copy-pasting between terminals.

---

<details>
<summary><strong>Prerequisites</strong></summary>

- **Rust/Cargo** — install from [rustup.rs](https://rustup.rs/)
- **Linux only** — may need: `pkg-config`, `libssl-dev`, `libsqlite3-dev`

</details>

---

## 1. Install

**Linux/Mac:**
```bash
./build.sh
```

**Windows (PowerShell):**
```powershell
.\build.ps1
```

Both scripts use `cargo install` which builds and puts `collab` and `collab-server` directly on your PATH. No manual copying.

---

## 2. Start the Server

Run once on a shared machine all workers can reach:

**Linux/Mac:**
```bash
collab-server
```

**Windows:**
```powershell
collab-server.exe
```

Listens on port 8000. Creates `collab.db` in the current directory — run it from a consistent location so history persists.

---

## 3. Configure Workers

Find where your config file goes:
```bash
collab config-path
```

Create that file (e.g. `~/.collab.toml` or `C:\Users\<you>\.collab.toml`):

```toml
host = "http://your-server:8000"
instance = "your-worker-name"
recipients = ["other-worker-1", "other-worker-2"]
```

- **host** — address of the collab server
- **instance** — your worker's unique name
- **recipients** — workers you expect to collaborate with; `watch` notifies you when they come online

You can also override with env vars (`COLLAB_SERVER`, `COLLAB_INSTANCE`) or CLI flags (`--server`, `--instance`). Priority: CLI flag > env var > config file.

---

## 4. Run

```bash
collab watch --role "working on auth module"
```

This heartbeats your presence to the server so others can see you in `collab roster`, and watches for incoming messages.

---

## Monitor

`collab monitor` opens a live TUI showing all online workers, their roles, last-seen times, and recent message activity:

```
collab monitor
```

![collab monitor screenshot](https://i.postimg.cc/4KSLGZy0/collab-monitor.png)

The roster updates every 2 seconds. Press `q` to quit.

```bash
collab monitor --interval 5   # slower refresh
```

---

## Commands

```bash
collab roster                           # Who's online and what they're working on
collab watch --role "description"       # Watch for messages + heartbeat presence
collab list                             # Check messages once
collab add @worker "message"            # Send a message
collab add @worker "msg" --refs abc123  # Reply referencing a previous message hash
collab history                          # All sent and received messages
collab history @worker                  # Conversation with a specific worker
collab monitor                          # Live TUI roster + message activity
collab config-path                      # Show path to config file
```

The `@` prefix on worker names is optional — `@worker` and `worker` are the same.

---

<details>
<summary><strong>Wiring into Claude Code (CLAUDE.md)</strong></summary>

Add this to your project's `CLAUDE.md` so each Claude Code worker starts watching automatically:

```markdown
## Collaboration

At the start of every session:
1. Check your current phase and task from the project context (ROADMAP.md, active PLAN.md, or recent git log)
2. Run `collab watch --role "<project>: <current phase/task>"` — use real context, not a generic description
   Example: `collab watch --role "yubitui: phase 09 OathScreen widget implementation"`
3. Run `collab roster` to see who else is online and what they're doing
4. When your focus changes, restart watch with an updated --role reflecting the new task
5. Before starting any new task, run `collab list` to check for pending messages
6. If there are messages, respond before proceeding: `collab add @sender "response" --refs <hash>`
7. If you make a change that affects shared interfaces, APIs, or files another worker depends on,
   notify them immediately: `collab add @other-worker "changed X in file Y — you may need to update Z"`
```

Each worker's `~/.collab.toml` should already have their `instance` name and `recipients` configured — Claude Code will pick that up automatically.

</details>

---

<details>
<summary><strong>Example</strong></summary>

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

</details>

---

<details>
<summary><strong>How It Works</strong></summary>

- One server, one SQLite database
- Workers heartbeat presence on every poll — appear in roster immediately without needing to send a message first
- Workers only see messages addressed to them
- Messages and presence entries expire after 1 hour
- Hashes let you reference specific messages when replying

</details>
