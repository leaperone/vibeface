use crate::terminal::{NodeStatus, RunState, age, safe_text, shorten, use_color};
use anyhow::{Context, Result, bail, ensure};
use crossterm::{
    cursor::{Hide, Show},
    event::{Event, EventStream, KeyCode, KeyEvent, KeyEventKind, KeyModifiers},
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use futures_util::StreamExt;
use ratatui::{
    Frame, Terminal,
    backend::CrosstermBackend,
    layout::{Constraint, Layout, Margin, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span, Text},
    widgets::{List, ListItem, ListState, Paragraph, Wrap},
};
use std::{
    io::{self, IsTerminal},
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicU8, Ordering},
    },
    time::{Duration, Instant},
};
use tokio::signal::unix::{SignalKind, signal};
use vibeface_control::ControlRequest;

const REFRESH_INTERVAL: Duration = Duration::from_secs(3);
const RAW: u8 = 1;
const ALTERNATE: u8 = 2;
const HIDDEN: u8 = 4;

struct TerminalGuard(Arc<AtomicU8>);

impl TerminalGuard {
    fn enter() -> Result<Self> {
        let guard = Self(Arc::new(AtomicU8::new(0)));
        let flags = Arc::clone(&guard.0);
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            restore(&flags);
            previous(info);
        }));
        enable_raw_mode().context("enable terminal raw mode")?;
        guard.0.fetch_or(RAW, Ordering::SeqCst);
        execute!(io::stdout(), EnterAlternateScreen).context("enter terminal alternate screen")?;
        guard.0.fetch_or(ALTERNATE, Ordering::SeqCst);
        execute!(io::stdout(), Hide).context("hide terminal cursor")?;
        guard.0.fetch_or(HIDDEN, Ordering::SeqCst);
        Ok(guard)
    }
}

fn restore(flags: &AtomicU8) {
    let flags = flags.swap(0, Ordering::SeqCst);
    if flags & HIDDEN != 0 {
        let _ = execute!(io::stdout(), Show);
    }
    if flags & ALTERNATE != 0 {
        let _ = execute!(io::stdout(), LeaveAlternateScreen);
    }
    if flags & RAW != 0 {
        let _ = disable_raw_mode();
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        restore(&self.0);
    }
}

struct Snapshot {
    node: NodeStatus,
    at: Instant,
}

enum Reading {
    Loading,
    Fresh(Snapshot),
    Failed {
        last: Option<Snapshot>,
        error: String,
    },
}

impl Reading {
    fn snapshot(&self) -> Option<&Snapshot> {
        match self {
            Self::Loading => None,
            Self::Fresh(snapshot) => Some(snapshot),
            Self::Failed { last, .. } => last.as_ref(),
        }
    }

    fn error(&self) -> Option<&str> {
        match self {
            Self::Failed { error, .. } => Some(error),
            _ => None,
        }
    }

    fn state_label(&self) -> &'static str {
        match self {
            Self::Loading => "LOADING",
            Self::Fresh(snapshot) => snapshot.node.status.label(),
            Self::Failed { last: Some(_), .. } => "STALE",
            Self::Failed { last: None, .. } => "UNAVAILABLE",
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum View {
    Network,
    Details,
}

struct App {
    reading: Reading,
    selected_device: Option<String>,
    list: ListState,
    view: View,
    detail_scroll: u16,
    help: bool,
    help_scroll: u16,
}

impl App {
    fn new() -> Self {
        Self {
            reading: Reading::Loading,
            selected_device: None,
            list: ListState::default(),
            view: View::Network,
            detail_scroll: 0,
            help: false,
            help_scroll: 0,
        }
    }

    fn devices(&self) -> Vec<&str> {
        self.reading.snapshot().map_or_else(Vec::new, |snapshot| {
            std::iter::once(snapshot.node.device_id.as_str())
                .chain(
                    snapshot
                        .node
                        .peers
                        .iter()
                        .flatten()
                        .map(|peer| peer.device_id.as_str()),
                )
                .collect()
        })
    }

    fn selected_index(&self) -> usize {
        self.devices()
            .iter()
            .position(|id| Some(*id) == self.selected_device.as_deref())
            .unwrap_or(0)
    }

    fn finish_reading(&mut self, result: Result<NodeStatus>) {
        let previous_index = self.selected_index();
        let previous = std::mem::replace(&mut self.reading, Reading::Loading);
        self.reading = match result {
            Ok(node) => Reading::Fresh(Snapshot {
                node,
                at: Instant::now(),
            }),
            Err(error) => {
                let last = match previous {
                    Reading::Fresh(snapshot) => Some(snapshot),
                    Reading::Failed { last, .. } => last,
                    Reading::Loading => None,
                };
                Reading::Failed {
                    last,
                    error: format!("{error:#}"),
                }
            }
        };
        let devices = self.devices();
        if !devices
            .iter()
            .any(|id| Some(*id) == self.selected_device.as_deref())
        {
            self.selected_device = devices
                .get(previous_index.min(devices.len().saturating_sub(1)))
                .map(|id| (*id).to_owned());
            self.detail_scroll = 0;
        }
    }

    fn select(&mut self, offset: isize) {
        let devices = self.devices();
        if devices.is_empty() {
            return;
        }
        let index = self
            .selected_index()
            .saturating_add_signed(offset)
            .min(devices.len() - 1);
        self.selected_device = Some(devices[index].to_owned());
        self.detail_scroll = 0;
    }

    fn key(&mut self, key: KeyEvent) -> Input {
        if key.kind == KeyEventKind::Release {
            return Input::None;
        }
        let pressed = key.kind == KeyEventKind::Press;
        if key.code == KeyCode::Char('q')
            || (key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL))
        {
            return Input::Quit;
        }
        match key.code {
            KeyCode::Esc if pressed => {
                if self.help {
                    self.help = false;
                } else if self.view == View::Details {
                    self.view = View::Network;
                } else {
                    return Input::Quit;
                }
            }
            KeyCode::Char('?') if pressed => {
                self.help = !self.help;
                self.help_scroll = 0;
            }
            KeyCode::Char('r') if pressed => return Input::Refresh,
            KeyCode::Enter if pressed && !self.help => {
                self.view = View::Details;
                self.detail_scroll = 0;
            }
            KeyCode::Up | KeyCode::Char('k') => self.move_by(-1),
            KeyCode::Down | KeyCode::Char('j') => self.move_by(1),
            KeyCode::PageUp => self.move_by(-8),
            KeyCode::PageDown => self.move_by(8),
            KeyCode::Home => self.move_by(isize::MIN),
            KeyCode::End => self.move_by(isize::MAX),
            _ => {}
        }
        Input::None
    }

    fn move_by(&mut self, offset: isize) {
        if self.help {
            self.help_scroll = usize::from(self.help_scroll)
                .saturating_add_signed(offset)
                .min(u16::MAX as usize) as u16;
        } else if self.view == View::Details {
            self.detail_scroll = usize::from(self.detail_scroll)
                .saturating_add_signed(offset)
                .min(u16::MAX as usize) as u16;
        } else {
            self.select(offset);
        }
    }
}

#[derive(PartialEq, Eq, Debug)]
enum Input {
    None,
    Quit,
    Refresh,
}

async fn inspect(core: PathBuf, data_dir: PathBuf) -> Result<NodeStatus> {
    let value = vibeface_control::request(&core, &ControlRequest::Inspect { data_dir }).await?;
    NodeStatus::parse(value)
}

pub async fn run(data_dir: PathBuf) -> Result<()> {
    ensure!(
        io::stdin().is_terminal() && io::stdout().is_terminal(),
        "tui requires an interactive terminal; use vibeface status or status --json"
    );
    ensure!(
        std::env::var("TERM").is_ok_and(|term| !term.is_empty() && term != "dumb"),
        "tui requires a capable terminal; use vibeface status or status --json"
    );
    let core = crate::core_binary()?;
    let mut terminate = signal(SignalKind::terminate())?;
    let mut hangup = signal(SignalKind::hangup())?;
    let mut interrupt = signal(SignalKind::interrupt())?;
    let mut suspend = signal(SignalKind::from_raw(libc::SIGTSTP))?;
    let guard = TerminalGuard::enter()?;
    let mut terminal = Terminal::new(CrosstermBackend::new(io::stdout()))?;
    let mut events = EventStream::new();
    let mut app = App::new();
    let palette = Palette::new(use_color());
    let mut reading = Box::pin(inspect(core.clone(), data_dir.clone()));
    let mut reading_active = true;
    let mut refresh = tokio::time::interval(REFRESH_INTERVAL);
    refresh.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut clock = tokio::time::interval(Duration::from_secs(1));
    clock.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let exit_signal = loop {
        terminal.draw(|frame| draw(frame, &mut app, reading_active, palette))?;
        tokio::select! {
            biased;
            _ = terminate.recv() => break Some(libc::SIGTERM),
            _ = hangup.recv() => break Some(libc::SIGHUP),
            _ = interrupt.recv() => break Some(libc::SIGINT),
            _ = suspend.recv() => {},
            event = events.next() => {
                match event.context("terminal input ended")?? {
                    Event::Key(key) => match app.key(key) {
                        Input::Quit => break None,
                        Input::Refresh if !reading_active => {
                            reading = Box::pin(inspect(core.clone(), data_dir.clone()));
                            reading_active = true;
                        }
                        _ => {},
                    },
                    Event::Resize(_, _) => {},
                    _ => {},
                }
            }
            result = &mut reading, if reading_active => {
                reading_active = false;
                app.finish_reading(result);
            }
            _ = refresh.tick() => {
                if !reading_active {
                    reading = Box::pin(inspect(core.clone(), data_dir.clone()));
                    reading_active = true;
                }
            }
            _ = clock.tick() => {},
        }
    };
    // Dropping the read cancels only this UI's kill-on-drop control helper.
    drop(reading);
    drop(terminal);
    drop(guard);
    if let Some(signal) = exit_signal {
        unsafe {
            libc::signal(signal, libc::SIG_DFL);
            libc::raise(signal);
        }
        bail!("terminal interrupted by signal {signal}");
    }
    Ok(())
}

#[derive(Clone, Copy)]
struct Palette {
    accent: Style,
    strong: Style,
    secondary: Style,
    attention: Style,
}

impl Palette {
    fn new(color: bool) -> Self {
        if color {
            Self {
                accent: Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
                strong: Style::default().add_modifier(Modifier::BOLD),
                secondary: Style::default().add_modifier(Modifier::DIM),
                attention: Style::default().add_modifier(Modifier::BOLD),
            }
        } else {
            Self {
                accent: Style::default(),
                strong: Style::default(),
                secondary: Style::default(),
                attention: Style::default(),
            }
        }
    }
}

fn draw(frame: &mut Frame<'_>, app: &mut App, refreshing: bool, palette: Palette) {
    let area = frame.area();
    if area.width < 32 || area.height < 14 {
        frame.render_widget(
            Paragraph::new("Window too small.\nEnlarge to 32 x 14.\nq quit")
                .wrap(Wrap { trim: false }),
            area,
        );
        return;
    }
    let area = area.inner(Margin {
        horizontal: if area.width >= 60 { 2 } else { 1 },
        vertical: 1,
    });
    let compact = area.height < 20;
    let [header, section, content, update, keys] = Layout::vertical([
        Constraint::Length(if compact { 2 } else { 3 }),
        Constraint::Length(if compact { 1 } else { 2 }),
        Constraint::Min(1),
        Constraint::Length(3),
        Constraint::Length(if area.width < 58 { 2 } else { 1 }),
    ])
    .areas(area);
    let [brand, state] =
        Layout::horizontal([Constraint::Min(12), Constraint::Length(13)]).areas(Rect {
            height: 1,
            ..header
        });
    frame.render_widget(Paragraph::new("vibeface").style(palette.accent), brand);
    let stale = matches!(app.reading, Reading::Failed { .. });
    frame.render_widget(
        Paragraph::new(app.reading.state_label())
            .right_aligned()
            .style(if stale {
                palette.attention
            } else {
                palette.strong
            }),
        state,
    );
    if let Some(snapshot) = app.reading.snapshot() {
        frame.render_widget(
            Paragraph::new(format!(
                "This node · {}",
                shorten(&snapshot.node.device_id, 16)
            ))
            .style(palette.secondary),
            Rect {
                y: header.y + 1,
                height: 1,
                ..header
            },
        );
    }
    let section_title = if app.help {
        "Keyboard"
    } else if app.view == View::Details {
        "< Network / Details"
    } else if stale {
        "Network · last known result"
    } else {
        "Network"
    };
    frame.render_widget(
        Paragraph::new(vec![
            Line::styled(section_title, palette.strong),
            Line::styled("─".repeat(section.width as usize), palette.secondary),
        ]),
        section,
    );
    if app.help {
        let help = Text::from(vec![
            Line::raw("↑/↓  j/k     Select or scroll"),
            Line::raw("PageUp/Down   Move by 8 rows"),
            Line::raw("Home/End      First or last row"),
            Line::raw("Enter         Full details"),
            Line::raw("Esc           Back, then quit"),
            Line::raw("r             Refresh status"),
            Line::raw("?             Toggle this help"),
            Line::raw("q  Ctrl-C     Quit"),
            Line::raw(""),
            Line::raw("Read-only. Refreshes every 3s."),
            Line::raw("Ctrl-Z is ignored. Use q to return to your shell."),
        ]);
        render_scrolled(frame, content, help, &mut app.help_scroll);
    } else if app.view == View::Details {
        let text = details(app, palette);
        render_scrolled(frame, content, text, &mut app.detail_scroll);
    } else if app.reading.snapshot().is_some() {
        let wide = content.width >= 84 && content.height >= 12;
        if wide {
            let [network, _, detail] = Layout::horizontal([
                Constraint::Min(36),
                Constraint::Length(4),
                Constraint::Length(34),
            ])
            .areas(content);
            let text = compact_details(app, detail.width, palette);
            draw_network(frame, network, app, palette);
            frame.render_widget(Paragraph::new(text).wrap(Wrap { trim: false }), detail);
        } else {
            draw_network(frame, content, app, palette);
        }
    } else {
        let text = if let Some(error) = app.reading.error() {
            format!(
                "Status unavailable\n\n{}\n\nCheck the data directory. Press r to retry.",
                safe_text(error)
            )
        } else {
            "Reading Node status…\n\nOnly existing Node and peer information is read.".into()
        };
        frame.render_widget(Paragraph::new(text).wrap(Wrap { trim: false }), content);
    }
    let mut update_lines = Vec::new();
    if let Some(error) = app.reading.error() {
        update_lines.push(Line::styled(
            "Refresh failed. Data is not current.",
            palette.attention,
        ));
        update_lines.push(Line::raw(safe_text(error)));
    }
    if let Some(snapshot) = app.reading.snapshot() {
        let age = age(snapshot.at.elapsed().as_secs());
        let prefix = if stale {
            "Last successful update"
        } else {
            "Updated"
        };
        update_lines.push(Line::styled(
            format!(
                "{prefix} {age} ago{}",
                if refreshing { " · refreshing" } else { "" }
            ),
            palette.secondary,
        ));
    } else if refreshing {
        update_lines.push(Line::styled("Reading status…", palette.secondary));
    }
    frame.render_widget(Paragraph::new(update_lines), update);
    let keys_text = if app.help {
        if keys.width < 58 {
            "↑/↓ scroll  Esc back\nq quit"
        } else {
            "↑/↓ scroll   Esc back   ? close   q quit"
        }
    } else if app.view == View::Details {
        if keys.width < 58 {
            "↑/↓ scroll  Esc back\nr refresh  ? help  q quit"
        } else {
            "↑/↓ scroll   Esc back   r refresh   ? help   q quit"
        }
    } else if keys.width < 58 {
        "j/k select  Enter details\nr refresh  ? help  q quit"
    } else {
        "j/k select   Enter details   r refresh   ? help   q quit"
    };
    frame.render_widget(Paragraph::new(keys_text).style(palette.secondary), keys);
}

fn draw_network(frame: &mut Frame<'_>, area: Rect, app: &mut App, palette: Palette) {
    let Some(snapshot) = app.reading.snapshot() else {
        return;
    };
    let node = &snapshot.node;
    let compact = area.height < 7;
    let [heading, list, summary] = Layout::vertical([
        Constraint::Length(if compact { 1 } else { 2 }),
        Constraint::Min(1),
        Constraint::Length(if compact { 1 } else { 2 }),
    ])
    .areas(area);
    frame.render_widget(
        Paragraph::new("Personal network").style(palette.secondary),
        heading,
    );
    let width = list.width.saturating_sub(2) as usize;
    let mut items = vec![network_item(
        "This node",
        "local",
        &format!("{} · {}", node.writes(), node.listen),
        width,
        palette,
    )];
    if let Some(peers) = &node.peers {
        for peer in peers {
            items.push(network_item(
                &peer.name,
                peer.reachability(),
                &format!(
                    "{} · {}",
                    peer.path_label(),
                    peer.last_sync(crate::now_ms())
                ),
                width,
                palette,
            ));
        }
    }
    let selected = app.selected_index();
    app.list.select(Some(selected));
    frame.render_stateful_widget(
        List::new(items)
            .highlight_symbol("> ")
            .highlight_style(palette.strong.remove_modifier(Modifier::DIM)),
        list,
        &mut app.list,
    );
    let note = if node.status == RunState::Stopped {
        "Start with vibeface --data-dir <DATA_DIR>."
    } else if node.peers.as_ref().is_some_and(Vec::is_empty) {
        "No peers yet. See vibeface join --help."
    } else {
        ""
    };
    frame.render_widget(
        Paragraph::new(vec![
            Line::styled(node.peer_summary(), palette.secondary),
            Line::raw(note),
        ]),
        summary,
    );
}

fn network_item(
    name: &str,
    state: &str,
    description: &str,
    width: usize,
    palette: Palette,
) -> ListItem<'static> {
    let name_width = width.saturating_sub(state.len() + 2);
    let name = shorten(name, name_width);
    let padding = name_width.saturating_sub(Span::raw(name.as_str()).width());
    ListItem::new(vec![
        Line::from(vec![
            Span::styled(format!("{name}{}  ", " ".repeat(padding)), palette.strong),
            Span::raw(state.to_owned()),
        ]),
        Line::styled(shorten(description, width), palette.secondary),
    ])
}

fn details(app: &App, palette: Palette) -> Text<'static> {
    let mut lines = Vec::new();
    let mut field = |label: &str, value: &str| {
        lines.push(Line::styled(label.to_owned(), palette.secondary));
        lines.push(Line::raw(safe_text(value)));
        lines.push(Line::raw(""));
    };
    if let Some(snapshot) = app.reading.snapshot() {
        let node = &snapshot.node;
        if app.selected_device.as_deref() == Some(node.device_id.as_str()) {
            field("THIS NODE", "Local device");
            field("Peer ID", &node.device_id);
            field(
                if app.reading.error().is_some() {
                    "Last known status"
                } else {
                    "Status"
                },
                node.status.label(),
            );
            field("Writes", node.writes());
            field("Listen", &node.listen.to_string());
            field(
                "Web",
                if node.web_enabled {
                    "enabled"
                } else {
                    "disabled"
                },
            );
            field("Version", node.version());
            field("Data", &node.data_dir.to_string_lossy());
            if let Some(error) = &node.error {
                field("Node detail", error);
            }
        } else if let Some(peer) = node
            .peers
            .iter()
            .flatten()
            .find(|peer| Some(peer.device_id.as_str()) == app.selected_device.as_deref())
        {
            field("PEER", &peer.name);
            field("Peer ID", &peer.device_id);
            field("Connection", peer.reachability());
            field("Path", peer.path_label());
            field("Last sync", &peer.last_sync(crate::now_ms()));
        }
    }
    if let Some(error) = app.reading.error() {
        field("Refresh failed. Last result is not current.", error);
    }
    if lines.is_empty() {
        lines.push(Line::raw("Reading Node status…"));
    }
    Text::from(lines)
}

fn compact_details(app: &App, width: u16, palette: Palette) -> Text<'static> {
    let Some(snapshot) = app.reading.snapshot() else {
        return Text::default();
    };
    let node = &snapshot.node;
    let stale = app.reading.error().is_some();
    let (heading, fields) = if app.selected_device.as_deref() == Some(node.device_id.as_str()) {
        (
            if stale {
                "THIS NODE · last result".into()
            } else {
                "THIS NODE".into()
            },
            vec![
                ("State", node.status.label().to_owned()),
                ("Writes", node.writes().to_owned()),
                (
                    "Web",
                    if node.web_enabled {
                        "enabled"
                    } else {
                        "disabled"
                    }
                    .to_owned(),
                ),
                ("Listen", node.listen.to_string()),
                ("Version", node.version().to_owned()),
                ("ID", node.device_id.clone()),
                ("Data", node.data_dir.to_string_lossy().into_owned()),
            ],
        )
    } else if let Some(peer) = node
        .peers
        .iter()
        .flatten()
        .find(|peer| Some(peer.device_id.as_str()) == app.selected_device.as_deref())
    {
        (
            shorten(&peer.name, width.into()),
            vec![
                ("State", peer.reachability().to_owned()),
                ("Path", peer.path_label().to_owned()),
                ("Sync", peer.last_sync(crate::now_ms())),
                ("ID", peer.device_id.clone()),
            ],
        )
    } else {
        return Text::default();
    };
    let mut lines = vec![Line::styled(heading, palette.strong), Line::raw("")];
    for (label, value) in fields {
        lines.push(Line::from(vec![
            Span::styled(format!("{label:<8} "), palette.secondary),
            Span::raw(shorten(&value, width.saturating_sub(9).into())),
        ]));
    }
    lines.extend([
        Line::raw(""),
        Line::styled("Enter full details", palette.secondary),
    ]);
    Text::from(lines)
}

fn render_scrolled(frame: &mut Frame<'_>, area: Rect, text: Text<'_>, scroll: &mut u16) {
    let paragraph = Paragraph::new(text).wrap(Wrap { trim: false });
    let max_scroll = paragraph
        .line_count(area.width)
        .saturating_sub(area.height.into())
        .min(u16::MAX.into()) as u16;
    *scroll = (*scroll).min(max_scroll);
    frame.render_widget(paragraph.scroll((*scroll, 0)), area);
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::backend::TestBackend;
    use serde_json::json;

    fn node() -> NodeStatus {
        NodeStatus::parse(json!({
            "status":"running", "device_id":"local-device-id", "data_dir":"/node/long directory/设备", "listen":"127.0.0.1:4790",
            "web_enabled":true, "core_version":"0.0.1", "write_state":"writable",
            "peers":[
                {"device_id":"peer-a", "name":"设备 e\u{301} 👩‍💻 \u{1b}]52;c;secret\u{7}\u{202e}extra long name", "path":"direct", "reachable":true, "synced_ms":1},
                {"device_id":"peer-b", "name":"Build", "path":null, "reachable":null, "synced_ms":null}
            ]
        })).unwrap()
    }

    fn render(app: &mut App, width: u16, height: u16) -> String {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal
            .draw(|frame| draw(frame, app, false, Palette::new(false)))
            .unwrap();
        let buffer = terminal.backend().buffer();
        buffer
            .content
            .chunks(width as usize)
            .map(|row| row.iter().map(|cell| cell.symbol()).collect::<String>())
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn network_renders_wide_narrow_and_tiny_without_untrusted_controls() {
        let mut app = App::new();
        assert!(render(&mut app, 45, 24).contains("Reading Node status"));
        app.finish_reading(Ok(node()));
        let wide = render(&mut app, 100, 28);
        assert!(
            wide.lines()
                .any(|line| line.contains("Web") && line.contains("enabled"))
        );
        assert!(wide.contains("Enter full details"));
        for (width, height) in [(100, 28), (45, 24), (32, 14)] {
            let output = render(&mut app, width, height);
            assert!(output.contains("RUNNING"), "{width}x{height}: {output}");
            assert!(output.contains("q quit"), "{width}x{height}: {output}");
            assert!(!output.contains('\u{1b}'));
            assert!(!output.contains('\u{202e}'));
            if height > 14 {
                assert!(output.contains("not tried yet"));
                assert!(output.contains("2 peers"));
            }
        }
        assert!(render(&mut app, 32, 12).contains("Window too small"));
        assert!(render(&mut app, 20, 6).contains("q quit"));
        for width in 32..=110 {
            for view in [View::Network, View::Details] {
                app.view = view;
                assert!(render(&mut app, width, 24).contains("q quit"), "{width}");
                app.help = true;
                assert!(
                    render(&mut app, width, 24).contains("q quit"),
                    "help {width}"
                );
                app.help = false;
            }
        }
        app.selected_device = Some("peer-a".into());
        app.view = View::Details;
        let output = render(&mut app, 45, 24);
        assert!(output.contains("peer-a"));
        assert!(output.contains("\\u{1b}"));
        app.detail_scroll = u16::MAX;
        assert!(render(&mut app, 45, 24).contains("Last sync"));
        assert!(render(&mut app, 45, 24).contains("direct"));
    }

    #[test]
    fn failed_refresh_retains_stale_data_and_device_selection_survives_reordering() {
        let mut app = App::new();
        app.finish_reading(Err(anyhow::anyhow!("first read failed")));
        assert!(app.reading.snapshot().is_none());
        assert_eq!(app.reading.state_label(), "UNAVAILABLE");
        app.finish_reading(Ok(node()));
        app.select(2);
        assert_eq!(app.selected_device.as_deref(), Some("peer-b"));
        let mut reordered = node();
        reordered.peers.as_mut().unwrap().reverse();
        app.finish_reading(Ok(reordered));
        assert_eq!(app.selected_device.as_deref(), Some("peer-b"));
        assert_eq!(app.selected_index(), 1);
        app.finish_reading(Err(anyhow::anyhow!("read failed\u{1b}[2J")));
        assert!(app.reading.snapshot().is_some());
        assert_eq!(app.reading.state_label(), "STALE");
        let output = render(&mut app, 100, 28);
        assert!(output.contains("last known result"));
        assert!(output.contains("Refresh failed"));
        assert!(output.contains("Last successful update"));
        assert!(output.contains("STALE"));
        app.selected_device = Some("local-device-id".into());
        app.view = View::Details;
        assert!(render(&mut app, 100, 28).contains("Last known status"));
        assert!(!output.contains('\u{1b}'));
    }

    #[test]
    fn key_release_does_not_navigate_and_repeats_do_not_retrigger_actions() {
        let mut app = App::new();
        app.finish_reading(Ok(node()));
        let key = |code, kind| KeyEvent::new_with_kind(code, KeyModifiers::NONE, kind);
        app.key(key(KeyCode::Down, KeyEventKind::Release));
        assert_eq!(app.selected_index(), 0);
        app.key(key(KeyCode::Down, KeyEventKind::Repeat));
        assert_eq!(app.selected_index(), 1);
        assert_eq!(
            app.key(key(KeyCode::Char('r'), KeyEventKind::Repeat)),
            Input::None
        );
        assert_eq!(
            app.key(key(KeyCode::Char('r'), KeyEventKind::Press)),
            Input::Refresh
        );
        app.key(key(KeyCode::Enter, KeyEventKind::Repeat));
        assert!(app.view == View::Network);
        app.key(key(KeyCode::Enter, KeyEventKind::Press));
        assert!(app.view == View::Details);
    }
}
