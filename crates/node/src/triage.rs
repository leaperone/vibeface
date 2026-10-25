//! Node-side triage of local agent sessions: one event shape for hooks, App Server and session
//! scans, the coordination Agent's prompts, outlet choice, webhook signing and delivery, and hook
//! installation. The coordination Agent judges; nothing here decides what matters.
use anyhow::{Context as _, Result, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest as _, Sha256};
use std::path::{Path, PathBuf};

pub const SCHEMA_VERSION: u32 = 1;
pub const MAX_MESSAGE: usize = 4000;
pub const MAX_DETAIL: usize = 2000;
pub const MAX_ATTEMPTS: u32 = 8;
pub const FIRST_BACKOFF_MS: i64 = 10_000;

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum Source {
    #[default]
    CodexHook,
    ClaudeHook,
    AppServer,
    SessionScan,
    Test,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum Agent {
    #[default]
    Codex,
    Claude,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    #[default]
    TurnEnd,
    NeedsInput,
    Notification,
}

/// Normalized event (design 4.1). Hooks send it without `node`, `host` and `event_id`; the
/// receiving Node fills those in `seal`, so every source derives the same ID.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Default)]
#[serde(default, deny_unknown_fields)]
pub struct AgentEvent {
    pub schema_version: u32,
    pub event_id: String,
    pub source: Source,
    pub host: String,
    pub node: String,
    pub agent: Agent,
    pub event: Kind,
    pub status: Option<String>,
    pub session_id: String,
    /// The turn's ID, or `tx:<transcript bytes>` for Claude Code, which has none.
    pub turn_id: String,
    pub cwd: String,
    pub repo: Option<String>,
    pub git_branch: Option<String>,
    pub git_head: Option<String>,
    pub last_message: String,
    pub detail: String,
    pub ts_ms: i64,
    pub test: bool,
    /// An App Server request waiting for an answer; set by the Node, never by a hook.
    pub request_id: Option<String>,
}

fn clip(text: &str, max: usize) -> String {
    text.chars().take(max).collect()
}

fn hex32(parts: &[&str]) -> String {
    let mut hash = Sha256::new();
    for part in parts {
        hash.update(part.as_bytes());
        hash.update([0]);
    }
    hex::encode(hash.finalize())[..32].to_owned()
}

/// A bot's question about an event: one per event wherever it is raised.
pub fn question_id(node: &str, event_id: &str) -> String {
    format!("q-{}", hex32(&[node, event_id]))
}

impl AgentEvent {
    /// Status is left out, so a hook, the App Server and a scan of the same turn collide.
    pub fn id_for(node: &str, agent: Agent, session: &str, turn: &str, kind: Kind) -> String {
        hex32(&[node, agent_name(agent), session, turn, kind_name(kind)])
    }

    pub fn seal(mut self, node: &str, host: &str, now_ms: i64) -> Result<Self> {
        ensure!(
            !self.session_id.is_empty() && self.session_id.len() <= 256,
            "session_id is required"
        );
        ensure!(self.turn_id.len() <= 256, "turn_id is too long");
        ensure!(self.cwd.len() <= 4096, "cwd is too long");
        self.schema_version = SCHEMA_VERSION;
        self.node = node.into();
        self.host = clip(host, 128);
        self.last_message = clip(&self.last_message, MAX_MESSAGE);
        self.detail = clip(&self.detail, MAX_DETAIL);
        if self.ts_ms <= 0 {
            self.ts_ms = now_ms;
        }
        self.event_id = Self::id_for(
            node,
            self.agent,
            &self.session_id,
            &self.turn_id,
            self.event,
        );
        Ok(self)
    }
}

fn agent_name(agent: Agent) -> &'static str {
    match agent {
        Agent::Codex => "codex",
        Agent::Claude => "claude",
    }
}
fn kind_name(kind: Kind) -> &'static str {
    match kind {
        Kind::TurnEnd => "turn_end",
        Kind::NeedsInput => "needs_input",
        Kind::Notification => "notification",
    }
}

fn s(value: &Value, key: &str) -> String {
    value[key].as_str().unwrap_or_default().to_owned()
}

/// A Codex hook's stdin (`Stop`, `PermissionRequest`). The tool input is left out: commands
/// are not surfaced.
pub fn from_codex_hook(input: &Value) -> Option<AgentEvent> {
    let (event, status, detail) = match input["hook_event_name"].as_str()? {
        "Stop" => (Kind::TurnEnd, Some("completed".to_owned()), String::new()),
        "PermissionRequest" => (Kind::NeedsInput, None, s(input, "tool_name")),
        _ => return None,
    };
    Some(AgentEvent {
        source: Source::CodexHook,
        agent: Agent::Codex,
        event,
        status,
        session_id: s(input, "session_id"),
        turn_id: s(input, "turn_id"),
        cwd: s(input, "cwd"),
        last_message: s(input, "last_bot_message"),
        detail,
        ..Default::default()
    })
}

/// A Claude Code hook's stdin (`Stop`, `StopFailure`, `Notification`); `transcript_bytes` keys
/// the turn.
pub fn from_claude_hook(input: &Value, transcript_bytes: u64) -> Option<AgentEvent> {
    let (event, status, detail) = match input["hook_event_name"].as_str()? {
        "Stop" => (Kind::TurnEnd, Some("completed".to_owned()), String::new()),
        "StopFailure" => (
            Kind::TurnEnd,
            Some("failed".to_owned()),
            [s(input, "error"), s(input, "error_details")]
                .join(" ")
                .trim()
                .into(),
        ),
        "Notification" => (
            Kind::Notification,
            None,
            [s(input, "notification_type"), s(input, "message")]
                .join(" ")
                .trim()
                .into(),
        ),
        _ => return None,
    };
    Some(AgentEvent {
        source: Source::ClaudeHook,
        agent: Agent::Claude,
        event,
        status,
        session_id: s(input, "session_id"),
        turn_id: format!("tx:{transcript_bytes}"),
        cwd: s(input, "cwd"),
        last_message: s(input, "last_bot_message"),
        detail,
        ..Default::default()
    })
}

/// Whether a Codex session's `source` (a rollout's `session_meta`, or an App Server thread)
/// is one the user drives. `codex exec`, MCP and sub-agent sessions run without anyone
/// watching them.
pub fn interactive_codex(source: &Value, originator: Option<&str>) -> bool {
    let background = match source {
        Value::String(source) => matches!(source.to_ascii_lowercase().as_str(), "exec" | "mcp"),
        Value::Object(source) => source
            .keys()
            .any(|key| key.eq_ignore_ascii_case("subagent")),
        _ => false,
    };
    !background && !originator.is_some_and(|originator| originator.ends_with("_exec"))
}

/// Whether a Claude Code hook runs in a session the user drives; `claude -p` and the SDKs set
/// an `sdk-*` entry point.
pub fn interactive_claude(entrypoint: Option<&str>) -> bool {
    !entrypoint.is_some_and(|entrypoint| entrypoint.starts_with("sdk"))
}

/// The `session_meta` payload on a Codex rollout's first line, if it has one.
pub fn rollout_meta(first_line: &str) -> Value {
    serde_json::from_str::<Value>(first_line)
        .ok()
        .filter(|line| line["type"] == "session_meta")
        .map(|line| line["payload"].clone())
        .unwrap_or(Value::Null)
}

/// Turn ends in the complete lines of a Codex rollout after `offset`, none for a session the
/// user does not drive. Returns the events and the offset after the last complete line.
pub fn scan_rollout(text: &str, offset: usize) -> (Vec<AgentEvent>, usize) {
    let meta = rollout_meta(text.lines().next().unwrap_or_default());
    let end = text
        .rfind('\n')
        .map_or(0, |at| at + 1)
        .max(offset.min(text.len()));
    let mut events = Vec::new();
    let Some(tail) = text.get(offset.min(end)..end) else {
        return (events, end);
    };
    if !interactive_codex(&meta["source"], meta["originator"].as_str()) {
        return (events, end);
    }
    for line in tail.lines() {
        let Ok(line) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        let payload = &line["payload"];
        if line["type"] != "event_msg" {
            continue;
        }
        let (status, message) = match payload["type"].as_str() {
            Some("task_complete") => ("completed", s(payload, "last_agent_message")),
            Some("turn_aborted") => ("interrupted", s(payload, "reason")),
            _ => continue,
        };
        events.push(AgentEvent {
            source: Source::SessionScan,
            agent: Agent::Codex,
            event: Kind::TurnEnd,
            status: Some(status.into()),
            session_id: s(&meta, "id"),
            turn_id: s(payload, "turn_id"),
            cwd: s(&meta, "cwd"),
            git_branch: meta["git"]["branch"].as_str().map(String::from),
            git_head: meta["git"]["commit_hash"].as_str().map(String::from),
            last_message: message,
            ..Default::default()
        });
    }
    (events, end)
}

pub use crate::model::Severity;

/// The coordination Agent's turn when new events wait: it decides, then answers through
/// `vibeface triage raise|dismiss`. Each event comes with the bot whose chat owns its session.
pub fn wake_prompt(
    events: &[(AgentEvent, Value)],
    cli: &str,
    answer_within_secs: u64,
) -> Result<String> {
    let items: Vec<Value> = events
        .iter()
        .map(|(e, bot)| {
            json!({"event_id":e.event_id,"agent":e.agent,"event":e.event,"status":e.status,
            "session_id":e.session_id,"turn_id":e.turn_id,"cwd":e.cwd,"repo":e.repo,
            "git_branch":e.git_branch,"last_message":clip(&e.last_message, 1500),
            "detail":clip(&e.detail, 500),"request_id":e.request_id,"bot":bot})
        })
        .collect();
    Ok(format!(
        "New events from the user's local coding agent sessions are waiting. Decide for each \
whether the user must look at it now. Stay silent unless it matters; events you leave alone are \
dropped after this turn.\n\
- Needs the user: `{cli} triage raise --event-id ID --severity info|attention|urgent --question TEXT [--choice LABEL]...`. \
It asks in the chat of the event's bot. Write TEXT in the user's language and plain words: what is happening and what \
they need to decide, in one or two sentences, never a shell command, code or raw output. Each --choice is one short action \
the user can pick (2 to 4, at most 8, no numbers; the card numbers them), the likely answer first and never a destructive \
one first; the user can always type an answer instead. While a question about the same session is unanswered, a raise returns it. \
The answer comes back to that bot as a new turn, and it acts on it.\n\
- Otherwise: `{cli} triage dismiss --event-id ID`.\n\
- An event with a request_id is a Codex App Server request waiting for an answer. `{cli} codex requests` \
shows its full params. Answer it yourself with `{cli} codex respond REQUEST_ID 'RESULT_JSON'` \
(the thread already runs with full access). Only when the user must decide, add `--request-id REQUEST_ID` \
to the raise: the request then waits for the answer, and the bot has {answer_within_secs} seconds \
after it to respond. A request neither answered nor raised is declined after {answer_within_secs} seconds \
or when this turn ends.\n\
`{cli} triage events` lists what is still pending.\n\nEvents:\n{}",
        serde_json::to_string_pretty(&items)?
    ))
}

/// A question is always recorded; `both` also sends it to the webhook.
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum Outlet {
    #[default]
    Record,
    Both,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ProjectOutlet {
    pub prefix: String,
    pub outlet: Outlet,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct TriageConfig {
    pub default_outlet: Outlet,
    pub projects: Vec<ProjectOutlet>,
    pub webhook_url: Option<String>,
    pub scan_codex_sessions: bool,
    pub codex_sessions: Option<PathBuf>,
}

impl Default for TriageConfig {
    fn default() -> Self {
        Self {
            default_outlet: Outlet::Record,
            projects: vec![],
            webhook_url: None,
            scan_codex_sessions: false,
            codex_sessions: None,
        }
    }
}

impl TriageConfig {
    /// The longest matching path prefix wins; `/a/b` matches `/a/b/c` but not `/a/bc`.
    pub fn outlet_for(&self, cwd: &str) -> Outlet {
        self.projects
            .iter()
            .filter(|p| Path::new(cwd).starts_with(&p.prefix))
            .max_by_key(|p| p.prefix.len())
            .map_or(self.default_outlet, |p| p.outlet)
    }

    pub fn validate(&self) -> Result<()> {
        if self.scan_codex_sessions {
            ensure!(
                self.codex_sessions
                    .as_ref()
                    .is_some_and(|path| path.is_absolute() && path.is_dir()),
                "set codex_sessions to an existing absolute directory before enabling session scanning"
            );
        }
        ensure!(self.projects.len() <= 256, "at most 256 project outlets");
        for project in &self.projects {
            ensure!(
                Path::new(&project.prefix).is_absolute() && project.prefix.len() <= 4096,
                "a project prefix must be an absolute path"
            );
        }
        if let Some(url) = &self.webhook_url {
            let parsed = reqwest::Url::parse(url).context("invalid webhook URL")?;
            let loopback = parsed.host_str().is_some_and(|host| {
                host == "localhost"
                    || host
                        .trim_matches(['[', ']'])
                        .parse::<std::net::IpAddr>()
                        .is_ok_and(|ip| ip.is_loopback())
            });
            ensure!(
                parsed.scheme() == "https" || (parsed.scheme() == "http" && loopback),
                "the webhook must use HTTPS"
            );
            ensure!(
                parsed.username().is_empty() && parsed.password().is_none() && url.len() <= 2048,
                "the webhook URL must not carry credentials"
            );
        }
        Ok(())
    }
}

/// HMAC-SHA256 (RFC 2104) over `sha2`, to avoid a dependency for twenty lines.
pub fn hmac_sha256(key: &[u8], message: &[u8]) -> [u8; 32] {
    let mut block = [0u8; 64];
    if key.len() > 64 {
        block[..32].copy_from_slice(&Sha256::digest(key));
    } else {
        block[..key.len()].copy_from_slice(key);
    }
    let pad = |byte: u8| block.map(|b| b ^ byte);
    let inner = Sha256::new()
        .chain_update(pad(0x36))
        .chain_update(message)
        .finalize();
    Sha256::new()
        .chain_update(pad(0x5c))
        .chain_update(inner)
        .finalize()
        .into()
}

/// `X-Vibeface-Signature` for a body sent at `timestamp` (Unix seconds).
pub fn signature(secret: &[u8], timestamp: i64, body: &str) -> String {
    format!(
        "v1={}",
        hex::encode(hmac_sha256(
            secret,
            format!("{timestamp}.{body}").as_bytes()
        ))
    )
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Delivery {
    Pending,
    Delivered,
    Dead,
}

/// One webhook delivery, keyed by `event_id` so an event is queued once.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct OutboxEntry {
    pub event_id: String,
    pub body: String,
    pub state: Delivery,
    pub attempts: u32,
    pub next_ms: i64,
}

impl OutboxEntry {
    pub fn new(event_id: &str, body: String, now_ms: i64) -> Self {
        Self {
            event_id: event_id.into(),
            body,
            state: Delivery::Pending,
            attempts: 0,
            next_ms: now_ms,
        }
    }

    /// Applies one attempt's result: an HTTP status, or `None` when the request failed.
    pub fn record(&mut self, status: Option<u16>, now_ms: i64) {
        self.attempts += 1;
        let retry = match status {
            Some(200..=299) => {
                self.state = Delivery::Delivered;
                return;
            }
            None | Some(408 | 429 | 500..=599) => true,
            Some(_) => false,
        };
        if !retry || self.attempts >= MAX_ATTEMPTS {
            self.state = Delivery::Dead;
            return;
        }
        self.next_ms = now_ms + FIRST_BACKOFF_MS * (1 << (self.attempts - 1).min(16));
    }
}

/// Hook files `vibeface hooks install` edits under a home directory.
pub struct HookFile {
    pub agent: Agent,
    pub path: PathBuf,
    pub events: &'static [&'static str],
}

pub fn hook_files(home: &Path, codex_home: Option<&Path>) -> [HookFile; 2] {
    [
        HookFile {
            agent: Agent::Codex,
            path: codex_home
                .map(Path::to_owned)
                .unwrap_or_else(|| home.join(".codex"))
                .join("hooks.json"),
            events: &["Stop", "PermissionRequest"],
        },
        HookFile {
            agent: Agent::Claude,
            path: home.join(".claude/settings.json"),
            events: &["Stop", "StopFailure", "Notification"],
        },
    ]
}

pub fn hook_command(program: &Path, agent: Agent, data_dir: &Path) -> String {
    let quote = |text: &str| format!("'{}'", text.replace('\'', r"'\''"));
    format!(
        "{} hook {} --data-dir {}",
        quote(&program.to_string_lossy()),
        agent_name(agent),
        quote(&data_dir.to_string_lossy())
    )
}

fn ours(entry: &Value, agent: Agent) -> bool {
    entry["hooks"].as_array().is_some_and(|hooks| {
        hooks.iter().any(|hook| {
            hook["command"].as_str().is_some_and(|command| {
                command.contains(&format!(" hook {} ", agent_name(agent)))
                    && command.contains("vibeface")
            })
        })
    })
}

/// Adds this Node's hook to each event of `settings`, leaving every other entry in place.
/// Returns the events it added; an event that already has it is left alone.
pub fn add_hooks(settings: &mut Value, file: &HookFile, command: &str) -> Result<Vec<String>> {
    if settings.is_null() {
        *settings = json!({});
    }
    let hooks = settings
        .as_object_mut()
        .context("hook file is not a JSON object")?
        .entry("hooks")
        .or_insert_with(|| json!({}))
        .as_object_mut()
        .context("`hooks` is not an object")?;
    let mut added = Vec::new();
    for event in file.events {
        let entries = hooks
            .entry(*event)
            .or_insert_with(|| json!([]))
            .as_array_mut()
            .with_context(|| format!("`hooks.{event}` is not a list"))?;
        if entries.iter().any(|entry| ours(entry, file.agent)) {
            continue;
        }
        entries.push(json!({"hooks":[{"type":"command","command":command,"timeout":10}]}));
        added.push((*event).to_owned());
    }
    Ok(added)
}

/// Removes only entries this tool added; returns the events it changed.
pub fn remove_hooks(settings: &mut Value, file: &HookFile) -> Vec<String> {
    let mut removed = Vec::new();
    if let Some(hooks) = settings["hooks"].as_object_mut() {
        for event in file.events {
            if let Some(entries) = hooks.get_mut(*event).and_then(Value::as_array_mut) {
                let before = entries.len();
                entries.retain(|entry| !ours(entry, file.agent));
                if entries.len() != before {
                    removed.push((*event).to_owned());
                }
            }
        }
    }
    removed
}

pub fn installed(settings: &Value, file: &HookFile) -> Vec<String> {
    file.events
        .iter()
        .filter(|event| {
            settings["hooks"][**event]
                .as_array()
                .is_some_and(|entries| entries.iter().any(|entry| ours(entry, file.agent)))
        })
        .map(|event| (*event).to_owned())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hmac_matches_rfc_4231() {
        let key = [0x0b; 20];
        assert_eq!(
            hex::encode(hmac_sha256(&key, b"Hi There")),
            "b0344c61d8db38535ca8afceaf0bf12b881dc200c9833da726e9376c2e32cff7"
        );
        let long = [0xaa; 131];
        assert_eq!(
            hex::encode(hmac_sha256(
                &long,
                b"Test Using Larger Than Block-Size Key - Hash Key First"
            )),
            "60e431591ee0b67f0d8a26aacbf5b77f8e0bc6213728c5140546040f0ee37f54"
        );
        assert!(signature(b"k", 1, "{}").starts_with("v1="));
        assert_ne!(signature(b"k", 1, "{}"), signature(b"k", 2, "{}"));
    }

    #[test]
    fn every_source_of_one_turn_gets_one_event_id() {
        let node = "n".repeat(64);
        let hook = from_codex_hook(&json!({"hook_event_name":"Stop","session_id":"t1","turn_id":"u1","cwd":"/p","last_bot_message":"done"}))
            .unwrap()
            .seal(&node, "h", 1)
            .unwrap();
        let rollout = format!(
            "{}\n{}\n{{\"type\":\"event_msg\",\"payload\":{{\"type\":\"task_",
            json!({"type":"session_meta","payload":{"id":"t1","cwd":"/p","git":{"branch":"main"}}}),
            json!({"type":"event_msg","payload":{"type":"task_complete","turn_id":"u1","last_agent_message":"done"}})
        );
        let (events, offset) = scan_rollout(&rollout, 0);
        assert_eq!(
            offset,
            rollout.rfind('\n').unwrap() + 1,
            "partial line kept for later"
        );
        let scanned = events[0].clone().seal(&node, "h", 1).unwrap();
        assert_eq!(scanned.event_id, hook.event_id);
        assert_eq!(scanned.git_branch.as_deref(), Some("main"));
        assert!(scan_rollout(&rollout, offset).0.is_empty());
        let failed = AgentEvent {
            status: Some("failed".into()),
            ..scanned.clone()
        }
        .seal(&node, "h", 1)
        .unwrap();
        assert_eq!(
            failed.event_id, hook.event_id,
            "status is not part of the ID"
        );
        let other = AgentEvent {
            turn_id: "u2".into(),
            ..scanned
        }
        .seal(&node, "h", 1)
        .unwrap();
        assert_ne!(other.event_id, hook.event_id);
        let claude = from_claude_hook(
            &json!({"hook_event_name":"Notification","session_id":"c","message":"approve?"}),
            10,
        )
        .unwrap();
        assert_eq!(
            (claude.turn_id.as_str(), claude.event),
            ("tx:10", Kind::Notification)
        );
        assert!(AgentEvent::default().seal(&node, "h", 1).is_err());
    }

    #[test]
    fn sessions_nobody_drives_are_not_scanned() {
        let rollout = |meta: Value| {
            format!(
                "{}\n{}\n",
                json!({"type":"session_meta","payload":meta}),
                json!({"type":"event_msg","payload":{"type":"task_complete","turn_id":"u1"}})
            )
        };
        for meta in [
            json!({"id":"t1","source":"cli","originator":"codex_cli_rs"}),
            json!({"id":"t1","source":"vscode"}),
            json!({"id":"t1"}),
        ] {
            assert_eq!(scan_rollout(&rollout(meta.clone()), 0).0.len(), 1, "{meta}");
        }
        for meta in [
            json!({"id":"t1","source":"exec","originator":"codex_exec"}),
            json!({"id":"t1","source":"cli","originator":"codex_exec"}),
            json!({"id":"t1","source":"mcp"}),
            json!({"id":"t1","source":{"subagent":"review"}}),
            json!({"id":"t1","source":{"subAgent":{"thread_spawn":{"depth":1}}}}),
        ] {
            let text = rollout(meta.clone());
            assert_eq!(scan_rollout(&text, 0), (vec![], text.len()), "{meta}");
        }
        assert!(interactive_claude(None) && interactive_claude(Some("cli")));
        assert!(!interactive_claude(Some("sdk-cli")) && !interactive_claude(Some("sdk-ts")));
    }

    #[test]
    fn outbox_retries_transient_failures_and_dead_letters_the_rest() {
        let mut entry = OutboxEntry::new("e", "{}".into(), 0);
        entry.record(Some(503), 0);
        assert_eq!((entry.state, entry.next_ms), (Delivery::Pending, 10_000));
        entry.record(None, 10_000);
        assert_eq!(entry.next_ms, 30_000);
        for _ in 2..MAX_ATTEMPTS {
            entry.record(Some(429), 0);
        }
        assert_eq!(
            (entry.state, entry.attempts),
            (Delivery::Dead, MAX_ATTEMPTS)
        );
        let mut rejected = OutboxEntry::new("e", "{}".into(), 0);
        rejected.record(Some(401), 0);
        assert_eq!((rejected.state, rejected.attempts), (Delivery::Dead, 1));
        let mut ok = OutboxEntry::new("e", "{}".into(), 0);
        ok.record(Some(202), 0);
        assert_eq!(ok.state, Delivery::Delivered);
    }

    #[test]
    fn outlets_match_the_longest_path_prefix() {
        let config = TriageConfig {
            default_outlet: Outlet::Record,
            projects: vec![
                ProjectOutlet {
                    prefix: "/w".into(),
                    outlet: Outlet::Both,
                },
                ProjectOutlet {
                    prefix: "/w/app".into(),
                    outlet: Outlet::Record,
                },
            ],
            ..Default::default()
        };
        assert_eq!(config.outlet_for("/w/app/src"), Outlet::Record);
        assert_eq!(config.outlet_for("/w/apps"), Outlet::Both);
        assert_eq!(config.outlet_for("/x"), Outlet::Record);
        assert!(serde_json::from_value::<Outlet>(json!("webhook")).is_err());
        for url in ["http://hook.example/x", "https://u:p@hook.example/"] {
            assert!(
                TriageConfig {
                    webhook_url: Some(url.into()),
                    ..Default::default()
                }
                .validate()
                .is_err()
            );
        }
        assert!(
            TriageConfig {
                webhook_url: Some("http://127.0.0.1:9/h".into()),
                ..Default::default()
            }
            .validate()
            .is_ok()
        );
    }

    #[test]
    fn hook_install_appends_once_and_removes_only_its_own_entries() {
        let home = Path::new("/home/u");
        let [codex, claude] = hook_files(home, None);
        assert_eq!(codex.path, home.join(".codex/hooks.json"));
        let command = hook_command(Path::new("/opt/vibeface"), Agent::Claude, Path::new("/d'x"));
        assert_eq!(command, r"'/opt/vibeface' hook claude --data-dir '/d'\''x'");
        let other = json!({"hooks":[{"type":"command","command":"orca notify"}]});
        let mut settings = json!({"model":"x","hooks":{"Stop":[other.clone()]}});
        assert_eq!(
            add_hooks(&mut settings, &claude, &command).unwrap(),
            ["Stop", "StopFailure", "Notification"]
        );
        assert!(
            add_hooks(&mut settings, &claude, &command)
                .unwrap()
                .is_empty()
        );
        assert_eq!(settings["hooks"]["Stop"][0], other);
        assert_eq!(settings["hooks"]["Stop"].as_array().unwrap().len(), 2);
        assert_eq!(installed(&settings, &claude).len(), 3);
        assert!(installed(&settings, &codex).is_empty());
        assert_eq!(remove_hooks(&mut settings, &claude).len(), 3);
        assert_eq!(
            settings,
            json!({"model":"x","hooks":{"Stop":[other],"StopFailure":[],"Notification":[]}})
        );
        assert!(add_hooks(&mut json!([]), &codex, &command).is_err());
    }
}
