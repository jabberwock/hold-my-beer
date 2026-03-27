# Claude IPC

**Communication system for multiple Claude Code instances working together.**

---

## Install

```bash
# Build
cd collab-cli && cargo build --release
```

**Linux/Mac:**
```bash
mkdir -p ~/bin
cp target/release/collab ~/bin/
```

**Windows:**
```powershell
mkdir -Force "$env:USERPROFILE\bin"
copy target\release\collab.exe "$env:USERPROFILE\bin\"
```

Add to `~/.zshrc` or `~/.bashrc` (Linux/Mac):
```bash
export PATH="$HOME/bin:$PATH"
export COLLAB_SERVER=http://kali:8000
```

Or `$PROFILE` (Windows PowerShell):
```powershell
$env:PATH = "$env:USERPROFILE\bin;$env:PATH"
$env:COLLAB_SERVER = "http://kali:8000"
```

---

## Server

**Start once:**
```bash
cd collab-server && cargo run --release
```

The server creates `collab.db` and listens on port 8000. All workers connect to this one server.

---

## Worker Setup

Each worker needs to know its own instance ID.

**Add to the worker's prompt file** (`.gsd/prompt.md` or `CLAUDE.md`):

```markdown
At session start, run:
bg_shell start collab --instance worker-name-here watch
```

Replace `worker-name-here` with:
- `yubitui`
- `MBPC`
- `worker-frontend`
- etc.

**That's it.** The worker will now see messages sent to it.

---

## Send Messages

```bash
collab add @other-worker "Your message here"
```

The other worker sees it in their watch output.

---

## Commands

```bash
collab roster                          # Who's active
collab list                            # Check messages once
collab watch                           # Watch continuously (auto-started)
collab add @worker "msg"               # Send message
collab add @worker "msg" --refs abc123 # Reference another message
collab history                         # See all messages
```

---

## How It Works

- One server, one database
- Each worker is a CLI client
- Workers only see messages TO them
- Messages expire after 1 hour
- SHA1 hashes for threading conversations

---

## Example

**Worker A (MBPC):**
```bash
collab add @yubitui "Fixed auth bug in login.rs"
```

**Worker B (yubitui) sees:**
```
🔔 New message!
Hash: f3b0577
From: @MBPC
Fixed auth bug in login.rs
```

**Worker B responds:**
```bash
collab add @MBPC "Confirmed - tests passing" --refs f3b0577
```

Done.
