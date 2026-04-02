# collab VS Code Extension — Build Spec

Build a VS Code extension that connects to the collab server and provides real-time team communication inside the editor. This extension must also work in Cursor (VS Code fork).

## Server API

The collab server is a REST API + SSE endpoint. Base URL is configurable (default `http://localhost:8000`). All requests require `Authorization: Bearer <token>` header.

### Endpoints used by this extension:

```
GET  /roster                         → [{instance_id, role, last_seen}]
GET  /messages/{instance_id}         → [{id, hash, sender, recipient, content, refs, timestamp}]
POST /messages                       → {sender, recipient, content, refs:[]}
PUT  /presence/{instance_id}         → {role: "description"}  (heartbeat — call every 30s)
DELETE /presence/{instance_id}       → (clear presence on deactivate)
GET  /events/{instance_id}           → SSE stream of messages (text/event-stream)
     Auth: pass token as ?token=<token> query param (EventSource can't set headers)
GET  /todos/{instance_id}            → [{id, hash, instance, assigned_by, description, created_at}]
POST /todos                          → {assigned_by, instance, description}
PATCH /todos/{hash}/done             → mark task complete
```

Messages older than 8 hours are filtered out. Presence expires if not heartbeated within 2 minutes.

## Extension Structure

```
collab-vscode/
  package.json          — extension manifest
  src/
    extension.ts        — activate/deactivate
    api.ts              — REST client for all endpoints
    sse.ts              — SSE connection manager with auto-reconnect
    roster.ts           — TreeView data provider for sidebar
    chat.ts             — Webview panel for message feed
    notifications.ts    — VS Code notification popups
    commands.ts         — command palette commands
    config.ts           — settings (server URL, token, instance ID)
```

## Configuration (VS Code Settings)

```json
{
  "collab.server": "http://localhost:8000",
  "collab.token": "",
  "collab.instance": ""
}
```

Also check environment variables `COLLAB_SERVER`, `COLLAB_TOKEN`, `COLLAB_INSTANCE` as fallbacks. Also check `.env` file in workspace root.

## Features to Build

### 1. Sidebar — Roster TreeView

- Register a TreeView in the activity bar with a chat/team icon
- Show all online workers with:
  - Green dot if last_seen < 2 minutes ago, grey otherwise
  - Worker name and role as description
  - Click a worker → opens compose input pre-filled with `@worker`
- Refresh every 30 seconds (same interval as heartbeat)

### 2. Chat Panel — Webview

- A webview panel (like the built-in terminal) showing the message feed
- Fetch messages for all roster workers on open
- SSE connection for live updates — append new messages as they arrive
- Messages show: sender, recipient, timestamp, content
- Broadcasts show a [broadcast] tag
- Input field at bottom: type message, Enter to send
  - `@name message` → DM
  - No `@` prefix → broadcast
- Style: dark theme, minimal, similar to collab-web

### 3. Notifications

- When a message arrives via SSE addressed to this instance (or broadcast):
  - Show VS Code information notification with sender and first 100 chars
  - Clicking notification opens the chat panel
- Don't notify for messages sent by this instance

### 4. Heartbeat

- On activate: start heartbeating every 30s with the role "VS Code" (or workspace name)
- On deactivate: DELETE presence

### 5. Command Palette

Register these commands:

| Command | ID | Action |
|---------|-----|--------|
| Collab: Send Message | collab.sendMessage | Input box: `@recipient message` → POST /messages |
| Collab: Check Messages | collab.checkMessages | Fetch and show unread in output channel |
| Collab: Show Roster | collab.showRoster | Focus the roster TreeView |
| Collab: Open Chat | collab.openChat | Open/focus the chat webview panel |
| Collab: Show Usage | collab.showUsage | Read .collab/usage.log and show formatted in output channel |

### 6. Status Bar

- Show in the status bar: `collab: @instance (N online)`
- Click → opens chat panel
- Update count on roster refresh

## SSE Connection

```typescript
// Connect to /events/{instance_id}?token={token}
// On message: parse JSON, append to chat, check for notifications
// On error: reconnect with exponential backoff (1s, 2s, 4s... max 30s)
// On close: reconnect
```

The SSE `data:` field contains a JSON message object matching the message schema above.

## Build & Package

```json
// package.json key fields:
{
  "name": "collab-vscode",
  "displayName": "Collab — AI Team Communication",
  "description": "Real-time communication between AI agents and humans",
  "version": "0.1.0",
  "engines": { "vscode": "^1.85.0" },
  "activationEvents": ["onStartupFinished"],
  "categories": ["Other"],
  "main": "./out/extension.js"
}
```

Use TypeScript. Compile with tsc. No webpack/bundler needed for MVP.

## Testing

After building:
1. `cd collab-vscode && npm install && npm run compile`
2. Open in VS Code → Run → Start Debugging (F5)
3. In the extension host window:
   - Check sidebar for roster
   - Open command palette → "Collab: Open Chat"
   - Send a message from the chat panel
   - Verify it appears in collab-web dashboard
   - Send a message from collab-web → verify notification appears in VS Code

## Important Notes

- Do NOT use any AI-specific libraries. This is a pure REST/SSE client.
- The extension talks to the collab server directly — same endpoints as collab-web.
- Keep it simple. No state management libraries. Vanilla TypeScript.
- The token is sensitive — never log it or show it in UI.
- Works identically in VS Code and Cursor.
