//! Terminal UI for deskd — real-time observability dashboard.
//!
//! Launched via `deskd serve --tui`. Connects to each agent's bus as a
//! `tui` client with `*` subscription to receive all messages.
//!
//! Two views (Phase A):
//! - **Dashboard** (key `1`): agents, task queue, active workflows, bus tail
//! - **Bus Stream** (key `2`): live tail of all bus messages with follow mode
//!
//! Navigation: `1`/`2` switch views, `j`/`k` scroll, `q` quit TUI, `Q` quit all.

#[cfg(feature = "tui")]
pub mod app {
    use std::collections::VecDeque;
    use std::io;
    use std::time::{Duration, Instant};

    use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyModifiers};
    use crossterm::terminal::{
        EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
    };
    use ratatui::Terminal;
    use ratatui::backend::CrosstermBackend;
    use ratatui::layout::{Constraint, Direction, Layout, Rect};
    use ratatui::style::{Color, Modifier, Style};
    use ratatui::text::{Line, Span};
    use ratatui::widgets::{Block, Borders, List, ListItem, Paragraph, Tabs};
    use tokio::sync::mpsc;

    use crate::domain::task::TaskStatus;
    use crate::ports::store::{StateMachineReader, TaskReader};

    /// Maximum number of bus messages to keep in the ring buffer.
    const MAX_BUS_MESSAGES: usize = 500;

    /// A bus message captured for display.
    #[derive(Debug, Clone)]
    pub struct BusEvent {
        pub timestamp: String,
        pub source: String,
        pub target: String,
        pub payload_preview: String,
    }

    /// Agent summary for the dashboard.
    #[derive(Debug, Clone)]
    pub struct AgentInfo {
        pub name: String,
        pub status: String,
        pub current_task: String,
        pub cost_usd: f64,
        pub turns: u32,
    }

    /// Active view selection.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum View {
        Dashboard,
        BusStream,
    }

    /// TUI application state.
    pub struct App {
        pub view: View,
        pub agents: Vec<AgentInfo>,
        pub bus_messages: VecDeque<BusEvent>,
        pub task_summary: (usize, usize, usize), // pending, active, done
        pub workflow_summary: Vec<(String, String, String)>, // id, model, state
        pub bus_scroll: usize,
        pub follow_mode: bool,
        pub should_quit: bool,
        pub should_quit_all: bool,
    }

    impl App {
        pub fn new() -> Self {
            Self {
                view: View::Dashboard,
                agents: Vec::new(),
                bus_messages: VecDeque::with_capacity(MAX_BUS_MESSAGES),
                task_summary: (0, 0, 0),
                workflow_summary: Vec::new(),
                bus_scroll: 0,
                follow_mode: true,
                should_quit: false,
                should_quit_all: false,
            }
        }
    }

    impl Default for App {
        fn default() -> Self {
            Self::new()
        }
    }

    impl App {
        pub fn push_bus_event(&mut self, event: BusEvent) {
            if self.bus_messages.len() >= MAX_BUS_MESSAGES {
                self.bus_messages.pop_front();
            }
            self.bus_messages.push_back(event);
            if self.follow_mode {
                self.bus_scroll = self.bus_messages.len().saturating_sub(1);
            }
        }

        fn handle_key(&mut self, key: KeyEvent) {
            match key.code {
                KeyCode::Char('q') => {
                    if key.modifiers.contains(KeyModifiers::SHIFT) {
                        self.should_quit_all = true;
                    } else {
                        self.should_quit = true;
                    }
                }
                KeyCode::Char('Q') => {
                    self.should_quit_all = true;
                }
                KeyCode::Char('1') => self.view = View::Dashboard,
                KeyCode::Char('2') => self.view = View::BusStream,
                KeyCode::Char('j') | KeyCode::Down => {
                    self.follow_mode = false;
                    if self.view == View::BusStream {
                        self.bus_scroll =
                            (self.bus_scroll + 1).min(self.bus_messages.len().saturating_sub(1));
                    }
                }
                KeyCode::Char('k') | KeyCode::Up => {
                    self.follow_mode = false;
                    if self.view == View::BusStream {
                        self.bus_scroll = self.bus_scroll.saturating_sub(1);
                    }
                }
                KeyCode::Char('G') => {
                    self.follow_mode = true;
                    self.bus_scroll = self.bus_messages.len().saturating_sub(1);
                }
                KeyCode::Char('g') => {
                    self.follow_mode = false;
                    self.bus_scroll = 0;
                }
                KeyCode::Char('f') => {
                    self.follow_mode = !self.follow_mode;
                    if self.follow_mode {
                        self.bus_scroll = self.bus_messages.len().saturating_sub(1);
                    }
                }
                _ => {}
            }
        }
    }

    /// Message sent from bus reader tasks to the TUI render loop.
    pub enum TuiMessage {
        BusEvent(BusEvent),
    }

    /// Spawn a bus listener that connects to a socket, registers as `tui`,
    /// subscribes to `*`, and forwards messages to the TUI channel.
    pub fn spawn_bus_listener(socket_path: String, tx: mpsc::UnboundedSender<TuiMessage>) {
        tokio::spawn(async move {
            use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
            use tokio::net::UnixStream;

            let stream = match UnixStream::connect(&socket_path).await {
                Ok(s) => s,
                Err(e) => {
                    tracing::warn!(socket = %socket_path, error = %e, "TUI: failed to connect to bus");
                    return;
                }
            };

            let (reader, mut writer) = stream.into_split();

            // Register with wildcard subscription.
            let reg = serde_json::json!({
                "type": "register",
                "name": "tui",
                "subscriptions": ["*"]
            });
            let mut line = serde_json::to_string(&reg).unwrap();
            line.push('\n');
            if let Err(e) = writer.write_all(line.as_bytes()).await {
                tracing::warn!(error = %e, "TUI: failed to register on bus");
                return;
            }

            let mut lines = BufReader::new(reader).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                let trimmed = line.trim();
                if trimmed.is_empty() {
                    continue;
                }
                if let Ok(v) = serde_json::from_str::<serde_json::Value>(trimmed) {
                    // Skip non-message envelopes (register acks, list responses).
                    if v.get("type").and_then(|t| t.as_str()) != Some("message") {
                        continue;
                    }
                    let source = v
                        .get("source")
                        .and_then(|s| s.as_str())
                        .unwrap_or("?")
                        .to_string();
                    let target = v
                        .get("target")
                        .and_then(|t| t.as_str())
                        .unwrap_or("?")
                        .to_string();
                    let payload = v.get("payload").cloned().unwrap_or_default();
                    let preview = payload_preview(&payload);
                    let timestamp = chrono::Local::now().format("%H:%M:%S").to_string();

                    let event = BusEvent {
                        timestamp,
                        source,
                        target,
                        payload_preview: preview,
                    };
                    if tx.send(TuiMessage::BusEvent(event)).is_err() {
                        break; // TUI closed
                    }
                }
            }
        });
    }

    /// Truncated preview of a bus message payload.
    fn payload_preview(payload: &serde_json::Value) -> String {
        if let Some(task) = payload.get("task").and_then(|t| t.as_str()) {
            return truncate(task, 80);
        }
        if let Some(text) = payload.get("text").and_then(|t| t.as_str()) {
            return truncate(text, 80);
        }
        let s = payload.to_string();
        truncate(&s, 80)
    }

    fn truncate(s: &str, max: usize) -> String {
        let clean: String = s.chars().filter(|c| !c.is_control()).collect();
        if clean.len() <= max {
            clean
        } else {
            format!("{}…", &clean[..max])
        }
    }

    /// Refresh agent info from state files on disk.
    pub fn refresh_agents() -> Vec<AgentInfo> {
        let state_dir = crate::infra::paths::state_dir();
        let mut infos = Vec::new();
        if let Ok(entries) = std::fs::read_dir(&state_dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.extension().and_then(|e| e.to_str()) != Some("yaml") {
                    continue;
                }
                if let Ok(contents) = std::fs::read_to_string(&path)
                    && let Ok(state) =
                        serde_yaml::from_str::<crate::app::agent::AgentState>(&contents)
                {
                    infos.push(AgentInfo {
                        name: state.config.name,
                        status: state.status,
                        current_task: state.current_task,
                        cost_usd: state.total_cost,
                        turns: state.total_turns,
                    });
                }
            }
        }
        infos.sort_by(|a, b| a.name.cmp(&b.name));
        infos
    }

    /// Refresh task queue summary from the store.
    pub fn refresh_tasks(store: &dyn TaskReader) -> (usize, usize, usize) {
        let pending = store
            .list(Some(TaskStatus::Pending))
            .map(|v| v.len())
            .unwrap_or(0);
        let active = store
            .list(Some(TaskStatus::Active))
            .map(|v| v.len())
            .unwrap_or(0);
        let done = store
            .list(Some(TaskStatus::Done))
            .map(|v| v.len())
            .unwrap_or(0);
        (pending, active, done)
    }

    /// Refresh workflow (SM instance) summary from the store.
    pub fn refresh_workflows(store: &dyn StateMachineReader) -> Vec<(String, String, String)> {
        store
            .list_all()
            .unwrap_or_default()
            .into_iter()
            .filter(|inst| {
                // Only show non-terminal (active) instances.
                !["done", "merged", "cancelled", "rejected", "closed"]
                    .contains(&inst.state.as_str())
            })
            .map(|inst| {
                let short_id = if inst.id.len() > 12 {
                    format!("{}…", &inst.id[..12])
                } else {
                    inst.id.clone()
                };
                (short_id, inst.model, inst.state)
            })
            .collect()
    }

    // ─── Rendering ─────────────────────────────────────────────────────────────

    fn draw(frame: &mut ratatui::Frame, app: &App) {
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Length(3), Constraint::Min(0)])
            .split(frame.area());

        draw_tabs(frame, app, chunks[0]);

        match app.view {
            View::Dashboard => draw_dashboard(frame, app, chunks[1]),
            View::BusStream => draw_bus_stream(frame, app, chunks[1]),
        }
    }

    fn draw_tabs(frame: &mut ratatui::Frame, app: &App, area: Rect) {
        let titles: Vec<Line> = vec![Line::from(" 1 Dashboard "), Line::from(" 2 Bus Stream ")];
        let selected = match app.view {
            View::Dashboard => 0,
            View::BusStream => 1,
        };
        let tabs = Tabs::new(titles)
            .block(Block::default().borders(Borders::ALL).title(" deskd TUI "))
            .select(selected)
            .style(Style::default().fg(Color::Gray))
            .highlight_style(
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            );
        frame.render_widget(tabs, area);
    }

    fn draw_dashboard(frame: &mut ratatui::Frame, app: &App, area: Rect) {
        // 2x2 grid: top row (agents | tasks), bottom row (workflows | bus tail)
        let rows = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
            .split(area);

        let top = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
            .split(rows[0]);

        let bottom = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
            .split(rows[1]);

        draw_agents_panel(frame, app, top[0]);
        draw_tasks_panel(frame, app, top[1]);
        draw_workflows_panel(frame, app, bottom[0]);
        draw_bus_tail_panel(frame, app, bottom[1]);
    }

    fn draw_agents_panel(frame: &mut ratatui::Frame, app: &App, area: Rect) {
        let items: Vec<ListItem> = app
            .agents
            .iter()
            .map(|a| {
                let status_color = match a.status.as_str() {
                    "working" => Color::Green,
                    "idle" => Color::DarkGray,
                    _ => Color::Yellow,
                };
                let task_preview = if a.current_task.is_empty() {
                    String::new()
                } else {
                    format!(" — {}", truncate(&a.current_task, 30))
                };
                ListItem::new(Line::from(vec![
                    Span::styled(
                        format!(" {:>8} ", a.status),
                        Style::default().fg(status_color),
                    ),
                    Span::raw(format!("{}{}", a.name, task_preview)),
                ]))
            })
            .collect();

        let list = List::new(items).block(
            Block::default()
                .borders(Borders::ALL)
                .title(format!(" Agents ({}) ", app.agents.len())),
        );
        frame.render_widget(list, area);
    }

    fn draw_tasks_panel(frame: &mut ratatui::Frame, app: &App, area: Rect) {
        let (pending, active, done) = app.task_summary;
        let text = vec![
            Line::from(vec![
                Span::styled(" pending  ", Style::default().fg(Color::Yellow)),
                Span::raw(format!("{}", pending)),
            ]),
            Line::from(vec![
                Span::styled(" active   ", Style::default().fg(Color::Green)),
                Span::raw(format!("{}", active)),
            ]),
            Line::from(vec![
                Span::styled(" done     ", Style::default().fg(Color::DarkGray)),
                Span::raw(format!("{}", done)),
            ]),
        ];
        let para = Paragraph::new(text)
            .block(Block::default().borders(Borders::ALL).title(" Task Queue "));
        frame.render_widget(para, area);
    }

    fn draw_workflows_panel(frame: &mut ratatui::Frame, app: &App, area: Rect) {
        let items: Vec<ListItem> = app
            .workflow_summary
            .iter()
            .map(|(id, model, state)| {
                ListItem::new(Line::from(vec![
                    Span::styled(format!(" {:<14}", state), Style::default().fg(Color::Cyan)),
                    Span::raw(format!("{} ({})", id, model)),
                ]))
            })
            .collect();

        let list = List::new(items).block(
            Block::default()
                .borders(Borders::ALL)
                .title(format!(" Workflows ({}) ", app.workflow_summary.len())),
        );
        frame.render_widget(list, area);
    }

    fn draw_bus_tail_panel(frame: &mut ratatui::Frame, app: &App, area: Rect) {
        let inner_height = area.height.saturating_sub(2) as usize;
        let start = app.bus_messages.len().saturating_sub(inner_height);
        let items: Vec<ListItem> = app
            .bus_messages
            .iter()
            .skip(start)
            .take(inner_height)
            .map(|e| bus_event_to_list_item(e))
            .collect();

        let list = List::new(items).block(
            Block::default()
                .borders(Borders::ALL)
                .title(" Bus (recent) "),
        );
        frame.render_widget(list, area);
    }

    fn draw_bus_stream(frame: &mut ratatui::Frame, app: &App, area: Rect) {
        let inner_height = area.height.saturating_sub(2) as usize;
        let total = app.bus_messages.len();

        // Compute visible window based on scroll position.
        let end = if app.follow_mode {
            total
        } else {
            (app.bus_scroll + 1).min(total)
        };
        let start = end.saturating_sub(inner_height);

        let items: Vec<ListItem> = app
            .bus_messages
            .iter()
            .skip(start)
            .take(end - start)
            .map(|e| bus_event_to_list_item(e))
            .collect();

        let follow_indicator = if app.follow_mode { " [FOLLOW] " } else { "" };
        let title = format!(
            " Bus Stream ({}/{}) {}",
            end.min(total),
            total,
            follow_indicator
        );

        let list = List::new(items).block(Block::default().borders(Borders::ALL).title(title));
        frame.render_widget(list, area);
    }

    fn bus_event_to_list_item(e: &BusEvent) -> ListItem<'_> {
        ListItem::new(Line::from(vec![
            Span::styled(
                format!("{} ", e.timestamp),
                Style::default().fg(Color::DarkGray),
            ),
            Span::styled(format!("{} ", e.source), Style::default().fg(Color::Cyan)),
            Span::styled("→ ", Style::default().fg(Color::DarkGray)),
            Span::styled(format!("{} ", e.target), Style::default().fg(Color::Yellow)),
            Span::raw(&e.payload_preview),
        ]))
    }

    // ─── Main loop ─────────────────────────────────────────────────────────────

    /// Run the TUI event loop. Returns `true` if the user pressed `Q` (quit all).
    pub async fn run(
        bus_sockets: Vec<String>,
        task_store: &dyn TaskReader,
        sm_store: &dyn StateMachineReader,
    ) -> io::Result<bool> {
        enable_raw_mode()?;
        let mut stdout = io::stdout();
        crossterm::execute!(stdout, EnterAlternateScreen)?;
        let backend = CrosstermBackend::new(stdout);
        let mut terminal = Terminal::new(backend)?;

        let mut app = App::new();

        // Channel for bus events from listener tasks.
        let (tx, mut rx) = mpsc::unbounded_channel::<TuiMessage>();

        // Spawn a bus listener for each socket.
        for socket in &bus_sockets {
            spawn_bus_listener(socket.clone(), tx.clone());
        }
        drop(tx); // Drop our copy so the channel closes when all listeners exit.

        let tick_rate = Duration::from_millis(250);
        let mut last_refresh = Instant::now() - Duration::from_secs(10); // force initial refresh

        loop {
            // Periodic refresh of agents, tasks, workflows (every 2 seconds).
            if last_refresh.elapsed() >= Duration::from_secs(2) {
                app.agents = refresh_agents();
                app.task_summary = refresh_tasks(task_store);
                app.workflow_summary = refresh_workflows(sm_store);
                last_refresh = Instant::now();
            }

            // Drain bus events.
            while let Ok(msg) = rx.try_recv() {
                match msg {
                    TuiMessage::BusEvent(e) => app.push_bus_event(e),
                }
            }

            terminal.draw(|f| draw(f, &app))?;

            // Poll for keyboard input with tick timeout.
            if event::poll(tick_rate)?
                && let Event::Key(key) = event::read()?
            {
                app.handle_key(key);
            }

            if app.should_quit || app.should_quit_all {
                break;
            }
        }

        disable_raw_mode()?;
        crossterm::execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
        terminal.show_cursor()?;

        Ok(app.should_quit_all)
    }
}
