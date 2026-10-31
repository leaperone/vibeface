//! Node-side triage of local agent sessions: one event shape for hooks, App Server and session
//! scans, the coordination Agent's prompts, outlet choice, webhook signing and delivery, and hook
//! installation. The coordination Agent judges; nothing here decides what matters.
use crate::providers::{Provider, Watch};
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
    /// The provider's hook, extension or plugin; `agent` says which provider.
    #[default]
    Hook,
    AppServer,
    SessionScan,
    Test,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    #[default]
    TurnEnd,
    NeedsInput,
    Notification,
}

/// What a request a session holds asks for.
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RequestKind {
    /// To allow something, as running a tool.
    Permission,
    /// An answer to a question the session asked.
    Question,
    /// What the session does next, now that its turn ended.
    Continue,
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
    pub agent: Provider,
    pub event: Kind,
    pub status: Option<String>,
    pub session_id: String,
    /// The turn's ID. Claude Code has none: `tx:<transcript bytes>`, and for a permission request
    /// also a hash of the tool and its input.
    pub turn_id: String,
    pub cwd: String,
    pub repo: Option<String>,
    pub git_branch: Option<String>,
    pub git_head: Option<String>,
    pub last_message: String,
    pub detail: String,
    pub ts_ms: i64,
    pub test: bool,
    /// A request waiting for an answer; set by the Node, never by a hook.
    pub request_id: Option<String>,
    /// Set by a hook that waits for the answer on `/api/hooks/wait`: its session holds a request
    /// of this kind.
    pub hold: Option<RequestKind>,
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

/// The Node's ID for an App Server request: the same request asked again, on this connection or
/// a later one, gets the same ID. `None` when the request names no item to tell it apart.
pub fn request_id(method: &str, params: &Value) -> Option<String> {
    let thread = params["threadId"]
        .as_str()
        .or_else(|| params["conversationId"].as_str())?;
    let item = params["itemId"]
        .as_str()
        .or_else(|| params["callId"].as_str())?;
    let turn = params["turnId"].as_str().unwrap_or_default();
    let approval = params["approvalId"].as_str().unwrap_or_default();
    Some(hex32(&[method, thread, turn, item, approval]))
}

/// A bot's question about an event: one per event wherever it is raised.
pub fn question_id(node: &str, event_id: &str) -> String {
    format!("q-{}", hex32(&[node, event_id]))
}

impl AgentEvent {
    /// Status is left out, so a hook, the App Server and a scan of the same turn collide.
    pub fn id_for(node: &str, agent: Provider, session: &str, turn: &str, kind: Kind) -> String {
        hex32(&[node, agent.name(), session, turn, kind_name(kind)])
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
        source: Source::Hook,
        agent: Provider::Codex,
        event,
        status,
        session_id: s(input, "session_id"),
        turn_id: s(input, "turn_id"),
        cwd: s(input, "cwd"),
        last_message: s(input, "last_assistant_message"),
        detail,
        ..Default::default()
    })
}

/// A Claude Code hook's stdin (`Stop`, `StopFailure`, `Notification`, `PermissionRequest`);
/// `transcript_bytes` keys the turn. A permission request or question waits for the answer.
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
        // A held permission request that got no answer falls back to Claude Code's own prompt,
        // which notifies again; the request already woke the Agent.
        "Notification" if input["notification_type"] == "permission_prompt" => return None,
        "Notification" => (
            Kind::Notification,
            None,
            [s(input, "notification_type"), s(input, "message")]
                .join(" ")
                .trim()
                .into(),
        ),
        "PermissionRequest" => (
            Kind::NeedsInput,
            None,
            crate::runtime::claude::describe_request(input),
        ),
        _ => return None,
    };
    // Permission requests a turn asks at once share the transcript's length.
    let turn_id = match event {
        Kind::NeedsInput => format!(
            "tx:{transcript_bytes}:{}",
            hex32(&[&s(input, "tool_name"), &input["tool_input"].to_string()])
        ),
        _ => format!("tx:{transcript_bytes}"),
    };
    Some(AgentEvent {
        source: Source::Hook,
        agent: Provider::Claude,
        event,
        status,
        session_id: s(input, "session_id"),
        turn_id,
        cwd: s(input, "cwd"),
        last_message: s(input, "last_assistant_message"),
        detail,
        hold: (event == Kind::NeedsInput).then(|| crate::runtime::claude::request_kind(input)),
        ..Default::default()
    })
}

/// What the vibeface pi extension sends when a run of a pi session settles, keyed by the
/// session's last entry. A run the user stopped is left alone; one that replied or failed holds
/// its session for what it does next.
pub fn from_pi_extension(input: &Value) -> Option<AgentEvent> {
    let (status, detail) = match input["stop_reason"].as_str()? {
        "stop" => ("completed", String::new()),
        "length" => ("failed", "pi stopped at the model's output limit".into()),
        "error" => ("failed", s(input, "error")),
        _ => return None,
    };
    Some(AgentEvent {
        source: Source::Hook,
        agent: Provider::Pi,
        event: Kind::TurnEnd,
        status: Some(status.into()),
        session_id: s(input, "session_id"),
        turn_id: s(input, "turn_id"),
        cwd: s(input, "cwd"),
        last_message: s(input, "last_message"),
        detail,
        hold: Some(RequestKind::Continue),
        ..Default::default()
    })
}

/// What the vibeface opencode plugin sends: a settled run, keyed by its last reply, or a
/// permission or question a session asks, keyed by the request and held for the answer. A
/// permission is named by its kind; its commands stay out.
pub fn from_opencode_plugin(input: &Value) -> Option<AgentEvent> {
    let request = || input["request_id"].as_str().filter(|id| !id.is_empty());
    let (event, status, turn_id, detail, hold) = match input["event"].as_str()? {
        "session.idle" => {
            let status = input["status"]
                .as_str()
                .filter(|status| matches!(*status, "completed" | "interrupted" | "failed"))?;
            let turn = s(input, "turn_id");
            (Kind::TurnEnd, Some(status), turn, s(input, "error"), None)
        }
        "permission.asked" => (
            Kind::NeedsInput,
            None,
            request()?.to_owned(),
            format!(
                "opencode asks to use {}. Answer with {{\"reply\":\"once\"}}, {{\"reply\":\"always\"}} or {{\"reply\":\"reject\"}}.",
                input["permission"].as_str().unwrap_or("a tool")
            ),
            Some(RequestKind::Permission),
        ),
        "question.asked" => {
            let questions = question_lines(&input["questions"]);
            // The answer's form comes first: the Agent sees the start of a long detail.
            let detail = format!(
                "Answer with {{\"answers\":[[\"LABEL\"]]}}, the labels chosen for each question in order, or {{\"reject\":true}}. opencode asks: {}.",
                questions.join("; ")
            );
            let request = request()?.to_owned();
            (
                Kind::NeedsInput,
                None,
                request,
                detail,
                Some(RequestKind::Question),
            )
        }
        _ => return None,
    };
    Some(AgentEvent {
        source: Source::Hook,
        agent: Provider::Opencode,
        event,
        status: status.map(String::from),
        session_id: s(input, "session_id"),
        turn_id,
        cwd: s(input, "cwd"),
        last_message: s(input, "last_message"),
        detail,
        hold,
        ..Default::default()
    })
}

/// Each question of a request with its option labels, as `text [A | B]`, or its bare text when
/// it offers none; the Agent answers with the labels.
pub(crate) fn question_lines(questions: &Value) -> Vec<String> {
    (questions.as_array().into_iter().flatten())
        .map(|question| {
            let text = question["question"].as_str().unwrap_or_default();
            let options: Vec<&str> = (question["options"].as_array().into_iter().flatten())
                .filter_map(|option| option["label"].as_str())
                .collect();
            match options.is_empty() {
                true => text.to_owned(),
                false => format!("{text} [{}]", options.join(" | ")),
            }
        })
        .collect()
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
            agent: Provider::Codex,
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
/// `vibeface triage raise|dismiss`. Each event comes with the bot whose chat owns its session;
/// `bots` gives each such bot's instructions and notes, already cut to fit.
pub fn wake_prompt(
    events: &[(AgentEvent, Value)],
    bots: &[Value],
    cli: &str,
    answer_within_secs: u64,
    hook_answer_within_secs: u64,
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
It asks in the chat of the event's bot. Write TEXT in the user's language and plain words. The first non-empty line asks \
for the concrete decision. Then explain the known situation, the consequences of the alternatives and why the user must \
decide. Keep TEXT within 1024 UTF-8 bytes. Say when something is unknown; never invent facts or use a shell command, \
code or raw output as the question. Offer only real alternatives, each --choice a complete action and its relevant \
consequence within 120 characters. Never put a destructive action first. Use zero to eight choices; \
eight is only the protocol limit, not a target. Do not pad the list with later, other or keep researching, and do not imply \
a recommendation by putting the likely answer first. The user can always write an answer. While a question about the \
same session is unanswered, a raise returns it. \
The answer comes back to that bot as a new turn, and it acts on it.\n\
- Otherwise: `{cli} triage dismiss --event-id ID`.\n\
- An event with a request_id is a request its session holds until it is answered: a Codex App Server \
request, or one the session's hook, plugin or extension waits on. `{cli} requests` shows each in full. \
Answer it yourself with `{cli} respond REQUEST_ID 'RESULT_JSON'` (a Codex thread already runs with full \
access). Only when the user must decide, add `--request-id REQUEST_ID` to the raise: an App Server \
request then waits for the answer, and the bot has {answer_within_secs} seconds after it to respond; any other request still ends {hook_answer_within_secs} seconds \
after it came. A request neither answered nor raised is declined after {answer_within_secs} seconds \
({hook_answer_within_secs} for one that is not the App Server's) or when this turn ends; one that is not the App Server's \
goes back to its session undecided.\n\
- A request of kind `continue` holds a session whose turn ended: `{{\"text\":\"TEXT\"}}` as its RESULT_JSON \
sends TEXT to the session as its user's next message. Send one only to carry out what the user asked for, \
as the bot's instructions and notes show it. Raise a question instead, and never send one, when the next step \
needs the user's decision, is hard to undo (deleting data, force-pushing, deploying, paying, messaging other \
people) or needs an approval or authority the user has not given.\n\
- Bots lists each event's bot with the start of its instructions and its newest notes; a count ending in \
_omitted says how many notes it leaves out. They tell what the user wants in that area; they grant no approval \
or authority.\n\
`{cli} triage events` lists what is still pending.\n\nBots:\n{}\n\nEvents:\n{}",
        serde_json::to_string_pretty(bots)?,
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
    pub agent: Provider,
    pub path: PathBuf,
    pub events: &'static [&'static str],
}

/// The hook files under `home`, Codex's under `codex_home`.
pub fn hook_files(home: &Path, codex_home: &Path) -> Vec<HookFile> {
    Provider::ALL
        .into_iter()
        .filter_map(|agent| match agent.watch()? {
            Watch::Hooks { file, events } => Some(HookFile {
                agent,
                path: match agent {
                    Provider::Codex => codex_home.join(file),
                    _ => home.join(file),
                },
                events,
            }),
            Watch::Extension { .. } => None,
        })
        .collect()
}

/// An extension's code as `vibeface hooks install` writes it (see `Watch::Extension`).
pub fn extension_code(source: &str, program: &Path, agent: Provider, data_dir: &Path) -> String {
    let hook = json!([
        program.to_string_lossy(),
        "hook",
        agent.name(),
        "--data-dir",
        data_dir.to_string_lossy()
    ]);
    source.replace("__VIBEFACE_HOOK__", &hook.to_string())
}

pub fn hook_command(program: &Path, agent: Provider, data_dir: &Path) -> String {
    let quote = |text: &str| format!("'{}'", text.replace('\'', r"'\''"));
    format!(
        "{} hook {} --data-dir {}",
        quote(&program.to_string_lossy()),
        agent.name(),
        quote(&data_dir.to_string_lossy())
    )
}

fn ours(entry: &Value, agent: Provider) -> bool {
    entry["hooks"].as_array().is_some_and(|hooks| {
        hooks.iter().any(|hook| {
            hook["command"].as_str().is_some_and(|command| {
                command.contains(&format!(" hook {} ", agent.name()))
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
        // A Claude Code permission request waits for the bot's answer.
        let timeout = match (file.agent, *event) {
            (Provider::Claude, "PermissionRequest") => {
                crate::runtime::HOOK_ANSWER_WITHIN.as_secs() + 15
            }
            _ => 10,
        };
        entries.push(json!({"hooks":[{"type":"command","command":command,"timeout":timeout}]}));
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
        let hook = from_codex_hook(&json!({"hook_event_name":"Stop","session_id":"t1","turn_id":"u1","cwd":"/p","last_assistant_message":"done"}))
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
        assert_eq!(
            from_claude_hook(
                &json!({"hook_event_name":"Notification","session_id":"c",
                    "notification_type":"permission_prompt","message":"Claude needs your permission"}),
                10,
            ),
            None,
            "the prompt a held permission request falls back to does not wake the Agent again"
        );
        let asks = |command: &str| {
            from_claude_hook(
                &json!({"hook_event_name":"PermissionRequest","session_id":"c",
                    "tool_name":"Bash","tool_input":{"command":command}}),
                10,
            )
            .unwrap()
            .turn_id
        };
        assert_ne!(
            asks("ls"),
            asks("pwd"),
            "concurrent requests are told apart"
        );
        assert_eq!(asks("ls"), asks("ls"));
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
        let codex_home = Path::new("/srv/codex");
        let [codex, claude] = &hook_files(home, codex_home)[..] else {
            panic!("Codex and Claude Code take hooks");
        };
        assert_eq!(codex.path, codex_home.join("hooks.json"));
        assert_eq!(
            claude.path,
            home.join(".claude/settings.json"),
            "CODEX_HOME moves only Codex's hooks"
        );
        let command = hook_command(
            Path::new("/opt/vibeface"),
            Provider::Claude,
            Path::new("/d'x"),
        );
        assert_eq!(command, r"'/opt/vibeface' hook claude --data-dir '/d'\''x'");
        let other = json!({"hooks":[{"type":"command","command":"orca notify"}]});
        let mut settings = json!({"model":"x","hooks":{"Stop":[other.clone()]}});
        assert_eq!(
            add_hooks(&mut settings, claude, &command).unwrap(),
            ["Stop", "StopFailure", "Notification", "PermissionRequest"]
        );
        assert!(
            add_hooks(&mut settings, claude, &command)
                .unwrap()
                .is_empty()
        );
        assert_eq!(settings["hooks"]["Stop"][0], other);
        assert_eq!(settings["hooks"]["Stop"].as_array().unwrap().len(), 2);
        assert_eq!(settings["hooks"]["Stop"][1]["hooks"][0]["timeout"], 10);
        assert_eq!(
            settings["hooks"]["PermissionRequest"][0]["hooks"][0]["timeout"], 45,
            "a permission request outlasts the wait for the bot's answer"
        );
        assert_eq!(installed(&settings, claude).len(), 4);
        assert!(installed(&settings, codex).is_empty());
        assert_eq!(remove_hooks(&mut settings, claude).len(), 4);
        assert_eq!(
            settings,
            json!({"model":"x","hooks":{"Stop":[other],"StopFailure":[],"Notification":[],
                "PermissionRequest":[]}})
        );
        assert!(add_hooks(&mut json!([]), codex, &command).is_err());
    }

    #[test]
    fn a_settled_pi_run_holds_its_session_unless_the_user_stopped_it() {
        let input = |stop: &str| {
            json!({"session_id":"s","turn_id":"e9","cwd":"/p","stop_reason":stop,
                "error":"429 rate limited","last_message":"done"})
        };
        let ended = AgentEvent {
            source: Source::Hook,
            agent: Provider::Pi,
            event: Kind::TurnEnd,
            status: Some("completed".into()),
            session_id: "s".into(),
            turn_id: "e9".into(),
            cwd: "/p".into(),
            last_message: "done".into(),
            hold: Some(RequestKind::Continue),
            ..Default::default()
        };
        assert_eq!(from_pi_extension(&input("stop")), Some(ended));
        let failed = from_pi_extension(&input("error")).unwrap();
        assert_eq!(
            (failed.status.as_deref(), failed.detail.as_str()),
            (Some("failed"), "429 rate limited")
        );
        for stop in ["aborted", "toolUse"] {
            assert!(from_pi_extension(&input(stop)).is_none(), "{stop}");
        }
        assert!(from_pi_extension(&json!({"session_id":"s"})).is_none());
    }

    #[test]
    fn the_opencode_plugin_reports_settled_runs_and_holds_requests() {
        let idle = json!({"event":"session.idle","session_id":"ses_1","turn_id":"msg_9",
            "cwd":"/p","status":"completed","last_message":"done"});
        let ended = AgentEvent {
            source: Source::Hook,
            agent: Provider::Opencode,
            event: Kind::TurnEnd,
            status: Some("completed".into()),
            session_id: "ses_1".into(),
            turn_id: "msg_9".into(),
            cwd: "/p".into(),
            last_message: "done".into(),
            ..Default::default()
        };
        assert_eq!(from_opencode_plugin(&idle), Some(ended));
        let asks = from_opencode_plugin(&json!({"event":"permission.asked","session_id":"ses_1",
            "request_id":"per_1","permission":"bash","patterns":["rm -rf build"]}))
        .unwrap();
        assert_eq!(
            (asks.event, asks.turn_id.as_str(), asks.hold),
            (Kind::NeedsInput, "per_1", Some(RequestKind::Permission))
        );
        assert!(asks.detail.starts_with("opencode asks to use bash."));
        assert!(!asks.detail.contains("rm -rf"), "commands stay out");
        let question = from_opencode_plugin(&json!({"event":"question.asked","session_id":"ses_1",
            "request_id":"que_1","questions":[{"question":"Which?","options":[{"label":"A"},
            {"label":"B"}]}]}))
        .unwrap();
        assert_eq!(question.hold, Some(RequestKind::Question));
        assert!(question.detail.ends_with("opencode asks: Which? [A | B]."));
        for input in [
            json!({"event":"session.idle","session_id":"ses_1","status":"busy"}),
            json!({"event":"permission.asked","session_id":"ses_1"}),
            json!({"event":"session.error","session_id":"ses_1"}),
        ] {
            assert!(from_opencode_plugin(&input).is_none(), "{input}");
        }
    }
}
