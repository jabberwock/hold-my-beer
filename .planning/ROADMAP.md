# Roadmap: collab-web

**Project:** collab-web — group chat web UI for collab
**Strategy:** Single phase, ship a working demo
**Last updated:** 2026-03-30

---

## Phase 1: collab-web group chat UI

**Goal:** A working, demo-ready group chat web app at `collab-web/index.html` that connects to the collab-server REST API.

**Delivers:**
- Zero build step — open index.html in any browser
- Group chat layout (bubbles, sender names, timestamps, grouping)
- Roster sidebar with live worker list
- Compose box with recipient selector
- 2-second auto-poll with deduplication
- Connection status indicator
- Instance ID prompt on load

**Exit criteria:**
- [ ] Opens in Chrome/Firefox/Safari without errors
- [ ] Shows messages from `/messages/{instance_id}` as chat bubbles
- [ ] Sends messages via `POST /messages`
- [ ] Roster sidebar populated from `GET /roster`
- [ ] Message grouping works (consecutive same-sender messages collapse)
- [ ] Polling runs every 2s without stacking requests
- [ ] Looks demo-ready — someone unfamiliar with collab looks at it and immediately understands it's a group chat

**File structure (from stack research):**
```
collab-web/
  index.html
  css/
    tokens.css      # CSS custom properties (colors, spacing, fonts)
    layout.css      # page shell, sidebar, message area, compose zone
    components.css  # bubble, roster-item, compose-box, badge
  js/
    api.js          # fetch wrappers for all 3 endpoints
    poll.js         # recursive setTimeout polling + dedup Set
    render.js       # DOM: bubble factory, roster render, grouping logic
    app.js          # init: identity prompt, wiring, startup
```

**Plans:**
1. HTML/CSS shell — page structure, CSS tokens, layout skeleton, empty states
2. API + polling layer — api.js, poll.js, fetch wrappers, 2s loop, dedup
3. Message rendering — render.js, bubbles, grouping, timestamps, refs indicator
4. Roster + compose — sidebar render, recipient selector, send flow, role badges
5. Polish — connection indicator, auto-scroll, "last updated" display, empty states

---

## Phase Order

| Phase | Focus | Depends On |
|-------|-------|------------|
| 1 | Full collab-web app | Nothing (new package) |

---

*Single-phase project. After Phase 1: demo-ready.*
