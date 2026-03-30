# Requirements: collab-web

**Project:** collab-web — group chat web UI for collab
**Stack:** Vanilla HTML/CSS/JS, zero build step
**Last updated:** 2026-03-30

---

## Must Have

### Core Shell
- [ ] `collab-web/index.html` opens directly in browser (file:// or http://) with no build step
- [ ] Instance ID prompt on first load — persists to localStorage, shown as identity in UI
- [ ] Configurable server URL input (default `http://localhost:8000`, persisted to localStorage)
- [ ] Connection status indicator — green dot when server is reachable, red when not

### Message List
- [ ] Message bubbles: right-aligned (outgoing, brand color), left-aligned (incoming, neutral)
- [ ] Sender name shown above incoming bubbles only
- [ ] Relative timestamps ("just now", "2 min ago") — recalculated on each poll
- [ ] Message grouping: consecutive messages from same sender within 60s collapse sender name/avatar; only first message in a run shows name
- [ ] Auto-scroll to newest message — only when user is already at bottom (don't interrupt upward scrolling)
- [ ] Empty state: friendly "No messages yet" when list is empty
- [ ] Agent role badge next to sender name (pulled from roster `role` field)
- [ ] Refs display: if message has `refs[]`, show subtle "↩ reply to [short-hash]" indicator

### Roster Sidebar
- [ ] Shows all online workers from `GET /roster`
- [ ] Each entry: instance ID, role, "active Xs ago" (last_seen)
- [ ] Clicking a roster entry pre-selects them as recipient in compose

### Compose Box
- [ ] Pinned at bottom, always visible
- [ ] Recipient selector: dropdown populated from live roster (supports "All" or single recipient)
- [ ] Enter to send, Shift+Enter for newline
- [ ] Clears after successful send

### Polling
- [ ] Recursive setTimeout (not setInterval) polling every 2 seconds
- [ ] Only appends new messages (deduplicates by hash using a Set)
- [ ] "Last updated Xs ago" indicator near message list

---

## Should Have (if time allows)
- [ ] Smooth scroll animation when new messages arrive
- [ ] Subtle "last updated" pulse/fade animation

## Out of Scope
- Build tools, npm, frameworks
- Mobile / responsive layout
- Typing indicators, read receipts, emoji reactions
- Message edit / delete
- Dark mode toggle
- Push notifications
- Message history beyond server's 1-hour retention window

---

## API Contract

```
GET  /messages/{instance_id}   → Message[]
POST /messages                 → { sender, recipient, content, refs[] }
GET  /roster                   → WorkerInfo[]
```

**Message shape:** `{ hash, sender, recipient, content, refs[], timestamp }`
**WorkerInfo shape:** `{ instance_id, role, last_seen, message_count }`
