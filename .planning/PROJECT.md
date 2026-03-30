# collab-web

## What This Is

A single-page web app that presents the collab messaging system as a group chat UI. Lives in `collab-web/` inside the existing `claude-ipc` repo. Zero build step — open `index.html` in any browser. Connects to the existing `collab-server` REST API without touching any existing code.

Primary purpose: **demo tool**. When someone looks at the TUI and goes "Huh!?", you open this instead.

## Core Value

Makes collab immediately understandable to non-technical audiences by presenting it as a familiar group chat interface (message bubbles, sender avatars/names, timestamps, compose box).

## Context

- **Repo**: `claude-ipc` (existing)
- **New package**: `collab-web/` — standalone, no dependencies on collab-cli or collab-server source
- **API**: Connects to existing `collab-server` REST API
  - `GET /messages/{instance_id}` — fetch messages for an instance
  - `POST /messages` — send a message
  - `GET /roster` — list online workers
- **Stack**: Vanilla HTML/CSS/JS — no build step, no npm, open directly in browser
- **Identity**: Prompt user for instance ID on first load
- **Polling**: Auto-refresh every 2 seconds

## Requirements

### Validated

(None yet — ship to validate)

### Active

- [ ] Single HTML file + CSS + JS, opens directly in browser without build step
- [ ] Identity prompt on load — asks for instance ID, persists to localStorage
- [ ] Group chat layout — message bubbles, sender name, timestamp per message
- [ ] Outgoing vs incoming messages visually distinct (alignment, color)
- [ ] Compose box at bottom — type and send to any online worker or all
- [ ] Recipient selector — choose who to send to from the online roster
- [ ] Auto-poll every 2 seconds for new messages
- [ ] Online roster sidebar showing who's currently active
- [ ] Configurable server URL (defaults to localhost:8000, editable in UI)
- [ ] Looks good enough to demo — clean, modern, not embarrassing

### Out of Scope

- Build tooling (webpack, vite, etc.) — zero build step is a hard requirement
- Authentication / tokens — same as the existing CLI (trusted local network)
- Message threading / reply chains in the UI — nice to have, not MVP
- Persistent message history beyond what the server returns — server owns retention
- Mobile-optimized layout — desktop demo use case only

## Key Decisions

| Decision | Rationale | Outcome |
|----------|-----------|---------|
| Vanilla HTML/JS/CSS | Zero build step = instant demo, no npm install friction | Selected |
| Instance ID prompt on load | Simplest auth, matches how the CLI works | Selected |
| Auto-poll every 2s | Existing REST API, no server changes needed | Selected |
| Lives in collab-web/ sub-dir | Doesn't touch existing packages | Selected |

## Evolution

This document evolves at phase transitions and milestone boundaries.

**After each phase transition** (via `/gsd:transition`):
1. Requirements invalidated? → Move to Out of Scope with reason
2. Requirements validated? → Move to Validated with phase reference
3. New requirements emerged? → Add to Active

---
*Last updated: 2026-03-30 after initialization*
