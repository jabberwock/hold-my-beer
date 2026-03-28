use anyhow::Result;
use crossterm::{
    event::{self, Event, KeyCode, KeyEventKind},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Cell, Paragraph, Row, Table},
    Terminal,
};
use std::io;
use std::time::{Duration, Instant};

use crate::client::{CollabClient, Message, WorkerInfo};

pub async fn run(server: &str, instance_id: &str, interval_secs: u64, token: Option<&str>) -> Result<()> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let client = CollabClient::new(server, instance_id, token);
    let mut last_refresh = Instant::now() - Duration::from_secs(interval_secs + 1);
    let mut workers: Vec<WorkerInfo> = vec![];
    let mut messages: Vec<Message> = vec![];
    let mut error: Option<String> = None;

    loop {
        // Refresh data
        if last_refresh.elapsed() >= Duration::from_secs(interval_secs) {
            match fetch_all(&client, instance_id).await {
                Ok((mut w, m)) => {
                    w.sort_by(|a, b| a.instance_id.cmp(&b.instance_id));
                    workers = w;
                    messages = m;
                    error = None;
                }
                Err(e) => {
                    error = Some(e.to_string());
                }
            }
            last_refresh = Instant::now();
        }

        // Draw
        terminal.draw(|f| {
            let area = f.area();

            let chunks = Layout::default()
                .direction(Direction::Vertical)
                .constraints([
                    Constraint::Length(1),   // header
                    Constraint::Min(6),      // roster
                    Constraint::Min(8),      // messages
                    Constraint::Length(1),   // footer
                ])
                .split(area);

            // Header
            let header = Paragraph::new(Line::from(vec![
                Span::styled(" collab monitor ", Style::default().fg(Color::Black).bg(Color::Cyan).add_modifier(Modifier::BOLD)),
                Span::raw(format!("  @{}  ", instance_id)),
                Span::styled(
                    format!("server: {}", server),
                    Style::default().fg(Color::DarkGray),
                ),
            ]));
            f.render_widget(header, chunks[0]);

            // Roster table
            let roster_rows: Vec<Row> = workers.iter().map(|w| {
                let you = if w.instance_id == instance_id { " ◀" } else { "" };
                let age = {
                    let secs = chrono::Utc::now()
                        .signed_duration_since(w.last_seen)
                        .num_seconds();
                    if secs < 60 { format!("{}s ago", secs) }
                    else { format!("{}m ago", secs / 60) }
                };
                let name_style = if w.instance_id == instance_id {
                    Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)
                } else {
                    Style::default().fg(Color::Green)
                };
                Row::new(vec![
                    Cell::from(format!("@{}{}", w.instance_id, you)).style(name_style),
                    Cell::from(if w.role.is_empty() { "—".to_string() } else { w.role.clone() })
                        .style(Style::default().fg(Color::White)),
                    Cell::from(age).style(Style::default().fg(Color::DarkGray)),
                    Cell::from(if w.message_count > 0 { format!("{} msgs", w.message_count) } else { String::new() })
                        .style(Style::default().fg(Color::DarkGray)),
                ])
            }).collect();

            let roster_title = format!(" Roster ({} online) ", workers.len());
            let roster = Table::new(
                roster_rows,
                [
                    Constraint::Length(20),
                    Constraint::Min(30),
                    Constraint::Length(10),
                    Constraint::Length(10),
                ],
            )
            .block(Block::default().borders(Borders::ALL).title(roster_title))
            .header(Row::new(vec!["Worker", "Role", "Last Seen", "Activity"])
                .style(Style::default().fg(Color::DarkGray).add_modifier(Modifier::BOLD)));
            f.render_widget(roster, chunks[1]);

            // Messages panel
            let msg_lines: Vec<Line> = if messages.is_empty() {
                vec![Line::from(Span::styled("  No messages in the last hour.", Style::default().fg(Color::DarkGray)))]
            } else {
                messages.iter().rev().take(20).map(|m| {
                    let direction = if m.recipient == instance_id {
                        Span::styled(format!("@{} → you", m.sender), Style::default().fg(Color::Yellow))
                    } else {
                        Span::styled(format!("you → @{}", m.recipient), Style::default().fg(Color::Cyan))
                    };
                    let time = Span::styled(
                        format!("  {}", m.timestamp.format("%H:%M:%S")),
                        Style::default().fg(Color::DarkGray),
                    );
                    let content = Span::raw(format!("  {}", truncate(&m.content, 60)));
                    Line::from(vec![direction, time, content])
                }).collect()
            };

            let msg_count = messages.len();
            let messages_widget = Paragraph::new(msg_lines)
                .block(Block::default().borders(Borders::ALL)
                    .title(format!(" Messages ({} in last hour) ", msg_count)));
            f.render_widget(messages_widget, chunks[2]);

            // Footer
            let next_refresh = interval_secs.saturating_sub(last_refresh.elapsed().as_secs());
            let status = if let Some(ref e) = error {
                format!(" Error: {} ", e)
            } else {
                format!(" Refreshing in {}s  |  q to quit ", next_refresh)
            };
            let footer_style = if error.is_some() {
                Style::default().fg(Color::Red)
            } else {
                Style::default().fg(Color::DarkGray)
            };
            f.render_widget(Paragraph::new(status).style(footer_style), chunks[3]);
        })?;

        // Input — non-blocking with short timeout
        if event::poll(Duration::from_millis(200))? {
            if let Event::Key(key) = event::read()? {
                if key.kind == KeyEventKind::Press {
                    match key.code {
                        KeyCode::Char('q') | KeyCode::Esc => break,
                        KeyCode::Char('r') => {
                            // Force refresh
                            last_refresh = Instant::now() - Duration::from_secs(interval_secs + 1);
                        }
                        _ => {}
                    }
                }
            }
        }
    }

    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    Ok(())
}

async fn fetch_all(client: &CollabClient, instance_id: &str) -> Result<(Vec<WorkerInfo>, Vec<Message>)> {
    let workers = client.fetch_roster_pub().await?;
    let messages = client.fetch_history_pub(instance_id).await?;
    Ok((workers, messages))
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        format!("{}…", s.chars().take(max - 1).collect::<String>())
    }
}
