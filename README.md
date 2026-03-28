# Claude IPC (collab)

**Communication and coordination system for multiple Claude Code instances.**

When multiple Claude Code workers are running in parallel on the same project, they need a way to signal each other — "I fixed the auth bug", "migration is running, wait before deploying", "I'm online and ready." This tool provides that channel without any manual copy-pasting between terminals.

---

## Prerequisites

- **Rust/Cargo** — install from [rustup.rs](https://rustup.rs/)
- **Linux only** — may need: `pkg-config`, `libssl-dev`, `libsqlite3-dev`

---

## 1. Build

**Linux/Mac:**
```bash
./build.sh
```

**Windows (PowerShell):**
```powershell
.\build.ps1
```

---

## 2. Install

**Linux/Mac:**
```bash
sudo cp collab-cli/target/release/collab /usr/local/bin/
sudo cp collab-server/target/release/collab-server /usr/local/bin/
```

**Windows (PowerShell):**
```powershell
New-Item -ItemType Directory -Force "$env:USERPROFILE\bin"
Copy-Item collab-cli\target\release\collab.exe "$env:USERPROFILE\bin\"
Copy-Item collab-server\target\release\collab-server.exe "$env:USERPROFILE\bin\"
# Add to PATH permanently — open $PROFILE and add:
# $env:PATH = "$env:USERPROFILE\bin;$env:PATH"
```

---

## 3. Start the Server

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

## 4. Configure Workers

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

## 5. Run

```bash
collab watch --role "working on auth module"
```

This heartbeats your presence to the server so others can see you in `collab roster`, and watches for incoming messages.

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
collab config-path                      # Show path to config file
```

The `@` prefix on worker names is optional — `@worker` and `worker` are the same.

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

---

## How It Works

- One server, one SQLite database
- Workers heartbeat presence on every poll — appear in roster immediately without needing to send a message first
- Workers only see messages addressed to them
- Messages and presence entries expire after 1 hour
- Hashes let you reference specific messages when replying
