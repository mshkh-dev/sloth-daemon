//! Terminal User Interface for sloth (`sloth-ui`).
//!
//! Connects on demand to sloth-daemon Unix domain control socket,
//! displays Active Cages and Audit Stream, and leaves no background overhead.

use std::io::{self, BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crossterm::{
    event::{self, Event, KeyCode, KeyEventKind, KeyModifiers},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{
    backend::{Backend, CrosstermBackend},
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, Paragraph, Row, Table, TableState},
    Frame, Terminal,
};
use sloth_proto::{CageRecord, DecisionRecord, Request, RequestEnvelope, Response, StatusData};

pub const DEFAULT_SOCKET_PATH: &str = "/run/sloth/sloth.sock";
const POLL_INTERVAL: Duration = Duration::from_millis(1000);
const SOCKET_TIMEOUT: Duration = Duration::from_millis(300);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActiveTab {
    Cages,
    Audit,
}

pub struct AppState {
    pub socket_path: PathBuf,
    pub active_tab: ActiveTab,
    pub status: Option<StatusData>,
    pub cages: Vec<CageRecord>,
    pub decisions: Vec<DecisionRecord>,
    pub error_message: Option<String>,
    pub last_poll: Instant,
    pub cages_table_state: TableState,
    pub audit_table_state: TableState,
    pub selected_reason: Option<String>,
}

impl AppState {
    pub fn new(socket_path: PathBuf) -> Self {
        let mut cages_state = TableState::default();
        let mut audit_state = TableState::default();
        cages_state.select(Some(0));
        audit_state.select(Some(0));

        Self {
            socket_path,
            active_tab: ActiveTab::Cages,
            status: None,
            cages: Vec::new(),
            decisions: Vec::new(),
            error_message: None,
            // Trigger poll immediately
            last_poll: Instant::now() - Duration::from_secs(5),
            cages_table_state: cages_state,
            audit_table_state: audit_state,
            selected_reason: None,
        }
    }

    pub fn poll_daemon_if_needed(&mut self) {
        let now = Instant::now();
        if now.duration_since(self.last_poll) >= POLL_INTERVAL {
            self.last_poll = now;
            self.fetch_all();
        }
    }

    pub fn fetch_all(&mut self) {
        let path = &self.socket_path;
        if !path.exists() {
            self.error_message = Some("daemon not running or socket missing".to_string());
            self.status = None;
            return;
        }

        // Query status
        match query_socket::<StatusData>(path, Request::Status) {
            Ok(s) => {
                self.status = Some(s);
                self.error_message = None;
            }
            Err(e) => {
                self.error_message = Some(e);
                self.status = None;
                return;
            }
        }

        // Query cages
        if let Ok(c) = query_socket::<Vec<CageRecord>>(path, Request::Cages) {
            self.cages = c;
            if self.cages.is_empty() {
                self.cages_table_state.select(None);
            } else if self.cages_table_state.selected().is_none() {
                self.cages_table_state.select(Some(0));
            } else if let Some(idx) = self.cages_table_state.selected() {
                if idx >= self.cages.len() {
                    self.cages_table_state.select(Some(self.cages.len() - 1));
                }
            }
        }

        // Query decisions
        if let Ok(d) =
            query_socket::<Vec<DecisionRecord>>(path, Request::Decisions { limit: Some(256) })
        {
            self.decisions = d;
            if self.decisions.is_empty() {
                self.audit_table_state.select(None);
            } else if self.audit_table_state.selected().is_none() {
                self.audit_table_state.select(Some(0));
            } else if let Some(idx) = self.audit_table_state.selected() {
                if idx >= self.decisions.len() {
                    self.audit_table_state
                        .select(Some(self.decisions.len() - 1));
                }
            }
        }
    }

    pub fn next_row(&mut self) {
        match self.active_tab {
            ActiveTab::Cages => {
                if self.cages.is_empty() {
                    self.cages_table_state.select(None);
                    return;
                }
                let i = match self.cages_table_state.selected() {
                    Some(i) => {
                        if i >= self.cages.len().saturating_sub(1) {
                            0
                        } else {
                            i + 1
                        }
                    }
                    None => 0,
                };
                self.cages_table_state.select(Some(i));
            }
            ActiveTab::Audit => {
                if self.decisions.is_empty() {
                    self.audit_table_state.select(None);
                    return;
                }
                let i = match self.audit_table_state.selected() {
                    Some(i) => {
                        if i >= self.decisions.len().saturating_sub(1) {
                            0
                        } else {
                            i + 1
                        }
                    }
                    None => 0,
                };
                self.audit_table_state.select(Some(i));
            }
        }
    }

    pub fn prev_row(&mut self) {
        match self.active_tab {
            ActiveTab::Cages => {
                if self.cages.is_empty() {
                    self.cages_table_state.select(None);
                    return;
                }
                let i = match self.cages_table_state.selected() {
                    Some(i) => {
                        if i == 0 {
                            self.cages.len().saturating_sub(1)
                        } else {
                            i - 1
                        }
                    }
                    None => 0,
                };
                self.cages_table_state.select(Some(i));
            }
            ActiveTab::Audit => {
                if self.decisions.is_empty() {
                    self.audit_table_state.select(None);
                    return;
                }
                let i = match self.audit_table_state.selected() {
                    Some(i) => {
                        if i == 0 {
                            self.decisions.len().saturating_sub(1)
                        } else {
                            i - 1
                        }
                    }
                    None => 0,
                };
                self.audit_table_state.select(Some(i));
            }
        }
    }

    pub fn show_selected_reason(&mut self) {
        match self.active_tab {
            ActiveTab::Cages => {
                if let Some(idx) = self.cages_table_state.selected() {
                    if let Some(cage) = self.cages.get(idx) {
                        self.selected_reason = Some(sanitize_str(&cage.reason));
                    }
                }
            }
            ActiveTab::Audit => {
                if let Some(idx) = self.audit_table_state.selected() {
                    if let Some(dec) = self.decisions.get(idx) {
                        self.selected_reason = Some(sanitize_str(&dec.reason));
                    }
                }
            }
        }
    }
}

/// Send request and parse response over a fresh blocking Unix connection.
fn query_socket<T: serde::de::DeserializeOwned>(path: &Path, req: Request) -> Result<T, String> {
    let mut stream = match UnixStream::connect(path) {
        Ok(s) => s,
        Err(err) => {
            return match err.kind() {
                io::ErrorKind::PermissionDenied => Err("run with sudo".to_string()),
                io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused => {
                    Err("daemon not running or socket missing".to_string())
                }
                _ => Err(format!("daemon connection error: {err}")),
            };
        }
    };

    let _ = stream.set_read_timeout(Some(SOCKET_TIMEOUT));
    let _ = stream.set_write_timeout(Some(SOCKET_TIMEOUT));

    let envelope = RequestEnvelope {
        proto: sloth_proto::PROTO_VERSION,
        request: req,
    };

    let payload = match serde_json::to_string(&envelope) {
        Ok(p) => p,
        Err(e) => return Err(format!("serialization error: {e}")),
    };

    if let Err(e) = stream.write_all(format!("{payload}\n").as_bytes()) {
        return Err(format!("failed to send request: {e}"));
    }

    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    if let Err(e) = reader.read_line(&mut line) {
        return Err(format!("failed to read response: {e}"));
    }

    let resp: Response<T> = match serde_json::from_str(&line) {
        Ok(r) => r,
        Err(_) => return Err("daemon returned invalid JSON".to_string()),
    };

    if !resp.ok {
        return Err(resp
            .error
            .unwrap_or_else(|| "unknown daemon error".to_string()));
    }

    resp.data.ok_or_else(|| "missing response data".to_string())
}

/// Strip control characters and sanitize strings for safe terminal rendering.
pub fn sanitize_str(s: &str) -> String {
    s.chars()
        .filter(|c| !c.is_control() || *c == '\n' || *c == '\t')
        .collect()
}

/// Truncate long strings with an ellipsis, preventing line wrapping in tables.
pub fn truncate_ellipsis(s: &str, max_len: usize) -> String {
    let sanitized = sanitize_str(s);
    if sanitized.chars().count() <= max_len {
        sanitized
    } else if max_len <= 3 {
        "...".chars().take(max_len).collect()
    } else {
        let keep: String = sanitized.chars().take(max_len - 3).collect();
        format!("{keep}...")
    }
}

/// Render UI into frame
pub fn ui_render(f: &mut Frame, state: &mut AppState) {
    let size = f.area();
    if size.width == 0 || size.height == 0 {
        return;
    }

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1), // Top Header line
            Constraint::Length(1), // Tabs indicator
            Constraint::Min(3),    // Main Table View
            Constraint::Length(1), // Keybinds footer
        ])
        .split(size);

    // 1. One-line header
    let header_line = match &state.status {
        Some(st) => {
            let mode_str = &st.mode;
            let dry_str = if st.dry_run {
                "dry-run: ON"
            } else {
                "dry-run: OFF"
            };
            let count = st.throttled_count;
            let cap = st.cap;
            let uptime_m = st.uptime_secs / 60;
            let uptime_s = st.uptime_secs % 60;
            format!(
                " Mode: {} | {} | Throttled: {}/{} | Uptime: {}m {}s",
                mode_str, dry_str, count, cap, uptime_m, uptime_s
            )
        }
        None => {
            let msg = state
                .error_message
                .as_deref()
                .unwrap_or("daemon not reachable");
            format!(" [STATUS: {}]", msg)
        }
    };
    let header_widget = Paragraph::new(Line::from(vec![
        Span::styled(
            " Sloth Control Center ",
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw(header_line),
    ]))
    .style(Style::default().bg(Color::DarkGray).fg(Color::White));
    f.render_widget(header_widget, chunks[0]);

    // 2. Tab selection line
    let (cages_tab, audit_tab) = match state.active_tab {
        ActiveTab::Cages => (
            Span::styled(
                " [1] Active Cages (Tab) ",
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD | Modifier::UNDERLINED),
            ),
            Span::styled(" [2] Audit Stream ", Style::default().fg(Color::DarkGray)),
        ),
        ActiveTab::Audit => (
            Span::styled(" [1] Active Cages ", Style::default().fg(Color::DarkGray)),
            Span::styled(
                " [2] Audit Stream (Tab) ",
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD | Modifier::UNDERLINED),
            ),
        ),
    };
    let tabs_widget = Paragraph::new(Line::from(vec![cages_tab, Span::raw(" | "), audit_tab]));
    f.render_widget(tabs_widget, chunks[1]);

    // 3. Main content table
    let table_area = chunks[2];
    match state.active_tab {
        ActiveTab::Cages => render_cages_table(f, table_area, state),
        ActiveTab::Audit => render_audit_table(f, table_area, state),
    }

    // 4. Footer line
    let footer_text = " [q/Ctrl+C] Quit  [Tab] Switch View  [↑/↓] Select Row  [Enter] Full Reason ";
    let footer_widget = Paragraph::new(Line::from(vec![Span::styled(
        footer_text,
        Style::default().fg(Color::Gray),
    )]));
    f.render_widget(footer_widget, chunks[3]);

    // Detail popup if Enter was pressed
    if let Some(reason) = &state.selected_reason {
        render_reason_popup(f, size, reason);
    }
}

fn render_cages_table(f: &mut Frame, area: Rect, state: &mut AppState) {
    let header = Row::new(vec![
        "PID",
        "Name",
        "Quota",
        "Throttled",
        "TTL Rem",
        "Trigger Reason",
    ])
    .style(
        Style::default()
            .fg(Color::Yellow)
            .add_modifier(Modifier::BOLD),
    )
    .bottom_margin(1);

    let max_reason_width = area.width.saturating_sub(45).max(10) as usize;

    let rows: Vec<Row> = if state.cages.is_empty() {
        vec![Row::new(vec![CellContent::from(
            "No active cages (no processes currently throttled)",
        )])]
    } else {
        state
            .cages
            .iter()
            .map(|c| {
                let pid_str = c.pid.to_string();
                let comm_str = truncate_ellipsis(&c.comm, 15);
                let quota_str = format!("{}%", c.quota_pct);
                let throttled_str = format!("{}s", c.throttled_secs);
                let ttl_str = format!("{}s", c.ttl_remaining_secs);
                let reason_str = truncate_ellipsis(&c.reason, max_reason_width);

                Row::new(vec![
                    pid_str,
                    comm_str,
                    quota_str,
                    throttled_str,
                    ttl_str,
                    reason_str,
                ])
            })
            .collect()
    };

    let table = Table::new(
        rows,
        [
            Constraint::Length(8),
            Constraint::Length(16),
            Constraint::Length(8),
            Constraint::Length(10),
            Constraint::Length(10),
            Constraint::Min(10),
        ],
    )
    .header(header)
    .block(
        Block::default()
            .borders(Borders::ALL)
            .title(format!(" Active Cages ({}) ", state.cages.len())),
    )
    .row_highlight_style(
        Style::default()
            .bg(Color::Blue)
            .fg(Color::White)
            .add_modifier(Modifier::BOLD),
    )
    .highlight_symbol(">> ");

    f.render_stateful_widget(table, area, &mut state.cages_table_state);
}

fn render_audit_table(f: &mut Frame, area: Rect, state: &mut AppState) {
    let header = Row::new(vec![
        "PID",
        "Name",
        "CPU %",
        "Action",
        "Conf",
        "Decision Reason",
    ])
    .style(
        Style::default()
            .fg(Color::Yellow)
            .add_modifier(Modifier::BOLD),
    )
    .bottom_margin(1);

    let max_reason_width = area.width.saturating_sub(45).max(10) as usize;

    let rows: Vec<Row> = if state.decisions.is_empty() {
        vec![Row::new(vec![CellContent::from("Audit log is empty")])]
    } else {
        state
            .decisions
            .iter()
            .map(|d| {
                let pid_str = d.pid.to_string();
                let comm_str = truncate_ellipsis(&d.comm, 15);
                let cpu_str = format!("{:.1}%", d.cpu_percent);
                let action_style = match d.action.as_str() {
                    "Throttle" => Style::default().fg(Color::Red),
                    "Release" => Style::default().fg(Color::Green),
                    _ => Style::default().fg(Color::White),
                };
                let action_span = Span::styled(d.action.clone(), action_style);
                let conf_str = d
                    .confidence
                    .map(|c| format!("{:.2}", c))
                    .unwrap_or_else(|| "-".to_string());
                let reason_str = truncate_ellipsis(&d.reason, max_reason_width);

                Row::new(vec![
                    Span::raw(pid_str),
                    Span::raw(comm_str),
                    Span::raw(cpu_str),
                    action_span,
                    Span::raw(conf_str),
                    Span::raw(reason_str),
                ])
            })
            .collect()
    };

    let table = Table::new(
        rows,
        [
            Constraint::Length(8),
            Constraint::Length(16),
            Constraint::Length(8),
            Constraint::Length(10),
            Constraint::Length(8),
            Constraint::Min(10),
        ],
    )
    .header(header)
    .block(Block::default().borders(Borders::ALL).title(format!(
        " Audit Stream ({} decisions) ",
        state.decisions.len()
    )))
    .row_highlight_style(
        Style::default()
            .bg(Color::Blue)
            .fg(Color::White)
            .add_modifier(Modifier::BOLD),
    )
    .highlight_symbol(">> ");

    f.render_stateful_widget(table, area, &mut state.audit_table_state);
}

struct CellContent(String);
impl From<&str> for CellContent {
    fn from(s: &str) -> Self {
        CellContent(s.to_string())
    }
}
impl From<CellContent> for ratatui::widgets::Cell<'_> {
    fn from(c: CellContent) -> Self {
        ratatui::widgets::Cell::from(c.0)
    }
}

fn render_reason_popup(f: &mut Frame, area: Rect, reason: &str) {
    let popup_width = area.width.saturating_sub(6).min(80).max(20);
    let popup_height = 8.min(area.height.saturating_sub(2)).max(4);

    let x = (area.width.saturating_sub(popup_width)) / 2;
    let y = (area.height.saturating_sub(popup_height)) / 2;
    let popup_area = Rect::new(x, y, popup_width, popup_height);

    f.render_widget(Clear, popup_area);

    let block = Block::default()
        .borders(Borders::ALL)
        .title(" Full Reason (Esc/Enter to close) ")
        .style(Style::default().bg(Color::Black).fg(Color::White));

    let paragraph = Paragraph::new(reason)
        .block(block)
        .style(Style::default().fg(Color::Yellow));

    f.render_widget(paragraph, popup_area);
}

/// Parse command line arguments for --socket <path>
fn parse_socket_path() -> PathBuf {
    let args: Vec<String> = std::env::args().collect();
    let mut i = 1;
    while i < args.len() {
        if args[i] == "--socket" && i + 1 < args.len() {
            return PathBuf::from(&args[i + 1]);
        }
        i += 1;
    }
    PathBuf::from(DEFAULT_SOCKET_PATH)
}

fn main() -> io::Result<()> {
    let socket_path = parse_socket_path();

    // Panic hook installation for terminal restoration
    let original_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |panic_info| {
        let _ = disable_raw_mode();
        let _ = execute!(io::stdout(), LeaveAlternateScreen);
        original_hook(panic_info);
    }));

    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let mut state = AppState::new(socket_path);

    let res = run_app(&mut terminal, &mut state);

    // Terminal cleanup
    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;

    if let Err(err) = res {
        eprintln!("Application error: {err}");
    }

    Ok(())
}

fn run_app<B: Backend>(terminal: &mut Terminal<B>, state: &mut AppState) -> io::Result<()> {
    loop {
        state.poll_daemon_if_needed();

        terminal.draw(|f| ui_render(f, state))?;

        // Short timeout poll so keyboard remains responsive regardless of daemon state
        if event::poll(Duration::from_millis(50))? {
            if let Event::Key(key) = event::read()? {
                if key.kind == KeyEventKind::Press {
                    // Ctrl+C quits cleanly
                    if key.modifiers.contains(KeyModifiers::CONTROL)
                        && key.code == KeyCode::Char('c')
                    {
                        break;
                    }

                    if state.selected_reason.is_some() {
                        if key.code == KeyCode::Esc
                            || key.code == KeyCode::Enter
                            || key.code == KeyCode::Char('q')
                        {
                            state.selected_reason = None;
                        }
                        continue;
                    }

                    match key.code {
                        KeyCode::Char('q') => break,
                        KeyCode::Tab => {
                            state.active_tab = match state.active_tab {
                                ActiveTab::Cages => ActiveTab::Audit,
                                ActiveTab::Audit => ActiveTab::Cages,
                            };
                        }
                        KeyCode::Down | KeyCode::Char('j') => state.next_row(),
                        KeyCode::Up | KeyCode::Char('k') => state.prev_row(),
                        KeyCode::Enter => state.show_selected_reason(),
                        _ => {}
                    }
                }
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::backend::TestBackend;

    #[test]
    fn test_sanitization_and_truncation() {
        let dirty = "foo\x07bar\x1b[31mbaz";
        let clean = sanitize_str(dirty);
        assert!(!clean.contains('\x07'));
        assert!(!clean.contains('\x1b'));

        let truncated = truncate_ellipsis("1234567890", 6);
        assert_eq!(truncated, "123...");
        let exact = truncate_ellipsis("123", 5);
        assert_eq!(exact, "123");
    }

    #[test]
    fn test_render_empty_views_test_backend() {
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut state = AppState::new(PathBuf::from("/tmp/nonexistent.sock"));

        // Render Active Cages (empty)
        state.active_tab = ActiveTab::Cages;
        terminal.draw(|f| ui_render(f, &mut state)).unwrap();

        // Render Audit Stream (empty)
        state.active_tab = ActiveTab::Audit;
        terminal.draw(|f| ui_render(f, &mut state)).unwrap();
    }

    #[test]
    fn test_render_small_20x5_terminal_no_panic() {
        let backend = TestBackend::new(20, 5);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut state = AppState::new(PathBuf::from("/tmp/nonexistent.sock"));

        // Both views in tight dimensions
        state.active_tab = ActiveTab::Cages;
        terminal.draw(|f| ui_render(f, &mut state)).unwrap();

        state.active_tab = ActiveTab::Audit;
        terminal.draw(|f| ui_render(f, &mut state)).unwrap();
    }

    #[test]
    fn test_render_populated_views_test_backend() {
        let backend = TestBackend::new(100, 30);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut state = AppState::new(PathBuf::from("/tmp/nonexistent.sock"));

        state.status = Some(StatusData {
            version: "0.1.0".to_string(),
            dry_run: true,
            active_cages: 1,
            uptime_secs: 180,
            decisions_recorded: 1,
            mode: "rules".to_string(),
            throttled_count: 1,
            cap: 5,
        });

        state.cages = vec![CageRecord {
            pid: 1234,
            comm: "stress-ng".into(),
            quota_pct: 20,
            throttled_secs: 42,
            ttl_remaining_secs: 558,
            reason: "CPU sustained above 90%".into(),
        }];

        state.decisions = vec![DecisionRecord {
            timestamp_epoch_secs: 1000,
            pid: 1234,
            comm: "stress-ng".into(),
            cpu_percent: 94.2,
            action: "Throttle".into(),
            reason: "CPU sustained above 90%".into(),
            confidence: Some(0.9),
        }];

        state.active_tab = ActiveTab::Cages;
        terminal.draw(|f| ui_render(f, &mut state)).unwrap();

        state.active_tab = ActiveTab::Audit;
        terminal.draw(|f| ui_render(f, &mut state)).unwrap();

        // Enter modal popup
        state.show_selected_reason();
        terminal.draw(|f| ui_render(f, &mut state)).unwrap();
    }
}
