use super::session::{Profile, event_payload, operation_event, require_accepted};
use crate::manage::ClientAccess;
use anyhow::{Context, Result, bail, ensure};
use clap::{Args, ValueEnum};
use serde::Serialize;
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, HashMap, HashSet},
    io::{BufRead, IsTerminal, Read, Write},
    path::{Path, PathBuf},
    time::Duration,
};
use tokio::{
    sync::{broadcast, mpsc},
    time::Instant,
};
use vibeface_client::{
    Action, ClientEvent, ClientReader, OperationSnapshot, OperationState, ReadRequest,
    history::{HistoryRequest, HistorySection},
    wire::{InputIntent, InputRequest, InputRoute, InputTarget},
};

const MAX_TEXT: usize = 65_536;
const MAX_ATTACHMENT: u64 = 64 * 1024 * 1024;
const HELP: &str = "/help                 Show commands\n/attach PATH          Attach a file to the next message\n/questions            List open questions\n/answer ID TEXT       Answer a question\n/stop-work ID         Request an explicit work stop\n/stop-reply TRIGGER   Request an explicit reply stop, e.g. message:ID\n/history              Show recent messages\n/quit                 Detach from the chat";

#[derive(Args)]
pub struct ChatOptions {
    #[command(flatten)]
    pub access: ClientAccess,
    /// Bot ID or an exact, unique Bot name.
    #[arg(long, required_unless_present = "conversation")]
    pub bot: Option<String>,
    /// Exact conversation ID. When --bot is set, both must identify the same Bot.
    #[arg(long)]
    pub conversation: Option<String>,
    /// Send one message and exit. Add --wait to wait for its response.
    #[arg(long, conflicts_with_all = ["text_file", "stdin", "interactive"])]
    pub message: Option<String>,
    /// Read one message from a UTF-8 file, or from stdin with '-'.
    #[arg(long, conflicts_with_all = ["stdin", "interactive"])]
    pub text_file: Option<PathBuf>,
    /// Read one UTF-8 message from stdin until EOF.
    #[arg(long, conflicts_with = "interactive")]
    pub stdin: bool,
    /// File to attach. Repeat this option for multiple files.
    #[arg(long = "attach")]
    pub attachments: Vec<PathBuf>,
    /// Stable message ID for one-shot retries. Reuse the exact same content and target.
    #[arg(long, conflicts_with = "interactive")]
    pub message_id: Option<String>,
    /// Wait for the original input's reply or a decision it requires.
    #[arg(long)]
    pub wait: bool,
    /// Response timeout in seconds. Zero waits without a deadline.
    #[arg(long, default_value_t = 300)]
    pub timeout: u64,
    /// Read chat commands and messages line by line, including when stdin is a pipe.
    #[arg(long)]
    pub interactive: bool,
    /// Typed InputTarget JSON. Defaults to the selected Bot's main conversation.
    #[arg(long, value_name = "JSON")]
    pub target: Option<String>,
    /// Input scheduling intent.
    #[arg(long, value_enum, default_value = "continue")]
    pub intent: Intent,
    /// Required revision of a task or another revisioned input destination.
    #[arg(long)]
    pub expected_revision: Option<u64>,
}

#[derive(Clone, Copy, ValueEnum)]
pub enum Intent {
    Continue,
    #[value(name = "new_task")]
    NewTask,
    Defer,
}
impl From<Intent> for InputIntent {
    fn from(value: Intent) -> Self {
        match value {
            Intent::Continue => Self::Continue,
            Intent::NewTask => Self::NewTask,
            Intent::Defer => Self::Defer,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Bot {
    id: String,
    conversation: String,
    name: String,
}

fn select_bot(home: &Value, selector: Option<&str>, conversation: Option<&str>) -> Result<Bot> {
    let bots = home["bots"]
        .as_array()
        .context("Node returned no Bot list")?;
    let bots = bots
        .iter()
        .map(|bot| -> Result<Bot> {
            Ok(Bot {
                id: bot["bot_id"].as_str().context("Bot has no ID")?.into(),
                conversation: bot["conversation_id"]
                    .as_str()
                    .context("Bot has no conversation ID")?
                    .into(),
                name: bot["name"].as_str().context("Bot has no name")?.into(),
            })
        })
        .collect::<Result<Vec<_>>>()?;
    let matches: Vec<&Bot> = match selector {
        Some(selector) => {
            let exact: Vec<_> = bots.iter().filter(|bot| bot.id == selector).collect();
            if exact.is_empty() {
                bots.iter().filter(|bot| bot.name == selector).collect()
            } else {
                exact
            }
        }
        None => bots
            .iter()
            .filter(|bot| Some(bot.conversation.as_str()) == conversation)
            .collect(),
    };
    ensure!(
        !matches.is_empty(),
        "no Bot matches the supplied Bot or conversation selector"
    );
    if matches.len() > 1 {
        let candidates = matches
            .iter()
            .map(|bot| {
                format!(
                    "{} ({}, conversation {})",
                    safe_text(&bot.name),
                    bot.id,
                    bot.conversation
                )
            })
            .collect::<Vec<_>>()
            .join(", ");
        bail!("Bot name is ambiguous; use an exact Bot ID. Candidates: {candidates}");
    }
    let bot = (*matches[0]).clone();
    ensure!(
        conversation.is_none_or(|id| id == bot.conversation),
        "--conversation does not belong to the selected Bot"
    );
    validate_id(&bot.id)?;
    validate_id(&bot.conversation)?;
    Ok(bot)
}

fn validate_id(id: &str) -> Result<()> {
    vibeface_protocol::validate_id(id).map_err(anyhow::Error::msg)
}

fn parse_target(text: Option<&str>) -> Result<Option<InputTarget>> {
    text.map(|text| {
        ensure!(text.len() <= 4096, "--target JSON exceeds 4096 bytes");
        let target: InputTarget = serde_json::from_str(text)
            .context("--target must be a typed InputTarget JSON object")?;
        let ids: Vec<&str> = match &target {
            InputTarget::BotMain { bot_id } => vec![bot_id],
            InputTarget::Task { task_id } => vec![task_id],
            InputTarget::Work { work_id } => vec![work_id],
            InputTarget::Routine { bot_id, trigger } => {
                ensure!(
                    vibeface_protocol::valid_business_id(trigger),
                    "routine trigger is invalid"
                );
                vec![bot_id]
            }
            InputTarget::ChannelMember { channel_id, bot_id } => vec![channel_id, bot_id],
            InputTarget::ChannelTask {
                channel_id,
                task_id,
                bot_id,
            } => vec![channel_id, task_id, bot_id],
        };
        for id in ids {
            validate_id(id)?;
        }
        Ok(target)
    })
    .transpose()
}

fn validate_target_bot(target: &InputTarget, bot: &Bot) -> Result<()> {
    let target_bot = match target {
        InputTarget::BotMain { bot_id }
        | InputTarget::Routine { bot_id, .. }
        | InputTarget::ChannelMember { bot_id, .. }
        | InputTarget::ChannelTask { bot_id, .. } => Some(bot_id),
        InputTarget::Task { .. } | InputTarget::Work { .. } => None,
    };
    ensure!(
        target_bot.is_none_or(|id| id == &bot.id),
        "--target names a different Bot than --bot/--conversation"
    );
    Ok(())
}

fn validate_attachment(path: &Path) -> Result<()> {
    let metadata = std::fs::metadata(path)
        .with_context(|| format!("cannot read attachment {}", path.display()))?;
    ensure!(
        metadata.is_file() && metadata.len() <= MAX_ATTACHMENT,
        "attachment must be a file of at most 64 MiB: {}",
        path.display()
    );
    ensure!(
        path.file_name().and_then(|name| name.to_str()).is_some(),
        "attachment needs a UTF-8 file name"
    );
    Ok(())
}

fn read_text(reader: impl Read) -> Result<String> {
    let mut bytes = Vec::new();
    reader.take(MAX_TEXT as u64 + 1).read_to_end(&mut bytes)?;
    ensure!(bytes.len() <= MAX_TEXT, "message exceeds 65536 bytes");
    String::from_utf8(bytes).context("message is not UTF-8")
}

fn message_text(options: &ChatOptions) -> Result<String> {
    if let Some(text) = &options.message {
        return Ok(text.clone());
    }
    if let Some(path) = &options.text_file
        && path != Path::new("-")
    {
        return read_text(
            std::fs::File::open(path).with_context(|| format!("cannot open {}", path.display()))?,
        );
    }
    read_text(std::io::stdin().lock())
}

fn make_input(
    id: String,
    target: &InputTarget,
    options: &ChatOptions,
    text: String,
    attachment_count: usize,
) -> Result<InputRequest> {
    validate_id(&id)?;
    ensure!(text.len() <= MAX_TEXT, "message exceeds 65536 bytes");
    ensure!(
        !text.trim().is_empty() || attachment_count > 0,
        "message needs text or an attachment"
    );
    Ok(InputRequest {
        message_id: id,
        route: InputRoute {
            target: target.clone(),
            intent: options.intent.into(),
            sequence: 0,
            expected_revision: options.expected_revision,
        },
        text,
        attachments: Vec::new(),
        report_results: true,
    })
}

#[derive(Clone, Copy, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum State {
    Accepted,
    Queued,
    Running,
    NeedsDecision,
    Unknown,
    Completed,
    Failed,
    Stopped,
}
impl State {
    fn terminal(self) -> bool {
        matches!(self, Self::Completed | Self::Failed | Self::Stopped)
    }
    fn label(self) -> &'static str {
        match self {
            Self::Accepted => "accepted",
            Self::Queued => "queued",
            Self::Running => "running",
            Self::NeedsDecision => "needs_decision",
            Self::Unknown => "unknown",
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Stopped => "stopped",
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
struct Observation {
    state: State,
    reply: Option<Value>,
    job_state: Option<String>,
    hold_reason: Option<String>,
    work_ids: Vec<String>,
    questions: Vec<Value>,
}
impl Default for Observation {
    fn default() -> Self {
        Self {
            state: State::Unknown,
            reply: None,
            job_state: None,
            hold_reason: None,
            work_ids: Vec::new(),
            questions: Vec::new(),
        }
    }
}

const JOB_PATHS: [&str; 4] = [
    "/scheduler/remote/jobs",
    "/scheduler/remote/scope_jobs",
    "/scheduler/jobs",
    "/scheduler/scope_jobs",
];

fn rows<'a>(snapshot: &'a Value, path: &str) -> impl Iterator<Item = &'a Value> {
    snapshot
        .pointer(path)
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
}

fn matching_jobs<'a>(snapshot: &'a Value, message_id: &str) -> Vec<&'a Value> {
    let trigger = format!("message:{message_id}");
    JOB_PATHS
        .iter()
        .flat_map(|path| rows(snapshot, path))
        .filter(|job| job["trigger"] == trigger)
        .collect()
}

fn question_matches_input(snapshot: &Value, message_id: &str, question: &Value) -> bool {
    if !question["answer"].is_null() || !question["closed"].is_null() {
        return false;
    }
    let trigger = format!("message:{message_id}");
    // The same device and provider session prove that a held request belongs to this work.
    if rows(snapshot, "/works").any(|work| {
        work["trigger"] == trigger
            && work["outcome"].is_null()
            && work["state"] != "finished"
            && work["thread_id"].is_string()
            && question["about"]["request_id"].is_string()
            && question["about"]["session_id"] == work["thread_id"]
            && question["about"]["node"] == work["target_node"]
    }) {
        return true;
    }
    let jobs = matching_jobs(snapshot, message_id);
    let Some(job) = jobs.first() else {
        return false;
    };
    if !matches!(
        job["state"].as_str(),
        Some("running" | "steering" | "claimed")
    ) {
        return false;
    }
    // A registered destination has one active turn. Exclude questions predating this input
    // and questions from other destinations or devices in the same conversation.
    let Some(input) = rows(snapshot, "/messages").find(|row| row["message_id"] == message_id)
    else {
        return false;
    };
    let time_matches = match (
        input["created_at_ms"].as_i64(),
        question["created_at_ms"].as_i64(),
    ) {
        (Some(input), Some(question)) => question >= input,
        _ => false,
    };
    let owner = job["source_node"]
        .as_str()
        .or_else(|| snapshot["conversation"]["owner"].as_str());
    input["target"].is_object()
        && question["target"] == input["target"]
        && time_matches
        && owner.is_some_and(|owner| question["about"]["node"] == owner)
}

fn observe(snapshot: &Value, message_id: &str) -> Observation {
    let trigger = format!("message:{message_id}");
    let jobs = matching_jobs(snapshot, message_id);
    let reply_ids: HashSet<_> = jobs
        .iter()
        .filter_map(|job| job["reply_message_id"].as_str())
        .collect();
    let reply = rows(snapshot, "/messages")
        .find(|message| {
            message["role"] == "assistant"
                && (message["in_reply_to"] == message_id
                    || message["message_id"]
                        .as_str()
                        .is_some_and(|id| reply_ids.contains(id)))
        })
        .cloned();
    let mut result = Observation {
        questions: rows(snapshot, "/questions")
            .filter(|question| question_matches_input(snapshot, message_id, question))
            .cloned()
            .collect(),
        reply,
        work_ids: rows(snapshot, "/works")
            .filter(|work| work["trigger"] == trigger)
            .filter_map(|work| work["work_id"].as_str().map(str::to_owned))
            .collect(),
        ..Observation::default()
    };
    let job = jobs.first().copied();
    result.job_state = job.and_then(|job| job["state"].as_str()).map(str::to_owned);
    result.hold_reason = job
        .and_then(|job| job["hold_reason"].as_str())
        .map(str::to_owned);
    if !result.questions.is_empty()
        || jobs
            .iter()
            .any(|job| job["pending_check"] == true || job["hold_reason"] == "pending_check")
        || rows(snapshot, "/checks").any(|check| check["trigger"] == trigger)
    {
        result.state = State::NeedsDecision;
        return result;
    }
    if let Some(reply) = &result.reply {
        result.state = match reply["status"].as_str() {
            Some("failed") => State::Failed,
            Some("stopped" | "interrupted") => State::Stopped,
            Some("completed") => State::Completed,
            _ => State::Unknown,
        };
        return result;
    }
    if snapshot["scheduler"]["remote"].is_object()
        && snapshot["scheduler"]["remote"]["reachable"] == false
    {
        return result;
    }
    result.state = match result.job_state.as_deref() {
        Some("queued" | "pending_delivery") => State::Queued,
        Some("claimed" | "running" | "steering" | "cancelling" | "publishing") => State::Running,
        Some("completed") => State::Completed,
        Some("failed" | "rejected") => State::Failed,
        Some("cancelled" | "interrupted") => State::Stopped,
        Some("steered") => match job.and_then(|job| job["outcome"].as_str()) {
            Some("answered") => State::Completed,
            Some("unfinished") => State::Stopped,
            _ => State::Unknown,
        },
        _ => State::Unknown,
    };
    result
}

/// Keep only matching historical rows. A bounded snapshot's omission is never completion evidence.
async fn add_history(
    reader: &ClientReader,
    snapshot: &mut Value,
    conversation: &str,
    section: HistorySection,
    matches: impl Fn(&Value) -> bool,
) -> Result<()> {
    let name = serde_json::to_value(section)?
        .as_str()
        .context("history section name")?
        .to_owned();
    let path = format!("/{}", section.snapshot_path().join("/"));
    if rows(snapshot, &path).any(&matches) {
        return Ok(());
    }
    let mut cursor = snapshot["history"][&name]["next_cursor"]
        .as_str()
        .map(str::to_owned);
    let mut seen = HashSet::new();
    while let Some(current) = cursor {
        ensure!(
            seen.insert(current.clone()),
            "Node repeated a conversation history cursor"
        );
        let page = reader
            .read(ReadRequest::ConversationHistory(HistoryRequest {
                conversation_id: conversation.to_owned(),
                section,
                cursor: Some(current),
                limit: 100,
            }))
            .await?;
        ensure!(
            page["conversation_id"] == conversation && page["section"] == name,
            "history page identifies a different conversation or section"
        );
        ensure!(
            page["history_authority"] == snapshot["history_authority"],
            "conversation history authority changed; refresh required"
        );
        let items = page["items"]
            .as_array()
            .context("history page has no items")?;
        let found: Vec<_> = items.iter().filter(|item| matches(item)).cloned().collect();
        if !found.is_empty() {
            snapshot
                .pointer_mut(&path)
                .and_then(Value::as_array_mut)
                .context("snapshot has no history section")?
                .extend(found);
            return Ok(());
        }
        cursor = page["next_cursor"].as_str().map(str::to_owned);
    }
    Ok(())
}

async fn enrich(
    reader: &ClientReader,
    snapshot: &mut Value,
    conversation: &str,
    message_id: &str,
) -> Result<()> {
    let trigger = format!("message:{message_id}");
    for section in [
        HistorySection::OwnerJobs,
        HistorySection::OwnerScopeJobs,
        HistorySection::Jobs,
        HistorySection::ScopeJobs,
        HistorySection::Works,
        HistorySection::Checks,
    ] {
        add_history(reader, snapshot, conversation, section, |row| {
            row["trigger"] == trigger
        })
        .await?;
    }
    let reply_ids: HashSet<String> = matching_jobs(snapshot, message_id)
        .iter()
        .filter_map(|job| job["reply_message_id"].as_str().map(str::to_owned))
        .collect();
    add_history(
        reader,
        snapshot,
        conversation,
        HistorySection::Messages,
        |row| {
            row["role"] == "assistant"
                && (row["in_reply_to"] == message_id
                    || row["message_id"]
                        .as_str()
                        .is_some_and(|id| reply_ids.contains(id)))
        },
    )
    .await?;
    add_history(
        reader,
        snapshot,
        conversation,
        HistorySection::Messages,
        |row| row["message_id"] == message_id,
    )
    .await?;
    let basis = snapshot.clone();
    add_history(
        reader,
        snapshot,
        conversation,
        HistorySection::Questions,
        |question| question_matches_input(&basis, message_id, question),
    )
    .await
}

fn safe_text(text: &str) -> String {
    text.chars()
        .filter(|c| {
            (!c.is_control() || matches!(c, '\n' | '\t'))
                && !matches!(c, '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}')
        })
        .collect()
}

fn show_message(profile: &Profile, message: &Value) -> Result<()> {
    if profile.json {
        return profile.print(&json!({"type":"message","message":message}));
    }
    println!(
        "{}: {}",
        safe_text(message["role"].as_str().unwrap_or("message")),
        safe_text(message["text"].as_str().unwrap_or(""))
    );
    for attachment in message["attachments"].as_array().into_iter().flatten() {
        eprintln!(
            "  attachment: {} ({})",
            safe_text(attachment["name"].as_str().unwrap_or("file")),
            safe_text(attachment["hash"].as_str().unwrap_or(""))
        );
    }
    Ok(())
}

fn show_history(
    profile: &Profile,
    snapshot: &Value,
    seen: &mut HashSet<String>,
    initial: bool,
) -> Result<()> {
    if initial && profile.json {
        profile.print(&json!({"type":"history","conversation_id":snapshot["conversation"]["conversation_id"],"messages":snapshot["messages"],"coverage":snapshot["history"]["messages"]}))?;
    }
    for message in rows(snapshot, "/messages") {
        if let Some(id) = message["message_id"].as_str() {
            let new = seen.insert(id.to_owned());
            if (initial && !profile.json) || (!initial && new && message["role"] != "user") {
                show_message(profile, message)?;
            }
        }
    }
    Ok(())
}

fn show_operation(profile: &Profile, operation: &OperationSnapshot) -> Result<()> {
    if profile.json {
        profile.print(&operation_event(operation))?;
    } else {
        eprintln!("Operation {}: {:?}", operation.id, operation.state);
    }
    Ok(())
}

fn show_state(
    profile: &Profile,
    conversation: &str,
    message_id: &str,
    observation: &Observation,
) -> Result<()> {
    if profile.json {
        profile.print(&json!({"type":"state","conversation_id":conversation,"message_id":message_id,
            "state":observation.state,"job_state":observation.job_state,"hold_reason":observation.hold_reason,"questions":observation.questions}))
    } else {
        for question in &observation.questions {
            eprintln!(
                "Question {}: {}",
                safe_text(question["question_id"].as_str().unwrap_or("")),
                safe_text(question["text"].as_str().unwrap_or(""))
            );
        }
        eprintln!(
            "Message {message_id}: {}{}",
            observation.state.label(),
            observation
                .hold_reason
                .as_ref()
                .map(|reason| format!(" ({})", safe_text(reason)))
                .unwrap_or_default()
        );
        Ok(())
    }
}

fn show_final(
    profile: &Profile,
    conversation: &str,
    message_id: &str,
    observation: &Observation,
    timed_out: bool,
    detached: bool,
) -> Result<()> {
    if profile.json {
        profile.print(&json!({"type":"final","conversation_id":conversation,"message_id":message_id,
            "state":observation.state,"job_state":observation.job_state,"hold_reason":observation.hold_reason,
            "reply":observation.reply,"questions":observation.questions,"timed_out":timed_out,"detached":detached}))
    } else {
        if timed_out {
            eprintln!(
                "Message {message_id}: timeout; original state is {}",
                observation.state.label()
            );
        } else if detached {
            eprintln!(
                "Detached from message {message_id}; state is {}",
                observation.state.label()
            );
        }
        Ok(())
    }
}

fn relevant(event: &Value, conversation: &str) -> bool {
    let event = event_payload(event);
    event["conversation_id"]
        .as_str()
        .or_else(|| event["scope_id"].as_str())
        .is_none_or(|id| id == conversation)
}

fn progress_update<'a>(
    event: &'a Value,
    conversation: &str,
    pending: &BTreeMap<String, Pending>,
    sequences: &mut HashMap<String, u64>,
) -> Option<&'a Value> {
    let payload = event_payload(event);
    if payload["conversation_id"] != conversation {
        return None;
    }
    let correlated = match payload["type"].as_str() {
        Some("manager_progress") => pending
            .keys()
            .any(|id| payload["trigger"] == format!("message:{id}")),
        Some("native_progress") => pending.values().any(|item| {
            item.observation
                .work_ids
                .iter()
                .any(|id| payload["work_id"] == *id)
        }),
        _ => false,
    };
    if !correlated {
        return None;
    }
    let sequence = payload["sequence"].as_u64().filter(|value| *value > 0)?;
    let key = json!([
        payload["type"],
        payload["source_node"],
        payload["target_node"],
        payload["trigger"],
        payload["work_id"],
        payload["invocation_id"]
    ])
    .to_string();
    if sequences.get(&key).is_some_and(|last| *last >= sequence) {
        return None;
    }
    sequences.insert(key, sequence);
    Some(payload)
}

fn refresh_required(event: &Value, conversation: &str) -> bool {
    relevant(event, conversation)
        && !matches!(
            event_payload(event)["type"].as_str(),
            Some("manager_progress" | "native_progress")
        )
}

fn show_progress(
    profile: &Profile,
    event: &ClientEvent,
    conversation: &str,
    pending: &BTreeMap<String, Pending>,
    sequences: &mut HashMap<String, u64>,
) -> Result<()> {
    let Some(payload) = progress_update(&event.payload, conversation, pending, sequences) else {
        return Ok(());
    };
    if profile.json {
        profile
            .print(&json!({"type":"progress","generation":event.generation,"payload":payload}))?;
    } else {
        let progress = &payload["progress"];
        let text = progress["text"]
            .as_str()
            .or_else(|| progress["status"].as_str())
            .or_else(|| progress["name"].as_str());
        if let Some(text) = text {
            eprintln!("Progress: {}", safe_text(text));
        }
    }
    Ok(())
}

struct Pending {
    observation: Observation,
    deadline: Option<Instant>,
}
impl Pending {
    fn new(seconds: u64) -> Result<Self> {
        let deadline = if seconds == 0 {
            None
        } else {
            Some(
                Instant::now()
                    .checked_add(Duration::from_secs(seconds))
                    .context("timeout is too large")?,
            )
        };
        Ok(Self {
            observation: Observation::default(),
            deadline,
        })
    }
}

async fn snapshot(reader: &ClientReader, conversation: &str) -> Result<Value> {
    let value = reader
        .read(ReadRequest::Conversation {
            conversation_id: conversation.to_owned(),
        })
        .await?;
    ensure!(
        value["conversation"]["conversation_id"] == conversation,
        "Node returned a different conversation"
    );
    Ok(value)
}

/// A replacement generation is subscribed before any durable state is reread. No write occurs here.
async fn reconnect(
    profile: &Profile,
    reader: &mut ClientReader,
    events: &mut broadcast::Receiver<ClientEvent>,
) -> Result<()> {
    reader.close();
    loop {
        tokio::time::sleep(Duration::from_secs(1)).await;
        match profile.reader().await {
            Ok(next) => {
                *events = next.subscribe();
                *reader = next;
                if profile.json {
                    profile
                        .print(&json!({"type":"reconnected","generation":reader.generation()}))?;
                } else {
                    eprintln!("Reconnected; reading the original messages' state.");
                }
                return Ok(());
            }
            Err(error) => eprintln!("Waiting to reconnect: {}", safe_text(&format!("{error:#}"))),
        }
    }
}

pub async fn run(options: ChatOptions) -> Result<()> {
    let terminal = std::io::stdin().is_terminal();
    let interactive = options.interactive
        || (terminal && options.message.is_none() && options.text_file.is_none() && !options.stdin);
    ensure!(
        !interactive || options.message_id.is_none(),
        "--message-id is only available for one-shot chat"
    );
    if let Some(id) = &options.message_id {
        validate_id(id)?;
    }
    if let Some(conversation) = &options.conversation {
        validate_id(conversation)?;
    }
    let target = parse_target(options.target.as_deref())?;
    for path in &options.attachments {
        validate_attachment(path)?;
    }
    let text = if interactive {
        None
    } else {
        let text = message_text(&options)?;
        ensure!(
            text.len() <= MAX_TEXT && (!text.trim().is_empty() || !options.attachments.is_empty()),
            "message needs text or an attachment within 65536 bytes"
        );
        Some(text)
    };
    let profile = Profile::open(&options.access)?;
    let mut reader = profile.reader().await?;
    let result: Result<()> = async {
        let bot = select_bot(
            &reader.read(ReadRequest::Home).await?,
            options.bot.as_deref(),
            options.conversation.as_deref(),
        )?;
        let target = target.unwrap_or_else(|| InputTarget::BotMain {
            bot_id: bot.id.clone(),
        });
        validate_target_bot(&target, &bot)?;
        let mut events = reader.subscribe();
        if interactive {
            interactive_chat(
                &profile,
                &options,
                &bot,
                &target,
                &mut reader,
                &mut events,
                terminal,
            )
            .await
        } else {
            let id = options
                .message_id
                .clone()
                .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
            let input = make_input(
                id.clone(),
                &target,
                &options,
                text.context("one-shot message is missing")?,
                options.attachments.len(),
            )?;
            let operation = profile
                .send_input(&bot.conversation, input, &options.attachments)
                .await?;
            show_operation(&profile, &operation)?;
            if options.wait {
                wait_for_reply(
                    &profile,
                    &bot.conversation,
                    &id,
                    options.timeout,
                    &mut reader,
                    &mut events,
                )
                .await
            } else {
                let state = if operation.state == OperationState::Accepted {
                    State::Accepted
                } else {
                    State::Unknown
                };
                show_final(
                    &profile,
                    &bot.conversation,
                    &id,
                    &Observation {
                        state,
                        ..Observation::default()
                    },
                    false,
                    false,
                )?;
                if !profile.json {
                    println!("Message {id}: {}", state.label());
                }
                require_accepted(&operation)
            }
        }
    }
    .await;
    reader.close();
    result
}

async fn wait_for_reply(
    profile: &Profile,
    conversation: &str,
    message_id: &str,
    seconds: u64,
    reader: &mut ClientReader,
    events: &mut broadcast::Receiver<ClientEvent>,
) -> Result<()> {
    let mut pending = BTreeMap::from([(message_id.to_owned(), Pending::new(seconds)?)]);
    let mut sequences = HashMap::new();
    let mut printed = None;
    let mut poll = tokio::time::interval(Duration::from_secs(1));
    let deadline = pending[message_id].deadline;
    enum Exit {
        Finished(Result<()>),
        Detached,
        TimedOut,
    }
    let exit = {
        let result = async {
            loop {
                let mut value = match snapshot(reader, conversation).await {
                    Ok(value) => value,
                    Err(error) => {
                        eprintln!(
                            "Reading the original message failed: {}",
                            safe_text(&format!("{error:#}"))
                        );
                        reconnect(profile, reader, events).await?;
                        sequences.clear();
                        continue;
                    }
                };
                if let Err(error) = enrich(reader, &mut value, conversation, message_id).await {
                    eprintln!(
                        "Refreshing the original message: {}",
                        safe_text(&format!("{error:#}"))
                    );
                    reconnect(profile, reader, events).await?;
                    continue;
                }
                let observation = observe(&value, message_id);
                pending
                    .get_mut(message_id)
                    .context("original pending message disappeared")?
                    .observation = observation.clone();
                if printed.as_ref() != Some(&observation) {
                    show_state(profile, conversation, message_id, &observation)?;
                    printed = Some(observation.clone());
                }
                if observation.state.terminal() || observation.state == State::NeedsDecision {
                    if !profile.json {
                        if let Some(reply) = &observation.reply {
                            show_message(profile, reply)?;
                        }
                        for question in &observation.questions {
                            eprintln!(
                                "Question {}: {}",
                                safe_text(question["question_id"].as_str().unwrap_or("")),
                                safe_text(question["text"].as_str().unwrap_or(""))
                            );
                        }
                    }
                    show_final(
                        profile,
                        conversation,
                        message_id,
                        &observation,
                        false,
                        false,
                    )?;
                    ensure!(
                        observation.state == State::Completed,
                        "message {message_id} ended in state {}; use /questions or `client read questions` for decisions",
                        observation.state.label()
                    );
                    return Ok(());
                }
                loop {
                    tokio::select! {
                        _ = poll.tick() => break,
                        event = events.recv() => match event {
                            Ok(event) if event.generation == reader.generation() => {
                                if matches!(event_payload(&event.payload)["type"].as_str(), Some("disconnected" | "resync_required" | "resync")) {
                                    reconnect(profile, reader, events).await?;
                                    sequences.clear();
                                    break;
                                }
                                show_progress(profile, &event, conversation, &pending, &mut sequences)?;
                                if refresh_required(&event.payload, conversation) { break; }
                            }
                            Ok(_) => {},
                            Err(_) => { reconnect(profile, reader, events).await?; sequences.clear(); break; }
                        }
                    }
                }
            }
        };
        tokio::pin!(result);
        tokio::select! {
            value = &mut result => Exit::Finished(value),
            signal = tokio::signal::ctrl_c() => { signal?; Exit::Detached }
            _ = async { match deadline { Some(at) => tokio::time::sleep_until(at).await, None => std::future::pending::<()>().await } } => Exit::TimedOut,
        }
    };
    let observation = &pending[message_id].observation;
    match exit {
        Exit::Finished(result) => result,
        Exit::Detached => {
            show_final(profile, conversation, message_id, observation, false, true)?;
            Ok(())
        }
        Exit::TimedOut => {
            show_final(profile, conversation, message_id, observation, true, false)?;
            bail!(
                "timed out waiting for message {message_id}; query the original message before retrying"
            )
        }
    }
}

fn input_lines() -> mpsc::Receiver<Result<String>> {
    let (sender, receiver) = mpsc::channel(16);
    // A plain thread can remain blocked in stdin after Ctrl-C without delaying Tokio shutdown.
    std::thread::spawn(move || {
        let stdin = std::io::stdin();
        let mut reader = stdin.lock();
        loop {
            let mut line = String::new();
            let result = (&mut reader).take(MAX_TEXT as u64 + 2).read_line(&mut line);
            let result = match result {
                Ok(0) => break,
                Ok(_) => {
                    if line.ends_with('\n') {
                        line.pop();
                        if line.ends_with('\r') {
                            line.pop();
                        }
                    }
                    if line.len() > MAX_TEXT {
                        Err(anyhow::anyhow!("input line exceeds 65536 bytes"))
                    } else {
                        Ok(line)
                    }
                }
                Err(error) => Err(error.into()),
            };
            let failed = result.is_err();
            if sender.blocking_send(result).is_err() || failed {
                break;
            }
        }
    });
    receiver
}

fn prompt(terminal: bool) {
    if terminal {
        eprint!("> ");
        let _ = std::io::stderr().flush();
    }
}

async fn interactive_chat(
    profile: &Profile,
    options: &ChatOptions,
    bot: &Bot,
    target: &InputTarget,
    reader: &mut ClientReader,
    events: &mut broadcast::Receiver<ClientEvent>,
    terminal: bool,
) -> Result<()> {
    let mut seen = HashSet::new();
    show_history(
        profile,
        &snapshot(reader, &bot.conversation).await?,
        &mut seen,
        true,
    )?;
    eprintln!(
        "Chat with {} ({}). /help lists commands. Ctrl-C detaches.",
        safe_text(&bot.name),
        bot.id
    );
    let mut lines = input_lines();
    let mut attachments = options.attachments.clone();
    let mut pending = BTreeMap::<String, Pending>::new();
    let mut sequences = HashMap::new();
    let mut poll = tokio::time::interval(Duration::from_secs(1));
    let mut dirty = false;
    prompt(terminal);
    loop {
        tokio::select! {
            signal = tokio::signal::ctrl_c() => { signal?; break; }
            line = lines.recv() => {
                let Some(line) = line else { break; };
                let line = line?;
                if line.trim() == "/quit" { break; }
                let result: Result<()> = async {
                    if line.starts_with('/') {
                        slash(profile, bot, &line, &mut attachments, reader, &mut seen).await?;
                    } else if !line.trim().is_empty() || !attachments.is_empty() {
                        let id = uuid::Uuid::new_v4().to_string();
                        let input = make_input(id.clone(), target, options, line, attachments.len())?;
                        let item = Pending::new(options.timeout)?;
                        let operation = profile.send_input(&bot.conversation, input, &attachments).await?;
                        attachments.clear();
                        show_operation(profile, &operation)?;
                        pending.insert(id, item);
                    }
                    Ok(())
                }.await;
                if let Err(error) = result {
                    if profile.json { profile.print(&json!({"type":"error","message":safe_text(&format!("{error:#}"))}))?; }
                    else { eprintln!("{}", safe_text(&format!("{error:#}"))); }
                }
                dirty = true;
                prompt(terminal);
            }
            event = events.recv() => {
                match event {
                    Ok(event) if event.generation == reader.generation() => {
                        if matches!(event_payload(&event.payload)["type"].as_str(), Some("disconnected" | "resync_required" | "resync")) {
                            tokio::select! {
                                result = reconnect(profile, reader, events) => result?,
                                signal = tokio::signal::ctrl_c() => { signal?; break; }
                            }
                            sequences.clear();
                            dirty = true;
                        } else {
                            show_progress(profile, &event, &bot.conversation, &pending, &mut sequences)?;
                            dirty |= refresh_required(&event.payload, &bot.conversation);
                        }
                    }
                    Ok(_) => {},
                    Err(_) => {
                        tokio::select! {
                            result = reconnect(profile, reader, events) => result?,
                            signal = tokio::signal::ctrl_c() => { signal?; break; }
                        }
                        sequences.clear();
                        dirty = true;
                    }
                }
            }
            _ = poll.tick() => {
                if !dirty && pending.is_empty() { continue; }
                dirty = false;
                let refresh = async {
                    let value = snapshot(reader, &bot.conversation).await?;
                    show_history(profile, &value, &mut seen, false)?;
                    let mut finished = Vec::new();
                    for (id, item) in &mut pending {
                        let mut value = value.clone();
                        enrich(reader, &mut value, &bot.conversation, id).await?;
                        let observation = observe(&value, id);
                        if observation != item.observation { show_state(profile, &bot.conversation, id, &observation)?; }
                        item.observation = observation;
                        let timed_out = item.deadline.is_some_and(|deadline| Instant::now() >= deadline);
                        if item.observation.state.terminal() || timed_out {
                            show_final(profile, &bot.conversation, id, &item.observation, timed_out, false)?;
                            finished.push(id.clone());
                        }
                    }
                    for id in finished { pending.remove(&id); }
                    Ok::<_, anyhow::Error>(())
                };
                let result = tokio::select! {
                    result = refresh => result,
                    signal = tokio::signal::ctrl_c() => { signal?; break; }
                };
                if let Err(error) = result {
                    eprintln!("Refreshing chat: {}", safe_text(&format!("{error:#}")));
                    dirty = true;
                    tokio::select! {
                        result = reconnect(profile, reader, events) => result?,
                        signal = tokio::signal::ctrl_c() => { signal?; break; }
                    }
                    sequences.clear();
                }
            }
        }
    }
    for (id, item) in pending {
        show_final(
            profile,
            &bot.conversation,
            &id,
            &item.observation,
            false,
            true,
        )?;
    }
    if profile.json {
        profile.print(&json!({"type":"detached","conversation_id":bot.conversation}))?;
    }
    Ok(())
}

async fn slash(
    profile: &Profile,
    bot: &Bot,
    line: &str,
    attachments: &mut Vec<PathBuf>,
    reader: &ClientReader,
    seen: &mut HashSet<String>,
) -> Result<()> {
    let (command, argument) = line
        .split_once(char::is_whitespace)
        .map(|(command, argument)| (command, argument.trim()))
        .unwrap_or((line, ""));
    let action = match command {
        "/help" => {
            ensure!(argument.is_empty(), "/help takes no arguments");
            eprintln!("{HELP}");
            return Ok(());
        }
        "/attach" => {
            ensure!(!argument.is_empty(), "usage: /attach PATH");
            let path = PathBuf::from(argument);
            validate_attachment(&path)?;
            attachments.push(path);
            eprintln!("Attached file to the next message.");
            return Ok(());
        }
        "/history" => {
            ensure!(argument.is_empty(), "/history takes no arguments");
            return show_history(
                profile,
                &snapshot(reader, &bot.conversation).await?,
                seen,
                true,
            );
        }
        "/questions" => {
            ensure!(argument.is_empty(), "/questions takes no arguments");
            let value = reader.read(ReadRequest::Questions).await?;
            let questions: Vec<_> = value["questions"]
                .as_array()
                .context("Node returned no questions list")?
                .iter()
                .filter(|question| {
                    question["conversation_id"] == bot.conversation
                        && question["answer"].is_null()
                        && question["closed"].is_null()
                })
                .cloned()
                .collect();
            if profile.json {
                profile.print(&json!({"type":"questions","conversation_id":bot.conversation,"questions":questions}))?;
            } else {
                if questions.is_empty() {
                    eprintln!("No open questions.");
                }
                for question in questions {
                    println!(
                        "Question {}: {}",
                        safe_text(question["question_id"].as_str().unwrap_or("")),
                        safe_text(question["text"].as_str().unwrap_or(""))
                    );
                    for choice in question["choices"].as_array().into_iter().flatten() {
                        println!("  {}", safe_text(choice["label"].as_str().unwrap_or("")));
                    }
                }
            }
            return Ok(());
        }
        "/answer" => {
            let (id, text) = argument
                .split_once(char::is_whitespace)
                .context("usage: /answer ID TEXT")?;
            validate_id(id)?;
            ensure!(
                !text.trim().is_empty() && text.len() <= MAX_TEXT,
                "answer needs text within 65536 bytes"
            );
            Action::Answer {
                conversation_id: bot.conversation.clone(),
                question_id: id.to_owned(),
                text: text.trim().to_owned(),
            }
        }
        "/stop-work" => {
            validate_id(argument).context("usage: /stop-work ID")?;
            Action::StopWork {
                conversation_id: bot.conversation.clone(),
                work_id: argument.to_owned(),
            }
        }
        "/stop-reply" => {
            ensure!(
                vibeface_protocol::valid_business_id(argument),
                "usage: /stop-reply TRIGGER, e.g. message:ID"
            );
            Action::StopReply {
                conversation_id: bot.conversation.clone(),
                trigger: argument.to_owned(),
            }
        }
        _ => bail!("unknown chat command; /help lists supported commands"),
    };
    let operation = profile.submit(action).await?;
    show_operation(profile, &operation)?;
    require_accepted(&operation)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exact_bot_id_wins_and_duplicate_names_list_candidates() {
        let home = json!({"bots":[
            {"bot_id":"first","conversation_id":"c1","name":"Writer"},
            {"bot_id":"second","conversation_id":"c2","name":"Writer"},
            {"bot_id":"Writer","conversation_id":"c3","name":"Other"}
        ]});
        assert_eq!(
            select_bot(&home, Some("Writer"), None)
                .unwrap()
                .conversation,
            "c3"
        );
        let duplicate = json!({"bots":home["bots"].as_array().unwrap()[..2]});
        let error = select_bot(&duplicate, Some("Writer"), None)
            .unwrap_err()
            .to_string();
        assert!(error.contains("first") && error.contains("second"));
        assert_eq!(select_bot(&home, None, Some("c2")).unwrap().id, "second");
        assert!(select_bot(&home, Some("first"), Some("c2")).is_err());
    }

    #[test]
    fn replies_and_progress_are_correlated_to_the_original_input() {
        let value = json!({"messages":[
            {"message_id":"unrelated","role":"assistant","in_reply_to":"other","status":"completed","text":"wrong reply"},
            {"message_id":"right","role":"assistant","in_reply_to":"input","status":"completed","text":"correct reply"}
        ],"scheduler":{"jobs":[{"trigger":"message:other","state":"completed"},{"trigger":"message:input","state":"running"}]}});
        assert_eq!(observe(&value, "input").state, State::Completed);
        assert_eq!(
            observe(&value, "input").reply.unwrap()["text"],
            "correct reply"
        );
        assert_eq!(observe(&value, "absent").state, State::Unknown);
        let linked = json!({"messages":[{"message_id":"reply","role":"assistant","in_reply_to":"earlier","status":"completed","text":"joined turn"}],
            "scheduler":{"scope_jobs":[{"trigger":"message:input","reply_message_id":"reply","state":"running"}]}});
        assert_eq!(
            observe(&linked, "input").reply.unwrap()["text"],
            "joined turn"
        );
    }

    #[test]
    fn wrapped_conversation_changes_refresh_only_the_selected_chat() {
        let event = json!({"type":"changed","payload":{
            "type":"conversation_delta","conversation_id":"chat",
            "messages":[{"message_id":"cli-during-watch","role":"user","text":"hello"}]
        }});
        assert!(relevant(&event, "chat"));
        assert!(refresh_required(&event, "chat"));
        assert!(!relevant(&event, "other"));
        assert!(!refresh_required(&event, "other"));
        assert_eq!(
            event_payload(&event)["messages"][0]["message_id"],
            "cli-during-watch"
        );
        let scoped = json!({"type":"changed","payload":{"type":"changed","scope_id":"chat"}});
        assert!(!relevant(&scoped, "other"));
    }

    #[test]
    fn wrapped_manager_progress_keeps_original_trigger_and_sequence() {
        let pending = BTreeMap::from([("input".to_owned(), Pending::new(0).unwrap())]);
        let mut sequences = HashMap::new();
        let mut event = json!({"type":"changed","payload":{
            "type":"manager_progress","conversation_id":"chat","trigger":"message:input",
            "source_node":"owner","invocation_id":"attempt","sequence":1,
            "progress":{"type":"text_delta","text":"reply text"}
        }});
        assert_eq!(
            progress_update(&event, "chat", &pending, &mut sequences).unwrap(),
            &json!({
                "type":"manager_progress","conversation_id":"chat","trigger":"message:input",
                "source_node":"owner","invocation_id":"attempt","sequence":1,
                "progress":{"type":"text_delta","text":"reply text"}
            })
        );
        assert!(!refresh_required(&event, "chat"));
        assert!(progress_update(&event, "chat", &pending, &mut sequences).is_none());
        event["payload"]["sequence"] = json!(2);
        event["payload"]["trigger"] = json!("message:unrelated");
        assert!(progress_update(&event, "chat", &pending, &mut sequences).is_none());
        event["payload"]["trigger"] = json!("message:input");
        assert!(progress_update(&event, "other", &pending, &mut sequences).is_none());
        assert_eq!(
            progress_update(&event, "chat", &pending, &mut sequences).unwrap()["sequence"],
            2
        );
    }

    #[test]
    fn wrapped_native_progress_requires_a_work_linked_to_the_original_input() {
        let mut item = Pending::new(0).unwrap();
        item.observation.work_ids.push("work".to_owned());
        let pending = BTreeMap::from([("input".to_owned(), item)]);
        let mut sequences = HashMap::new();
        let mut event = json!({"type":"changed","payload":{
            "type":"native_progress","conversation_id":"chat","work_id":"unrelated",
            "target_node":"worker","sequence":1,"progress":{"type":"status","text":"running"}
        }});
        assert!(progress_update(&event, "chat", &pending, &mut sequences).is_none());
        event["payload"]["work_id"] = json!("work");
        assert_eq!(
            progress_update(&event, "chat", &pending, &mut sequences).unwrap()["progress"],
            json!({"type":"status","text":"running"})
        );
        assert!(progress_update(&event, "chat", &pending, &mut sequences).is_none());
        assert!(!refresh_required(&event, "chat"));
    }

    #[test]
    fn pending_decisions_and_unknown_owners_cannot_be_reported_as_complete() {
        let mut value = json!({"messages":[],"scheduler":{"jobs":[{"trigger":"message:input","state":"queued","hold_reason":"active_turn"}],"remote":null},"checks":[]});
        assert_eq!(observe(&value, "input").state, State::Queued);
        value["scheduler"]["jobs"][0]["state"] = json!("running");
        assert_eq!(observe(&value, "input").state, State::Running);
        value["scheduler"]["jobs"][0]["pending_check"] = json!(true);
        assert_eq!(observe(&value, "input").state, State::NeedsDecision);
        value["scheduler"]["jobs"][0]["pending_check"] = json!(false);
        value["scheduler"]["remote"] = json!({"reachable":false});
        assert_eq!(observe(&value, "input").state, State::Unknown);
        value["checks"] = json!([{"trigger":"message:other"}]);
        assert_eq!(observe(&value, "input").state, State::Unknown);
        value["checks"] = json!([{"trigger":"message:input"}]);
        assert_eq!(observe(&value, "input").state, State::NeedsDecision);
    }

    #[test]
    fn decisions_require_the_original_active_destination_or_work_session() {
        let mut snapshot = json!({
            "conversation":{"conversation_id":"chat","owner":"owner"},
            "messages":[{"message_id":"input","role":"user","created_at_ms":10,"target":{"kind":"bot_main","bot_id":"bot"}}],
            "scheduler":{"jobs":[{"trigger":"message:input","state":"running","source_node":"owner"}]},
            "questions":[{"question_id":"question","answer":null,"closed":null,"created_at_ms":11,
                "target":{"kind":"task","task_id":"other"},"about":{"node":"owner"}}]
        });
        assert_eq!(observe(&snapshot, "input").state, State::Running);
        snapshot["questions"][0]["target"] = json!({"kind":"bot_main","bot_id":"bot"});
        assert_eq!(observe(&snapshot, "input").state, State::NeedsDecision);
        assert_eq!(
            observe(&snapshot, "input").questions[0]["question_id"],
            "question"
        );
        snapshot["questions"][0]["created_at_ms"] = json!(9);
        assert_eq!(observe(&snapshot, "input").state, State::Running);
        snapshot["questions"][0]["created_at_ms"] = json!(11);
        snapshot["scheduler"]["jobs"][0]["state"] = json!("queued");
        assert_eq!(observe(&snapshot, "input").state, State::Queued);
        snapshot["works"] = json!([{"work_id":"work","trigger":"message:input","state":"running","outcome":null,"thread_id":"session","target_node":"worker"}]);
        snapshot["questions"][0]["about"] =
            json!({"node":"worker","session_id":"session","request_id":"request"});
        assert_eq!(observe(&snapshot, "input").state, State::NeedsDecision);
    }

    #[test]
    fn steered_input_requires_a_recorded_outcome() {
        let mut value = json!({"scheduler":{"jobs":[{"trigger":"message:input","state":"steered","outcome":"unknown"}]}});
        assert_eq!(observe(&value, "input").state, State::Unknown);
        value["scheduler"]["jobs"][0]["outcome"] = json!("answered");
        assert_eq!(observe(&value, "input").state, State::Completed);
        value["scheduler"]["jobs"][0]["outcome"] = json!("unfinished");
        assert_eq!(observe(&value, "input").state, State::Stopped);
    }

    #[test]
    fn terminal_text_and_typed_targets_are_validated_at_the_boundary() {
        assert_eq!(
            safe_text("hello\x1b[2J\r\x07\u{009b}\u{202e}world\n\t"),
            "hello[2Jworld\n\t"
        );
        assert_eq!(read_text("你好\n".as_bytes()).unwrap(), "你好\n");
        assert!(read_text(vec![b'x'; MAX_TEXT + 1].as_slice()).is_err());
        assert!(read_text([0xff].as_slice()).is_err());
        assert!(parse_target(Some(r#"{"kind":"task","task_id":"task","typo":true}"#)).is_err());
        assert!(parse_target(Some(r#"{"kind":"task","task_id":"../escape"}"#)).is_err());
        assert_eq!(
            parse_target(Some(
                r#"{"kind":"routine","bot_id":"bot","trigger":"schedule:daily:1"}"#
            ))
            .unwrap(),
            Some(InputTarget::Routine {
                bot_id: "bot".into(),
                trigger: "schedule:daily:1".into()
            })
        );
    }
}
