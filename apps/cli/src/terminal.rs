use anyhow::{Context, Result, ensure};
use clap::builder::styling::Styles;
use crossterm::style::Stylize;
use ratatui::{style::Style, text::Span};
use serde::Deserialize;
use serde_json::Value;
use std::{
    io::{self, Write},
    net::SocketAddr,
    path::PathBuf,
};

pub fn help_styles() -> Styles {
    let strong = clap::builder::styling::Style::new().bold();
    Styles::styled()
        .header(strong)
        .usage(strong)
        .literal(strong)
        .placeholder(clap::builder::styling::Style::new())
}

pub fn use_emphasis() -> bool {
    std::env::var_os("NO_COLOR").is_none() && std::env::var("TERM").is_ok_and(|term| term != "dumb")
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RunState {
    Running,
    Stopped,
    Unavailable,
}

impl RunState {
    pub fn label(self) -> &'static str {
        match self {
            Self::Running => "RUNNING",
            Self::Stopped => "STOPPED",
            Self::Unavailable => "UNAVAILABLE",
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WriteState {
    Aligning,
    ReadOnly,
    Writable,
    Forked,
}

impl WriteState {
    pub fn label(self) -> &'static str {
        match self {
            Self::Aligning => "aligning",
            Self::ReadOnly => "read_only",
            Self::Writable => "writable",
            Self::Forked => "forked",
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PeerPath {
    Direct,
    Relay,
}

impl PeerPath {
    pub fn label(self) -> &'static str {
        match self {
            Self::Direct => "direct",
            Self::Relay => "relay",
        }
    }
}

#[derive(Clone, Debug, Deserialize)]
pub struct PeerStatus {
    pub device_id: String,
    pub name: String,
    pub path: Option<PeerPath>,
    pub reachable: Option<bool>,
    pub synced_ms: Option<i64>,
}

impl PeerStatus {
    pub fn reachability(&self) -> &'static str {
        match self.reachable {
            Some(true) => "reachable",
            Some(false) => "unreachable",
            None => "not tried yet",
        }
    }

    pub fn path_label(&self) -> &'static str {
        self.path.map_or("unknown path", PeerPath::label)
    }

    pub fn last_sync(&self, now_ms: i64) -> String {
        self.synced_ms.map_or_else(
            || "never synced".into(),
            |at| {
                format!(
                    "synced {} ago",
                    age(now_ms.saturating_sub(at).max(0) as u64 / 1000)
                )
            },
        )
    }
}

#[derive(Clone, Debug, Deserialize)]
pub struct NodeStatus {
    pub status: RunState,
    pub device_id: String,
    pub data_dir: PathBuf,
    pub listen: SocketAddr,
    pub web_enabled: bool,
    pub version: Option<String>,
    pub core_version: String,
    pub write_state: Option<WriteState>,
    pub peers: Option<Vec<PeerStatus>>,
    pub error: Option<String>,
}

impl NodeStatus {
    pub fn parse(value: Value) -> Result<Self> {
        let node: Self = serde_json::from_value(value).context("invalid Node status response")?;
        ensure!(!node.device_id.is_empty(), "Node status has no device ID");
        ensure!(
            node.data_dir.is_absolute(),
            "Node status has a relative data directory"
        );
        ensure!(
            !node.core_version.is_empty(),
            "Node status has no Core version"
        );
        if let Some(peers) = &node.peers {
            for peer in peers {
                ensure!(!peer.device_id.is_empty(), "peer status has no device ID");
            }
        }
        Ok(node)
    }

    pub fn version(&self) -> &str {
        self.version.as_deref().unwrap_or(&self.core_version)
    }

    pub fn writes(&self) -> &'static str {
        self.write_state.map_or("unavailable", WriteState::label)
    }

    pub fn peer_summary(&self) -> String {
        self.peers.as_ref().map_or_else(
            || "Peer status unavailable".into(),
            |peers| {
                let reachable = peers
                    .iter()
                    .filter(|peer| peer.reachable == Some(true))
                    .count();
                format!("{} peers · {reachable} reachable", peers.len())
            },
        )
    }
}

pub fn age(seconds: u64) -> String {
    match seconds {
        0..60 => format!("{seconds}s"),
        60..3600 => format!("{}m", seconds / 60),
        3600..86400 => format!("{}h", seconds / 3600),
        _ => format!("{}d", seconds / 86400),
    }
}

pub fn safe_text(value: &str) -> String {
    let mut safe = String::with_capacity(value.len());
    for ch in value.chars() {
        if ch.is_control()
            || matches!(ch, '\u{061c}' | '\u{200e}' | '\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}')
        {
            safe.extend(ch.escape_unicode());
        } else {
            safe.push(ch);
        }
    }
    safe
}

pub fn shorten(value: &str, width: usize) -> String {
    let value = safe_text(value);
    let span = Span::raw(value.as_str());
    if span.width() <= width {
        return value;
    }
    if width == 0 {
        return String::new();
    }
    let mut short = String::new();
    let mut used = 0;
    for grapheme in span.styled_graphemes(Style::default()) {
        let cells = Span::raw(grapheme.symbol).width();
        if used + cells > width - 1 {
            break;
        }
        short.push_str(grapheme.symbol);
        used += cells;
    }
    short.push('…');
    short
}

pub fn print_status(node: &NodeStatus) -> io::Result<()> {
    let mut out = io::stdout().lock();
    let width = crossterm::terminal::size().map_or(80, |(width, _)| width.max(16));
    writeln!(out)?;
    if use_emphasis() {
        writeln!(
            out,
            "  {}  {}",
            "vibeface".bold(),
            node.status.label().bold()
        )?;
    } else {
        writeln!(out, "  vibeface  {}", node.status.label())?;
    }
    writeln!(out, "  This node\n")?;
    for (label, value) in [
        ("Peer ID", node.device_id.clone()),
        ("Listen", node.listen.to_string()),
        ("Writes", node.writes().into()),
        (
            "Web",
            if node.web_enabled {
                "enabled"
            } else {
                "disabled"
            }
            .into(),
        ),
        ("Version", node.version().into()),
        ("Data", node.data_dir.to_string_lossy().into_owned()),
    ] {
        write_wrapped(
            &mut out,
            &format!("  {label:<9} "),
            "            ",
            &value,
            width,
        )?;
    }
    writeln!(out, "\n  {}", node.peer_summary())?;
    if let Some(peers) = &node.peers {
        if peers.is_empty() {
            write_wrapped(
                &mut out,
                "  ",
                "  ",
                "No peers yet. Use vibeface join --help to connect a device.",
                width,
            )?;
        }
        for peer in peers {
            writeln!(out)?;
            write_wrapped(
                &mut out,
                "    ",
                "    ",
                &format!("{}  {}", peer.name, peer.reachability()),
                width,
            )?;
            write_wrapped(
                &mut out,
                "    ",
                "    ",
                &format!(
                    "{} · {}",
                    peer.path_label(),
                    peer.last_sync(crate::now_ms())
                ),
                width,
            )?;
            write_wrapped(&mut out, "    ID  ", "        ", &peer.device_id, width)?;
        }
    }
    if let Some(error) = &node.error {
        writeln!(out)?;
        write_wrapped(&mut out, "  Detail    ", "            ", error, width)?;
    }
    if node.status == RunState::Stopped {
        writeln!(out)?;
        write_wrapped(
            &mut out,
            "  ",
            "  ",
            "Start with vibeface --data-dir <DATA_DIR> using the Data path above.",
            width,
        )?;
    }
    writeln!(out)
}

fn write_wrapped(
    out: &mut impl Write,
    prefix: &str,
    continuation: &str,
    value: &str,
    width: u16,
) -> io::Result<()> {
    let available = width
        .saturating_sub(Span::raw(prefix).width() as u16 + 2)
        .max(1);
    for (index, line) in wrapped_lines(&safe_text(value), available)
        .iter()
        .enumerate()
    {
        writeln!(
            out,
            "{}{line}",
            if index == 0 { prefix } else { continuation }
        )?;
    }
    Ok(())
}

fn wrapped_lines(value: &str, width: u16) -> Vec<String> {
    let mut lines = Vec::new();
    let mut line = String::new();
    let mut used = 0;
    for grapheme in Span::raw(value).styled_graphemes(Style::default()) {
        let cells = Span::raw(grapheme.symbol).width();
        if used > 0 && used + cells > usize::from(width) {
            lines.push(std::mem::take(&mut line));
            used = 0;
        }
        line.push_str(grapheme.symbol);
        used += cells;
    }
    lines.push(line);
    lines
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn test_data_dir() -> String {
        if cfg!(windows) {
            r"C:\vibeface\node".to_owned()
        } else {
            "/node".to_owned()
        }
    }

    #[test]
    fn status_preserves_unknown_peers_and_validates_the_display_boundary() {
        let value = json!({"status":"stopped","device_id":"local","data_dir":test_data_dir(),"listen":"127.0.0.1:4790","web_enabled":true,"core_version":"0.0.1","peers":null});
        let node = NodeStatus::parse(value.clone()).unwrap();
        assert!(node.peers.is_none());
        assert_eq!(node.peer_summary(), "Peer status unavailable");
        let mut empty = value.clone();
        empty["peers"] = json!([]);
        assert_eq!(
            NodeStatus::parse(empty).unwrap().peer_summary(),
            "0 peers · 0 reachable"
        );
        for (key, invalid) in [
            ("status", json!("healthy")),
            ("listen", json!("bogus")),
            ("data_dir", json!("relative")),
            ("device_id", json!("")),
        ] {
            let mut invalid_value = value.clone();
            invalid_value[key] = invalid;
            assert!(NodeStatus::parse(invalid_value).is_err(), "{key}");
        }
        let peer: PeerStatus = serde_json::from_value(json!({"device_id":"peer","name":"Build","path":null,"reachable":null,"synced_ms":null})).unwrap();
        assert_eq!(peer.reachability(), "not tried yet");
        assert_eq!(peer.last_sync(1000), "never synced");
    }

    #[test]
    fn terminal_text_cannot_emit_controls_or_split_graphemes() {
        let safe = safe_text("A\u{1b}]52;c;secret\u{7}\n\u{85}\u{202e}B");
        assert_eq!(safe, "A\\u{1b}]52;c;secret\\u{7}\\u{a}\\u{85}\\u{202e}B");
        assert!(!safe.chars().any(char::is_control));
        for width in 0..14 {
            let short = shorten("设备e\u{301}👩‍💻 device", width);
            assert!(Span::raw(short.as_str()).width() <= width);
            assert!(!short.ends_with('\u{301}'));
        }
        assert_eq!(shorten("e\u{301}xyz", 2), "e\u{301}…");
    }

    #[test]
    fn human_status_wraps_complete_values_with_aligned_continuations() {
        let value = "/long directory/设备/e\u{301}/👩‍💻/complete-device-id";
        let mut out = Vec::new();
        write_wrapped(&mut out, "  Data      ", "            ", value, 45).unwrap();
        let text = String::from_utf8(out).unwrap();
        let mut recovered = String::new();
        for line in text.lines() {
            assert!(line.starts_with("            ") || line.starts_with("  Data      "));
            assert!(Span::raw(line).width() <= 43);
            recovered.push_str(&line[12..]);
        }
        assert_eq!(recovered, value);
    }
}
