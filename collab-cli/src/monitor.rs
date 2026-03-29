use anyhow::Result;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use ratatui::{
    buffer::Buffer,
    layout::Rect,
    style::{Color, Modifier, Style},
};
use std::cell::{Cell, RefCell};
use std::time::{Duration, Instant};
use textual_rs::{
    event::keybinding::KeyBinding,
    widget::{context::AppContext, EventPropagation, WidgetId},
    App, ModalScreen, Widget, WorkerResult,
};

use crate::client::{load_read_state, save_read_state, CollabClient, Message, WorkerInfo};

type FetchData = (Vec<WorkerInfo>, Vec<Message>);
type SendResult = Result<String, String>; // Ok(hash of first sent msg) or Err(message)

// ── CSS ───────────────────────────────────────────────────────────────────────
const CSS: &str = r#"
MonitorScreen {
    background: $background;
    color: $foreground;
}
MessageModal {
    background: $background;
    color: $foreground;
    height: 100%;
    width: 100%;
}
ComposeModal {
    height: 100%;
    width: 100%;
}
"#;

// ── Send helper (runs inside worker future) ───────────────────────────────────

async fn send_to_all(
    server: String,
    instance_id: String,
    token: Option<String>,
    recipients: Vec<String>,
    content: String,
    reply_hash: Option<String>,
) -> SendResult {
    let client = CollabClient::new(&server, &instance_id, token.as_deref());
    let refs = reply_hash.into_iter().collect::<Vec<_>>();
    let mut last_hash = String::new();
    for recipient in &recipients {
        match client.send_message_raw(recipient, &content, refs.clone()).await {
            Ok(msg) => last_hash = msg.hash,
            Err(e) => return Err(format!("Failed sending to @{}: {}", recipient, e)),
        }
    }
    Ok(last_hash)
}

// ── Least-capacitated helper ─────────────────────────────────────────────────

/// Returns the instance_id of the worker (not self) with the fewest messages.
fn least_capacitated<'a>(workers: &'a [WorkerInfo], self_id: &str) -> Option<&'a str> {
    workers.iter()
        .filter(|w| w.instance_id != self_id)
        .min_by_key(|w| w.message_count)
        .map(|w| w.instance_id.as_str())
}

// ── Entry point ───────────────────────────────────────────────────────────────

pub fn run(server: &str, instance_id: &str, interval_secs: u64, token: Option<&str>) -> Result<()> {
    let server = server.to_string();
    let instance_id = instance_id.to_string();
    let token = token.map(str::to_string);
    App::new(move || {
        Box::new(MonitorScreen::new(server, instance_id, interval_secs, token))
    })
    .with_css(CSS)
    .run()
}

// ── Fetch helper (runs inside worker future) ──────────────────────────────────

async fn fetch_data(server: String, instance_id: String, token: Option<String>) -> Result<FetchData, String> {
    let client = CollabClient::new(&server, &instance_id, token.as_deref());
    let (workers_r, messages_r) = tokio::join!(
        client.fetch_roster_pub(),
        client.fetch_history_pub(&instance_id),
    );
    match (workers_r, messages_r) {
        (Ok(mut workers), Ok(messages)) => {
            workers.sort_by(|a, b| a.instance_id.cmp(&b.instance_id));
            Ok((workers, messages))
        }
        (Err(e), _) => Err(e.to_string()),
        (_, Err(e)) => Err(e.to_string()),
    }
}

// ── MonitorScreen ─────────────────────────────────────────────────────────────

struct MonitorScreen {
    server: String,
    instance_id: String,
    interval_secs: u64,
    token: Option<String>,
    workers: RefCell<Vec<WorkerInfo>>,
    messages: RefCell<Vec<Message>>,
    /// Cursor in *display* order (0 = newest message).
    msg_cursor: Cell<usize>,
    /// Scroll offset in display order.
    msg_scroll: Cell<usize>,
    error: RefCell<Option<String>>,
    /// Transient status shown in footer (e.g. "No other workers online")
    status_msg: RefCell<Option<String>>,
    own_id: Cell<Option<WidgetId>>,
    /// Y of the first message data row; updated each render for click hit-testing.
    msg_data_start_y: Cell<u16>,
    /// When to next auto-fetch; None means fetch immediately on first render.
    next_fetch_at: Cell<Option<Instant>>,
    /// Tracks last click (time + display index) for double-click detection.
    last_click: RefCell<Option<(Instant, usize)>>,
}

impl MonitorScreen {
    fn new(server: String, instance_id: String, interval_secs: u64, token: Option<String>) -> Self {
        Self {
            server,
            instance_id,
            interval_secs,
            token,
            workers: RefCell::new(vec![]),
            messages: RefCell::new(vec![]),
            msg_cursor: Cell::new(0),
            msg_scroll: Cell::new(0),
            error: RefCell::new(None),
            status_msg: RefCell::new(None),
            own_id: Cell::new(None),
            msg_data_start_y: Cell::new(0),
            next_fetch_at: Cell::new(None),
            last_click: RefCell::new(None),
        }
    }

    fn spawn_fetch_now(&self, ctx: &AppContext) {
        let Some(id) = self.own_id.get() else { return };
        let server = self.server.clone();
        let instance_id = self.instance_id.clone();
        let token = self.token.clone();
        // Schedule next auto-fetch from now
        self.next_fetch_at
            .set(Some(Instant::now() + Duration::from_secs(self.interval_secs)));
        ctx.run_worker(id, async move {
            fetch_data(server, instance_id, token).await
        });
    }

    fn clamp_scroll(&self, viewport_rows: usize) {
        let cursor = self.msg_cursor.get();
        let scroll = self.msg_scroll.get();
        if cursor < scroll {
            self.msg_scroll.set(cursor);
        } else if viewport_rows > 0 && cursor >= scroll + viewport_rows {
            self.msg_scroll.set(cursor + 1 - viewport_rows);
        }
    }

    fn open_modal(&self, ctx: &AppContext) {
        let messages = self.messages.borrow();
        let len = messages.len();
        if len == 0 {
            return;
        }
        let cursor = self.msg_cursor.get();
        // cursor 0 = newest = messages[len-1]
        let vec_idx = len.saturating_sub(1 + cursor);
        if vec_idx >= len {
            return;
        }
        let msg = messages[vec_idx].clone();
        drop(messages);
        let dialog = MessageModal::new(msg, self.instance_id.clone());
        ctx.push_screen_deferred(Box::new(ModalScreen::new(Box::new(dialog))));
    }

    fn open_compose(&self, ctx: &AppContext, reply_hash: Option<String>, reply_to: Option<String>) {
        let workers = self.workers.borrow().clone();
        let others: Vec<WorkerInfo> = workers.into_iter()
            .filter(|w| w.instance_id != self.instance_id)
            .collect();
        if others.is_empty() {
            *self.status_msg.borrow_mut() = Some("No other workers online — press r to refresh".to_string());
            return;
        }
        *self.status_msg.borrow_mut() = None;
        // Pre-select all; if replying, only pre-select the reply target
        let selected: Vec<bool> = others.iter().map(|w| {
            match &reply_to {
                Some(id) => &w.instance_id == id,
                None => true,
            }
        }).collect();
        let modal = ComposeModal::new(
            self.server.clone(),
            self.instance_id.clone(),
            self.token.clone(),
            others,
            selected,
            reply_hash,
        );
        ctx.push_screen_deferred(Box::new(ModalScreen::new(Box::new(modal))));
    }

    fn open_reply(&self, ctx: &AppContext) {
        let messages = self.messages.borrow();
        let len = messages.len();
        if len == 0 { return; }
        let cursor = self.msg_cursor.get();
        let vec_idx = len.saturating_sub(1 + cursor);
        if vec_idx >= len { return; }
        let msg = messages[vec_idx].clone();
        drop(messages);
        // Only reply to incoming messages
        if msg.sender == self.instance_id { return; }
        self.open_compose(ctx, Some(msg.hash), Some(msg.sender));
    }
}

static MONITOR_BINDINGS: &[KeyBinding] = &[
    KeyBinding {
        key: KeyCode::Char('q'),
        modifiers: KeyModifiers::NONE,
        action: "quit",
        description: "Quit",
        show: true,
    },
    KeyBinding {
        key: KeyCode::Esc,
        modifiers: KeyModifiers::NONE,
        action: "quit",
        description: "Quit",
        show: false,
    },
    KeyBinding {
        key: KeyCode::Char('r'),
        modifiers: KeyModifiers::NONE,
        action: "refresh",
        description: "Refresh",
        show: true,
    },
    KeyBinding {
        key: KeyCode::Up,
        modifiers: KeyModifiers::NONE,
        action: "cursor_up",
        description: "Up",
        show: false,
    },
    KeyBinding {
        key: KeyCode::Char('k'),
        modifiers: KeyModifiers::NONE,
        action: "cursor_up",
        description: "Up",
        show: false,
    },
    KeyBinding {
        key: KeyCode::Down,
        modifiers: KeyModifiers::NONE,
        action: "cursor_down",
        description: "Down",
        show: false,
    },
    KeyBinding {
        key: KeyCode::Char('j'),
        modifiers: KeyModifiers::NONE,
        action: "cursor_down",
        description: "Down",
        show: false,
    },
    KeyBinding {
        key: KeyCode::Enter,
        modifiers: KeyModifiers::NONE,
        action: "view_message",
        description: "View",
        show: true,
    },
    KeyBinding {
        key: KeyCode::Char('n'),
        modifiers: KeyModifiers::NONE,
        action: "compose",
        description: "New message",
        show: true,
    },
    KeyBinding {
        key: KeyCode::Char('R'),
        modifiers: KeyModifiers::NONE,
        action: "reply",
        description: "Reply to selected",
        show: true,
    },
];

impl Widget for MonitorScreen {
    fn widget_type_name(&self) -> &'static str {
        "MonitorScreen"
    }

    fn can_focus(&self) -> bool {
        true
    }

    fn on_mount(&self, id: WidgetId) {
        self.own_id.set(Some(id));
    }

    fn on_unmount(&self, _: WidgetId) {
        self.own_id.set(None);
    }

    fn key_bindings(&self) -> &[KeyBinding] {
        MONITOR_BINDINGS
    }

    fn on_action(&self, action: &str, ctx: &AppContext) {
        match action {
            "quit" => ctx.quit(),
            "refresh" => self.spawn_fetch_now(ctx),
            "cursor_up" => {
                let cur = self.msg_cursor.get();
                if cur > 0 {
                    self.msg_cursor.set(cur - 1);
                }
            }
            "cursor_down" => {
                let len = self.messages.borrow().len();
                let cur = self.msg_cursor.get();
                if len > 0 && cur + 1 < len {
                    self.msg_cursor.set(cur + 1);
                }
            }
            "view_message" => self.open_modal(ctx),
            "compose" => self.open_compose(ctx, None, None),
            "reply" => self.open_reply(ctx),
            _ => {}
        }
    }

    fn on_event(&self, event: &dyn std::any::Any, ctx: &AppContext) -> EventPropagation {
        // Worker result
        if let Some(result) = event.downcast_ref::<WorkerResult<Result<FetchData, String>>>() {
            match &result.value {
                Ok((workers, messages)) => {
                    *self.workers.borrow_mut() = workers.clone();
                    *self.messages.borrow_mut() = messages.clone();
                    *self.error.borrow_mut() = None;
                    let len = messages.len();
                    let cursor = self.msg_cursor.get();
                    if len > 0 && cursor >= len {
                        self.msg_cursor.set(len - 1);
                    }
                }
                Err(e) => {
                    *self.error.borrow_mut() = Some(e.clone());
                }
            }
            return EventPropagation::Stop;
        }

        // Mouse click → move cursor to clicked row; double-click opens modal
        if let Some(m) = event.downcast_ref::<MouseEvent>() {
            if matches!(m.kind, MouseEventKind::Down(MouseButton::Left)) {
                let data_y = self.msg_data_start_y.get();
                if data_y > 0 && m.row >= data_y {
                    let display_idx = self.msg_scroll.get() + (m.row - data_y) as usize;
                    let len = self.messages.borrow().len();
                    if display_idx < len {
                        let now = Instant::now();
                        let is_double = self
                            .last_click
                            .borrow()
                            .as_ref()
                            .map(|(t, row)| *row == display_idx && now.duration_since(*t).as_millis() < 400)
                            .unwrap_or(false);
                        *self.last_click.borrow_mut() = Some((now, display_idx));
                        self.msg_cursor.set(display_idx);
                        if is_double {
                            self.open_modal(ctx);
                        }
                        return EventPropagation::Stop;
                    }
                }
            }
        }

        EventPropagation::Continue
    }

    fn render(&self, ctx: &AppContext, area: Rect, buf: &mut Buffer) {
        // Trigger fetch on first render (next_fetch_at = None) or when interval has elapsed.
        let should_fetch = self
            .next_fetch_at
            .get()
            .map(|t| Instant::now() >= t)
            .unwrap_or(true);
        if should_fetch {
            self.spawn_fetch_now(ctx);
        }
        if area.height < 4 {
            return;
        }

        let workers = self.workers.borrow();
        let messages = self.messages.borrow();
        let error = self.error.borrow();
        let cursor = self.msg_cursor.get();
        let w = area.width as usize;

        // ── Layout ────────────────────────────────────────────────────────────
        let header_y = area.y;
        let footer_y = area.y + area.height - 1;
        let content_start = area.y + 1;
        let content_h = area.height.saturating_sub(2);

        // Roster: title(1) + header(1) + sep(1) + data rows
        let roster_data_visible = (workers.len() as u16).min(content_h.saturating_sub(6).max(2));
        let roster_total = 3 + roster_data_visible;

        // Messages: remainder
        let msg_panel_y = content_start + roster_total;
        let msg_panel_h = content_h.saturating_sub(roster_total);
        let msg_data_rows = msg_panel_h.saturating_sub(3) as usize; // title+header+sep

        // ── Column widths ──────────────────────────────────────────────────────
        // Roster full row: "  " + Worker(18) + " │ " + Role(flex) + " │ " + LastSeen(10) + " │ " + Activity(8)
        //   fixed = 2 + 18 + 3 + 3 + 10 + 3 + 8 = 47
        const WORKER_W: usize = 18;
        const LAST_SEEN_W: usize = 10;
        const ACTIVITY_W: usize = 8;
        let roster_fixed = 2 + WORKER_W + 3 + 3 + LAST_SEEN_W + 3 + ACTIVITY_W;
        let role_w = w.saturating_sub(roster_fixed).max(8);

        // Messages full row: "  " + Direction(dir_w) + " │ " + Time(8) + " │ " + Content(flex)
        //   fixed = 2 + dir_w + 3 + 8 + 3 = dir_w + 16
        let max_name = messages
            .iter()
            .flat_map(|m| [m.sender.len(), m.recipient.len()])
            .max()
            .unwrap_or(0)
            .max(self.instance_id.len());
        let dir_w = (max_name + 8).max(16).min(30);
        let msg_fixed = dir_w + 16;
        let content_w = w.saturating_sub(msg_fixed).max(10);

        // Adjust scroll so cursor stays in view
        self.clamp_scroll(msg_data_rows);
        let scroll = self.msg_scroll.get();

        // ── Header ────────────────────────────────────────────────────────────
        let h_style = Style::default()
            .fg(Color::Black)
            .bg(Color::Cyan)
            .add_modifier(Modifier::BOLD);
        fill_line(buf, area.x, header_y, area.width, h_style);
        let header_text = format!(
            " collab monitor  @{}  {}",
            self.instance_id, self.server
        );
        buf.set_string(area.x, header_y, &clip(&header_text, w), h_style);

        // ── Roster panel ──────────────────────────────────────────────────────
        let dim = Style::default().fg(Color::DarkGray);
        let sep_style = Style::default().fg(Color::Rgb(60, 60, 90));

        // Title bar
        let r_title = format!("─ Roster ({} online) ", workers.len());
        let r_line = format!("{}{}", r_title, "─".repeat(w.saturating_sub(r_title.len())));
        buf.set_string(area.x, content_start, &clip(&r_line, w), dim);

        // Column headers
        let r_head = format!(
            "  {:<w0$} │ {:<w1$} │ {:<w2$} │ {:<w3$}",
            "Worker", "Role", "Last Seen", "Activity",
            w0 = WORKER_W, w1 = role_w, w2 = LAST_SEEN_W, w3 = ACTIVITY_W
        );
        buf.set_string(
            area.x,
            content_start + 1,
            &clip(&r_head, w),
            dim.add_modifier(Modifier::BOLD),
        );

        // Separator
        let r_sep = format!(
            "  {}─┼─{}─┼─{}─┼─{}",
            "─".repeat(WORKER_W),
            "─".repeat(role_w),
            "─".repeat(LAST_SEEN_W),
            "─".repeat(ACTIVITY_W),
        );
        buf.set_string(area.x, content_start + 2, &clip(&r_sep, w), sep_style);

        // Worker rows
        let lc = least_capacitated(&workers, &self.instance_id);
        for (i, worker) in workers.iter().enumerate().take(roster_data_visible as usize) {
            let y = content_start + 3 + i as u16;
            let you = if worker.instance_id == self.instance_id { " ◀" } else { "" };
            let star = if Some(worker.instance_id.as_str()) == lc { " ★" } else { "" };
            let name = format!("@{}{}{}", worker.instance_id, star, you);
            let role = if worker.role.is_empty() {
                "—".to_string()
            } else {
                worker.role.clone()
            };
            let age = age_str(worker.last_seen);
            let activity = if worker.message_count > 0 {
                format!("{} msgs", worker.message_count)
            } else {
                String::new()
            };

            let name_style = if worker.instance_id == self.instance_id {
                Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(Color::Green)
            };

            let mut cx = area.x;
            buf.set_string(cx, y, "  ", Style::default());
            cx += 2;
            buf.set_string(cx, y, &pad(&name, WORKER_W), name_style);
            cx += WORKER_W as u16;
            buf.set_string(cx, y, " │ ", sep_style);
            cx += 3;
            buf.set_string(cx, y, &pad(&clip(&role, role_w), role_w), Style::default().fg(Color::White));
            cx += role_w as u16;
            buf.set_string(cx, y, " │ ", sep_style);
            cx += 3;
            buf.set_string(cx, y, &pad(&clip(&age, LAST_SEEN_W), LAST_SEEN_W), dim);
            cx += LAST_SEEN_W as u16;
            buf.set_string(cx, y, " │ ", sep_style);
            cx += 3;
            buf.set_string(cx, y, &pad(&clip(&activity, ACTIVITY_W), ACTIVITY_W), dim);
        }

        // ── Messages panel ────────────────────────────────────────────────────
        let m_title = format!("─ Messages ({} in last hour) ", messages.len());
        let m_line = format!("{}{}", m_title, "─".repeat(w.saturating_sub(m_title.len())));
        buf.set_string(area.x, msg_panel_y, &clip(&m_line, w), dim);

        // Column headers
        let m_head = format!(
            "  {:<w1$} │ {:<8} │ {:<w2$}",
            "Direction",
            "Time",
            "Content",
            w1 = dir_w,
            w2 = content_w
        );
        buf.set_string(
            area.x,
            msg_panel_y + 1,
            &clip(&m_head, w),
            dim.add_modifier(Modifier::BOLD),
        );

        // Separator
        let m_sep = format!(
            "  {}─┼─{}─┼─{}",
            "─".repeat(dir_w),
            "─".repeat(8),
            "─".repeat(content_w),
        );
        buf.set_string(area.x, msg_panel_y + 2, &clip(&m_sep, w), sep_style);

        // Record data start Y for click hit-testing
        self.msg_data_start_y.set(msg_panel_y + 3);

        // Message rows — display newest first
        let msg_count = messages.len();
        for row_offset in 0..msg_data_rows {
            let display_idx = scroll + row_offset;
            if display_idx >= msg_count {
                break;
            }
            let vec_idx = msg_count - 1 - display_idx;
            let msg = &messages[vec_idx];
            let y = msg_panel_y + 3 + row_offset as u16;
            let is_cursor = display_idx == cursor;

            let direction = if msg.recipient == self.instance_id {
                format!("@{} → you", msg.sender)
            } else {
                format!("you → @{}", msg.recipient)
            };
            let time_str = msg.timestamp.format("%H:%M:%S").to_string();
            let content_str = clip_no_ellipsis(&msg.content, content_w);

            // Cursor row gets a highlighted background
            if is_cursor {
                buf.set_style(
                    Rect::new(area.x, y, area.width, 1),
                    Style::default().bg(Color::Rgb(20, 35, 55)),
                );
            }

            let dir_style = if is_cursor {
                Style::default()
                    .fg(Color::Rgb(0, 255, 163))
                    .add_modifier(Modifier::BOLD)
            } else if msg.recipient == self.instance_id {
                Style::default().fg(Color::Yellow)
            } else {
                Style::default().fg(Color::Cyan)
            };
            let time_style = if is_cursor {
                Style::default().fg(Color::Rgb(0, 255, 163))
            } else {
                dim
            };
            let content_style = if is_cursor {
                Style::default().fg(Color::White).add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(Color::White)
            };

            let mut cx = area.x;
            buf.set_string(cx, y, "  ", Style::default());
            cx += 2;
            buf.set_string(cx, y, &pad(&clip(&direction, dir_w), dir_w), dir_style);
            cx += dir_w as u16;
            buf.set_string(cx, y, " │ ", sep_style);
            cx += 3;
            buf.set_string(cx, y, &pad(&time_str, 8), time_style);
            cx += 8;
            buf.set_string(cx, y, " │ ", sep_style);
            cx += 3;
            buf.set_string(cx, y, &content_str, content_style);
        }

        // ── Footer ────────────────────────────────────────────────────────────
        fill_line(buf, area.x, footer_y, area.width, dim);
        let status_msg = self.status_msg.borrow();
        let footer_text = if let Some(ref e) = *error {
            format!(" Error: {}", e)
        } else if let Some(ref s) = *status_msg {
            format!(" {}", s)
        } else {
            format!(" ↑↓ Navigate  │  Enter View  │  n New message  │  R Reply  │  r Refresh  │  q Quit")
        };
        let footer_style = if error.is_some() {
            Style::default().fg(Color::Red)
        } else {
            dim
        };
        buf.set_string(area.x, footer_y, &clip(&footer_text, w), footer_style);

        drop(workers);
        drop(messages);
        drop(error);
    }
}

// ── ComposeModal ──────────────────────────────────────────────────────────────

#[derive(Clone, Copy, PartialEq)]
enum ComposeField { Recipients, Message }

struct ComposeModal {
    server: String,
    instance_id: String,
    token: Option<String>,
    /// Other workers (self excluded)
    workers: Vec<WorkerInfo>,
    /// Checked state per worker
    selected: RefCell<Vec<bool>>,
    /// Cursor within recipient list
    list_cursor: Cell<usize>,
    /// Scroll offset for recipient list
    list_scroll: Cell<usize>,
    /// Which field is focused
    focused: Cell<ComposeField>,
    /// The message being typed
    message: RefCell<String>,
    /// Optional hash to attach as reply ref
    reply_hash: Option<String>,
    own_id: Cell<Option<WidgetId>>,
    sending: Cell<bool>,
    error: RefCell<Option<String>>,
    /// How many visible rows the recipient list has (updated each render)
    list_visible_rows: Cell<usize>,
}

impl ComposeModal {
    fn new(
        server: String,
        instance_id: String,
        token: Option<String>,
        workers: Vec<WorkerInfo>,
        selected: Vec<bool>,
        reply_hash: Option<String>,
    ) -> Self {
        Self {
            server,
            instance_id,
            token,
            workers,
            selected: RefCell::new(selected),
            list_cursor: Cell::new(0),
            list_scroll: Cell::new(0),
            focused: Cell::new(ComposeField::Recipients),
            message: RefCell::new(String::new()),
            reply_hash,
            own_id: Cell::new(None),
            sending: Cell::new(false),
            error: RefCell::new(None),
            list_visible_rows: Cell::new(4),
        }
    }

    fn clamp_list_scroll(&self) {
        let cursor = self.list_cursor.get();
        let scroll = self.list_scroll.get();
        let visible = self.list_visible_rows.get().max(1);
        if cursor < scroll {
            self.list_scroll.set(cursor);
        } else if cursor >= scroll + visible {
            self.list_scroll.set(cursor + 1 - visible);
        }
    }

    fn do_send(&self, ctx: &AppContext) {
        let message = self.message.borrow().trim().to_string();
        if message.is_empty() {
            *self.error.borrow_mut() = Some("Message cannot be empty".to_string());
            return;
        }
        let selected = self.selected.borrow();
        let recipients: Vec<String> = self.workers.iter().enumerate()
            .filter(|(i, _)| selected.get(*i).copied().unwrap_or(false))
            .map(|(_, w)| w.instance_id.clone())
            .collect();
        drop(selected);
        if recipients.is_empty() {
            *self.error.borrow_mut() = Some("Select at least one recipient".to_string());
            return;
        }
        let Some(id) = self.own_id.get() else { return };
        self.sending.set(true);
        *self.error.borrow_mut() = None;

        // Persist the first recipient as last_compose_recipient
        let mut state = load_read_state();
        state.last_compose_recipient.insert(self.instance_id.clone(), recipients[0].clone());
        save_read_state(&state);

        let server = self.server.clone();
        let instance_id = self.instance_id.clone();
        let token = self.token.clone();
        let reply_hash = self.reply_hash.clone();
        ctx.run_worker(id, send_to_all(server, instance_id, token, recipients, message, reply_hash));
    }
}

static COMPOSE_BINDINGS: &[KeyBinding] = &[
    KeyBinding {
        key: KeyCode::Esc,
        modifiers: KeyModifiers::NONE,
        action: "close",
        description: "Cancel",
        show: true,
    },
    KeyBinding {
        key: KeyCode::Tab,
        modifiers: KeyModifiers::NONE,
        action: "tab",
        description: "Switch field",
        show: true,
    },
    KeyBinding {
        key: KeyCode::Enter,
        modifiers: KeyModifiers::NONE,
        action: "enter",
        description: "Send",
        show: true,
    },
    KeyBinding {
        key: KeyCode::Up,
        modifiers: KeyModifiers::NONE,
        action: "list_up",
        description: "Up",
        show: false,
    },
    KeyBinding {
        key: KeyCode::Down,
        modifiers: KeyModifiers::NONE,
        action: "list_down",
        description: "Down",
        show: false,
    },
];
// Note: Char-key bindings (j/k/space/a) are intentionally absent — they are handled
// in on_event so they don't get consumed before text input in the Message field.

impl Widget for ComposeModal {
    fn widget_type_name(&self) -> &'static str { "ComposeModal" }
    fn can_focus(&self) -> bool { true }
    fn on_mount(&self, id: WidgetId) { self.own_id.set(Some(id)); }
    fn on_unmount(&self, _: WidgetId) { self.own_id.set(None); }
    fn key_bindings(&self) -> &[KeyBinding] { COMPOSE_BINDINGS }

    fn on_action(&self, action: &str, ctx: &AppContext) {
        if self.sending.get() { return; }
        match action {
            "close" => ctx.pop_screen_deferred(),
            "tab" => {
                self.focused.set(match self.focused.get() {
                    ComposeField::Recipients => ComposeField::Message,
                    ComposeField::Message => ComposeField::Recipients,
                });
            }
            "enter" => {
                match self.focused.get() {
                    ComposeField::Recipients => self.focused.set(ComposeField::Message),
                    ComposeField::Message => self.do_send(ctx),
                }
            }
            "list_up" if self.focused.get() == ComposeField::Recipients => {
                let cur = self.list_cursor.get();
                if cur > 0 { self.list_cursor.set(cur - 1); }
            }
            "list_down" if self.focused.get() == ComposeField::Recipients => {
                let len = self.workers.len();
                let cur = self.list_cursor.get();
                if cur + 1 < len { self.list_cursor.set(cur + 1); }
            }
            "toggle" if self.focused.get() == ComposeField::Recipients => {
                let cur = self.list_cursor.get();
                let mut sel = self.selected.borrow_mut();
                if let Some(v) = sel.get_mut(cur) { *v = !*v; }
            }
            "select_all" if self.focused.get() == ComposeField::Recipients => {
                let mut sel = self.selected.borrow_mut();
                let any_unchecked = sel.iter().any(|&v| !v);
                for v in sel.iter_mut() { *v = any_unchecked; }
            }
            _ => {}
        }
    }

    fn on_event(&self, event: &dyn std::any::Any, ctx: &AppContext) -> EventPropagation {
        // Send result
        if let Some(result) = event.downcast_ref::<WorkerResult<SendResult>>() {
            self.sending.set(false);
            match &result.value {
                Ok(_) => ctx.pop_screen_deferred(),
                Err(e) => *self.error.borrow_mut() = Some(e.clone()),
            }
            return EventPropagation::Stop;
        }

        // Raw key events — handle here so char-key bindings (j/k/space/a) don't steal
        // input from the Message field. Always consume to prevent leaking to background.
        if let Some(key) = event.downcast_ref::<KeyEvent>() {
            if self.sending.get() { return EventPropagation::Stop; }
            let in_msg = self.focused.get() == ComposeField::Message;
            let in_rcpt = self.focused.get() == ComposeField::Recipients;
            match key.code {
                KeyCode::Char(c) => {
                    if in_msg {
                        self.message.borrow_mut().push(c);
                    } else if in_rcpt {
                        // Handle recipient-list char shortcuts here since they're not bindings
                        match c {
                            'k' => {
                                let cur = self.list_cursor.get();
                                if cur > 0 { self.list_cursor.set(cur - 1); }
                            }
                            'j' => {
                                let len = self.workers.len();
                                let cur = self.list_cursor.get();
                                if cur + 1 < len { self.list_cursor.set(cur + 1); }
                            }
                            ' ' => {
                                let cur = self.list_cursor.get();
                                let mut sel = self.selected.borrow_mut();
                                if let Some(v) = sel.get_mut(cur) { *v = !*v; }
                            }
                            'a' => {
                                let mut sel = self.selected.borrow_mut();
                                let any_unchecked = sel.iter().any(|&v| !v);
                                for v in sel.iter_mut() { *v = any_unchecked; }
                            }
                            _ => {}
                        }
                    }
                    return EventPropagation::Stop;
                }
                KeyCode::Backspace => {
                    if in_msg { self.message.borrow_mut().pop(); }
                    return EventPropagation::Stop;
                }
                _ => {}
            }
        }

        EventPropagation::Continue
    }

    fn render(&self, _ctx: &AppContext, area: Rect, buf: &mut Buffer) {
        if area.width < 30 || area.height < 10 { return; }

        // Dim background
        for y in area.y..area.y + area.height {
            for x in area.x..area.x + area.width {
                if let Some(cell) = buf.cell_mut((x, y)) {
                    cell.set_bg(Color::Rgb(5, 5, 15));
                    cell.set_fg(Color::DarkGray);
                }
            }
        }

        let n_workers = self.workers.len();
        // dialog height: border(1) + pad(1) + list_header(1) + list rows + pad(1) + msg_top(1) + msg_row(1) + msg_bot(1) + error(1) + footer(1) + border(1)
        let list_rows = n_workers.min(6);
        let dlg_h = (10 + list_rows as u16).min(area.height.saturating_sub(2));
        let dlg_w = ((area.width as usize * 8 / 10) as u16).min(90).max(50);
        let dlg_x = area.x + area.width.saturating_sub(dlg_w) / 2;
        let dlg_y = area.y + area.height.saturating_sub(dlg_h) / 2;

        let bg_style = Style::default().bg(Color::Rgb(15, 15, 30)).fg(Color::White);
        for y in dlg_y..dlg_y + dlg_h {
            fill_line(buf, dlg_x, y, dlg_w, bg_style);
        }

        let sending = self.sending.get();
        let border_col = if sending { Color::Yellow } else { Color::Cyan };
        draw_box(buf, dlg_x, dlg_y, dlg_w, dlg_h, border_col);

        let title = if sending { " Sending… " }
                    else if self.reply_hash.is_some() { " Reply " }
                    else { " New Message " };
        let title_x = dlg_x + dlg_w.saturating_sub(title.len() as u16) / 2;
        buf.set_string(title_x, dlg_y, title,
            Style::default().fg(Color::Black).bg(border_col).add_modifier(Modifier::BOLD));

        let dim = Style::default().fg(Color::DarkGray);
        let inner_x = dlg_x + 2;
        let inner_w = dlg_w.saturating_sub(4) as usize;
        let mut y = dlg_y + 2;
        let max_y = dlg_y + dlg_h - 2; // reserve bottom border row

        // ── Recipient list ─────────────────────────────────────────────────────
        let list_focused = self.focused.get() == ComposeField::Recipients;
        let lc = least_capacitated(&self.workers, &self.instance_id);
        let lbl_style = if list_focused {
            Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)
        } else { dim };
        let selected_count = self.selected.borrow().iter().filter(|&&v| v).count();
        let rcpt_label = format!("Recipients: {}/{} selected  (Space toggle, a all/none)", selected_count, n_workers);
        if y < max_y { buf.set_string(inner_x, y, &clip(&rcpt_label, inner_w), lbl_style); y += 1; }

        let cursor = self.list_cursor.get();
        // Compute visible rows for this render
        let available_list = (max_y.saturating_sub(y + 4)) as usize; // leave room for msg+footer
        let visible = available_list.min(n_workers).max(1);
        self.list_visible_rows.set(visible);
        self.clamp_list_scroll();
        let scroll = self.list_scroll.get();

        let sel = self.selected.borrow();
        for row in 0..visible {
            let idx = scroll + row;
            if idx >= n_workers { break; }
            if y >= max_y { break; }
            let worker = &self.workers[idx];
            let checked = sel.get(idx).copied().unwrap_or(false);
            let is_cursor = list_focused && idx == cursor;
            let star = if Some(worker.instance_id.as_str()) == lc { " ★" } else { "" };
            let check = if checked { "✓" } else { " " };
            let scroll_marker = if idx == scroll && scroll > 0 { "↑" }
                                 else if idx == scroll + visible - 1 && scroll + visible < n_workers { "↓" }
                                 else { " " };
            let row_text = format!(" [{}]{} @{}{}", check, scroll_marker, worker.instance_id, star);
            let row_style = if is_cursor {
                Style::default().fg(Color::Black).bg(Color::Cyan).add_modifier(Modifier::BOLD)
            } else if checked {
                Style::default().fg(Color::Green)
            } else {
                dim
            };
            if is_cursor {
                fill_line(buf, inner_x, y, inner_w as u16, row_style);
            }
            buf.set_string(inner_x, y, &clip(&row_text, inner_w), row_style);
            y += 1;
        }
        drop(sel);

        y += 1; // spacing

        // ── Message field ──────────────────────────────────────────────────────
        let msg_focused = self.focused.get() == ComposeField::Message;
        let border_color = if msg_focused { Color::Cyan } else { Color::Rgb(60, 60, 80) };
        let border_style = Style::default().fg(border_color).bg(Color::Rgb(15, 15, 30));
        let msg_bg = if msg_focused {
            Style::default().fg(Color::White).bg(Color::Rgb(20, 20, 50))
        } else {
            Style::default().fg(Color::Gray).bg(Color::Rgb(25, 25, 40))
        };
        // Top border with "Message" label
        if y < max_y {
            let label = if msg_focused { " Message " } else { " Message " };
            let label_style = if msg_focused {
                Style::default().fg(Color::Black).bg(Color::Cyan).add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(Color::DarkGray).bg(Color::Rgb(60, 60, 80))
            };
            let box_w = inner_w as u16;
            buf.set_string(inner_x, y, "╭", border_style);
            buf.set_string(inner_x + box_w - 1, y, "╮", border_style);
            let label_x = inner_x + 1 + (box_w.saturating_sub(2).saturating_sub(label.len() as u16)) / 2;
            let dash_end = label_x - inner_x - 1;
            for i in 1..=dash_end { buf.set_string(inner_x + i, y, "─", border_style); }
            buf.set_string(label_x, y, label, label_style);
            let after = label_x + label.len() as u16;
            for i in after..inner_x + box_w - 1 { buf.set_string(i, y, "─", border_style); }
            y += 1;
        }
        // Input row
        if y < max_y {
            let msg = self.message.borrow();
            let field_w = inner_w.saturating_sub(2); // inside the box borders
            let display = if msg_focused {
                let s = tail_chars(&msg, field_w.saturating_sub(1));
                format!("{}|", s)
            } else {
                tail_chars(&msg, field_w)
            };
            buf.set_string(inner_x, y, "│", border_style);
            fill_line(buf, inner_x + 1, y, field_w as u16, msg_bg);
            buf.set_string(inner_x + 1, y, &display, msg_bg);
            buf.set_string(inner_x + inner_w as u16 - 1, y, "│", border_style);
            y += 1;
        }
        // Bottom border
        if y < max_y {
            let box_w = inner_w as u16;
            buf.set_string(inner_x, y, "╰", border_style);
            buf.set_string(inner_x + box_w - 1, y, "╯", border_style);
            for i in 1..box_w - 1 { buf.set_string(inner_x + i, y, "─", border_style); }
        }

        // ── Error line ─────────────────────────────────────────────────────────
        if let Some(ref e) = *self.error.borrow() {
            let ey = dlg_y + dlg_h - 3;
            if ey > dlg_y + 2 {
                put(buf, inner_x, ey, e, inner_w, Style::default().fg(Color::Red));
            }
        }

        // ── Footer ─────────────────────────────────────────────────────────────
        let hint = " [Tab] Switch  [↑↓] Navigate  [Space] Toggle  [Enter] Send  [Esc] Cancel ";
        let hint_x = dlg_x + dlg_w.saturating_sub(hint.len() as u16) / 2;
        buf.set_string(hint_x, dlg_y + dlg_h - 2, &clip(hint, dlg_w as usize), dim);
    }
}

// ── MessageModal ──────────────────────────────────────────────────────────────

struct MessageModal {
    msg: Message,
    instance_id: String,
    own_id: Cell<Option<WidgetId>>,
    scroll: Cell<usize>,
    /// Total wrapped content lines; updated each render for scroll clamping.
    total_lines: Cell<usize>,
    /// Visible content rows; updated each render.
    visible_rows: Cell<usize>,
}

impl MessageModal {
    fn new(msg: Message, instance_id: String) -> Self {
        Self {
            msg,
            instance_id,
            own_id: Cell::new(None),
            scroll: Cell::new(0),
            total_lines: Cell::new(0),
            visible_rows: Cell::new(0),
        }
    }

    fn scroll_up(&self) {
        let s = self.scroll.get();
        if s > 0 {
            self.scroll.set(s - 1);
        }
    }

    fn scroll_down(&self) {
        let total = self.total_lines.get();
        let visible = self.visible_rows.get();
        let s = self.scroll.get();
        if total > visible && s + visible < total {
            self.scroll.set(s + 1);
        }
    }

    fn page_up(&self) {
        let visible = self.visible_rows.get().max(1);
        let s = self.scroll.get();
        self.scroll.set(s.saturating_sub(visible));
    }

    fn page_down(&self) {
        let total = self.total_lines.get();
        let visible = self.visible_rows.get().max(1);
        let s = self.scroll.get();
        let max_scroll = total.saturating_sub(visible);
        self.scroll.set((s + visible).min(max_scroll));
    }
}

static MODAL_BINDINGS: &[KeyBinding] = &[
    KeyBinding {
        key: KeyCode::Esc,
        modifiers: KeyModifiers::NONE,
        action: "close",
        description: "Close",
        show: true,
    },
    KeyBinding {
        key: KeyCode::Char('q'),
        modifiers: KeyModifiers::NONE,
        action: "close",
        description: "Close",
        show: false,
    },
    KeyBinding {
        key: KeyCode::Enter,
        modifiers: KeyModifiers::NONE,
        action: "close",
        description: "Close",
        show: false,
    },
    KeyBinding {
        key: KeyCode::Up,
        modifiers: KeyModifiers::NONE,
        action: "scroll_up",
        description: "Scroll up",
        show: false,
    },
    KeyBinding {
        key: KeyCode::Char('k'),
        modifiers: KeyModifiers::NONE,
        action: "scroll_up",
        description: "Scroll up",
        show: false,
    },
    KeyBinding {
        key: KeyCode::Down,
        modifiers: KeyModifiers::NONE,
        action: "scroll_down",
        description: "Scroll down",
        show: false,
    },
    KeyBinding {
        key: KeyCode::Char('j'),
        modifiers: KeyModifiers::NONE,
        action: "scroll_down",
        description: "Scroll down",
        show: false,
    },
    KeyBinding {
        key: KeyCode::PageUp,
        modifiers: KeyModifiers::NONE,
        action: "page_up",
        description: "Page up",
        show: false,
    },
    KeyBinding {
        key: KeyCode::PageDown,
        modifiers: KeyModifiers::NONE,
        action: "page_down",
        description: "Page down",
        show: false,
    },
];

impl Widget for MessageModal {
    fn widget_type_name(&self) -> &'static str {
        "MessageModal"
    }

    fn can_focus(&self) -> bool {
        true
    }

    fn on_mount(&self, id: WidgetId) {
        self.own_id.set(Some(id));
    }

    fn on_unmount(&self, _: WidgetId) {
        self.own_id.set(None);
    }

    fn key_bindings(&self) -> &[KeyBinding] {
        MODAL_BINDINGS
    }

    fn on_action(&self, action: &str, ctx: &AppContext) {
        match action {
            "close" => ctx.pop_screen_deferred(),
            "scroll_up" => self.scroll_up(),
            "scroll_down" => self.scroll_down(),
            "page_up" => self.page_up(),
            "page_down" => self.page_down(),
            _ => {}
        }
    }

    fn render(&self, _ctx: &AppContext, area: Rect, buf: &mut Buffer) {
        if area.width < 20 || area.height < 6 {
            return;
        }

        // Dim everything beneath the dialog
        for y in area.y..area.y + area.height {
            for x in area.x..area.x + area.width {
                if let Some(cell) = buf.cell_mut((x, y)) {
                    cell.set_bg(Color::Rgb(5, 5, 15));
                    cell.set_fg(Color::DarkGray);
                }
            }
        }

        // Dialog box — centered, 70% wide / 70% tall
        let dlg_w = ((area.width as usize * 7 / 10) as u16).min(100).max(50);
        let dlg_h = ((area.height as usize * 7 / 10) as u16).max(10).min(area.height.saturating_sub(4));
        let dlg_x = area.x + area.width.saturating_sub(dlg_w) / 2;
        let dlg_y = area.y + area.height.saturating_sub(dlg_h) / 2;

        // Fill dialog background
        let bg = Style::default().bg(Color::Rgb(15, 15, 30)).fg(Color::White);
        for y in dlg_y..dlg_y + dlg_h {
            fill_line(buf, dlg_x, y, dlg_w, bg);
        }

        // Border
        draw_box(buf, dlg_x, dlg_y, dlg_w, dlg_h, Color::Cyan);

        // Title
        let title = " Message Detail ";
        let title_x = dlg_x + dlg_w.saturating_sub(title.len() as u16) / 2;
        buf.set_string(
            title_x,
            dlg_y,
            title,
            Style::default()
                .fg(Color::Black)
                .bg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        );

        // Inner content
        let inner_x = dlg_x + 2;
        let inner_w = dlg_w.saturating_sub(4) as usize;
        let mut y = dlg_y + 2;
        let max_y = dlg_y + dlg_h - 2;

        let dim = Style::default().fg(Color::DarkGray);

        let direction = if self.msg.recipient == self.instance_id {
            format!("From: @{}  →  you", self.msg.sender)
        } else {
            format!("you  →  @{}", self.msg.recipient)
        };
        put(buf, inner_x, y, &direction, inner_w, Style::default().fg(Color::Yellow));
        y += 1;

        if y < max_y {
            let t = format!("Time:  {}", self.msg.timestamp.format("%Y-%m-%d %H:%M:%S UTC"));
            put(buf, inner_x, y, &t, inner_w, dim);
            y += 1;
        }
        if y < max_y {
            let h = format!("Hash:  {}", self.msg.hash);
            put(buf, inner_x, y, &h, inner_w, dim);
            y += 1;
        }
        if !self.msg.refs.is_empty() && y < max_y {
            let r = format!("Refs:  {}", self.msg.refs.join(", "));
            put(buf, inner_x, y, &r, inner_w, dim);
            y += 1;
        }

        // Separator
        if y < max_y {
            buf.set_string(inner_x, y, &"─".repeat(inner_w), dim);
            y += 1;
        }

        // Word-wrapped content (scrollable)
        let content_lines = wrap_text(&self.msg.content, inner_w);
        let total = content_lines.len();
        let available_rows = max_y.saturating_sub(y) as usize;
        // Clamp scroll so we don't scroll past the end
        let max_scroll = total.saturating_sub(available_rows);
        let scroll = self.scroll.get().min(max_scroll);
        self.scroll.set(scroll);
        self.total_lines.set(total);
        self.visible_rows.set(available_rows);

        for (i, line) in content_lines.iter().enumerate().skip(scroll) {
            if y >= max_y {
                break;
            }
            put(buf, inner_x, y, line, inner_w, Style::default().fg(Color::White));
            y += 1;
            let _ = i;
        }

        // Hint in bottom border — show scroll indicator if content overflows
        let hint = if total > available_rows {
            let pct = if max_scroll == 0 { 100 } else { scroll * 100 / max_scroll };
            format!(" ↑↓/jk/PgUp/PgDn Scroll ({}%)  [Esc] Close ", pct)
        } else {
            " [Esc] Close ".to_string()
        };
        let hint_x = dlg_x + dlg_w.saturating_sub(hint.len() as u16) / 2;
        buf.set_string(hint_x, dlg_y + dlg_h - 1, &hint, dim);
    }
}

// ── Render helpers ────────────────────────────────────────────────────────────

/// Truncate to `max` chars, appending '…' if truncated.
fn clip(s: &str, max: usize) -> String {
    if max == 0 {
        return String::new();
    }
    let chars: Vec<char> = s.chars().collect();
    if chars.len() <= max {
        chars.into_iter().collect()
    } else {
        let mut out: String = chars.into_iter().take(max.saturating_sub(1)).collect();
        out.push('…');
        out
    }
}

/// Truncate to `max` chars without adding an ellipsis (for content columns).
fn clip_no_ellipsis(s: &str, max: usize) -> String {
    s.chars().take(max).collect()
}

/// Pad or truncate to exactly `width` chars.
fn pad(s: &str, width: usize) -> String {
    let chars: Vec<char> = s.chars().collect();
    if chars.len() >= width {
        chars.into_iter().take(width).collect()
    } else {
        format!("{}{}", s, " ".repeat(width - chars.len()))
    }
}

/// Fill a full row with spaces in the given style (sets background).
fn fill_line(buf: &mut Buffer, x: u16, y: u16, w: u16, style: Style) {
    buf.set_string(x, y, &" ".repeat(w as usize), style);
}

/// Write a single line, clipped to `max_w`.
fn put(buf: &mut Buffer, x: u16, y: u16, s: &str, max_w: usize, style: Style) {
    buf.set_string(x, y, &clip(s, max_w), style);
}

/// Draw a rounded box border.
fn draw_box(buf: &mut Buffer, x: u16, y: u16, w: u16, h: u16, color: Color) {
    let bg = Color::Rgb(15, 15, 30);
    let s = Style::default().fg(color).bg(bg);
    if w < 2 || h < 2 {
        return;
    }
    buf.set_string(x, y, "╭", s);
    buf.set_string(x + w - 1, y, "╮", s);
    buf.set_string(x, y + h - 1, "╰", s);
    buf.set_string(x + w - 1, y + h - 1, "╯", s);
    for i in 1..w - 1 {
        buf.set_string(x + i, y, "─", s);
        buf.set_string(x + i, y + h - 1, "─", s);
    }
    for i in 1..h - 1 {
        buf.set_string(x, y + i, "│", s);
        buf.set_string(x + w - 1, y + i, "│", s);
    }
}

/// Human-readable age string.
fn age_str(dt: chrono::DateTime<chrono::Utc>) -> String {
    let secs = chrono::Utc::now()
        .signed_duration_since(dt)
        .num_seconds()
        .max(0);
    if secs < 60 {
        format!("{}s ago", secs)
    } else if secs < 3600 {
        format!("{}m ago", secs / 60)
    } else {
        format!("{}h ago", secs / 3600)
    }
}

/// Return the last `max` chars of `s` (so the cursor end is always visible).
fn tail_chars(s: &str, max: usize) -> String {
    if max == 0 { return String::new(); }
    let chars: Vec<char> = s.chars().collect();
    if chars.len() <= max {
        chars.into_iter().collect()
    } else {
        chars[chars.len() - max..].iter().collect()
    }
}

/// Word-wrap `text` to lines of at most `width` chars.
fn wrap_text(text: &str, width: usize) -> Vec<String> {
    if width == 0 {
        return vec![];
    }
    let mut lines = vec![];
    for para in text.split('\n') {
        if para.is_empty() {
            lines.push(String::new());
            continue;
        }
        let mut current = String::new();
        for word in para.split_whitespace() {
            if current.is_empty() {
                current.push_str(word);
            } else if current.chars().count() + 1 + word.len() <= width {
                current.push(' ');
                current.push_str(word);
            } else {
                lines.push(std::mem::take(&mut current));
                current.push_str(word);
            }
        }
        if !current.is_empty() {
            lines.push(current);
        }
    }
    lines
}
