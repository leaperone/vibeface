//! Local scheduling state is never reconstructed by importing shared conversation history.
//! A job is keyed by `(conversation, message_id | work_id)`, so retries and sync never add a second one.
use crate::{
    model::{Schedule, VersionVector, WorkOutcome},
    providers::Provider,
    work::Signal,
};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use vibeface_bridge::failure::{ExecFailure, FailureKind, ProviderStage};

pub const MAX_PENDING_JOBS: usize = 256;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TriggerKind {
    Human,
    /// The user's answer to a question this conversation asked; it changes no settings.
    Answer,
    WorkResult,
    /// A `task` work this Node accepted; its turn ends in `WorkFinished`, not a reply.
    AgentTask,
    /// A due time of a schedule of this conversation: a routine, or a reminder.
    Schedule,
    /// Another bot's message to this one.
    BotMessage,
}

pub fn message_trigger(message_id: &str) -> String {
    format!("message:{message_id}")
}

pub fn work_trigger(work_id: &str) -> String {
    format!("work:{work_id}")
}

pub fn task_trigger(work_id: &str) -> String {
    format!("task:{work_id}")
}

/// What an agent task's coordination job did that its work must follow.
pub(crate) enum TaskJobChange<'a> {
    /// The turn ended, or was cancelled before it started; the reply is the result.
    Ended {
        outcome: &'a JobOutcome,
        text: &'a str,
    },
}

/// The one place an agent task's job state reaches its work: the work and the signal a change
/// of job `trigger` implies, or `None` when the job runs no agent task.
pub(crate) fn task_work_signal<'a>(
    trigger: &'a str,
    change: TaskJobChange,
) -> Option<(&'a str, Signal)> {
    let work_id = trigger.strip_prefix("task:")?;
    let signal = match change {
        TaskJobChange::Ended { outcome, text } => {
            let (outcome, reason, failure) = match outcome {
                JobOutcome::Completed => (WorkOutcome::Completed, None, None),
                JobOutcome::Failed(failure) => (
                    WorkOutcome::Failed,
                    Some(failure.detail.clone()),
                    Some(failure.kind),
                ),
                JobOutcome::Cancelled(evidence) => {
                    (WorkOutcome::Interrupted, Some(evidence.clone()), None)
                }
                JobOutcome::Interrupted {
                    failure,
                    process_unconfirmed,
                } => (
                    if *process_unconfirmed {
                        WorkOutcome::Unverified
                    } else {
                        WorkOutcome::Interrupted
                    },
                    Some(failure.detail.clone()),
                    Some(failure.kind),
                ),
            };
            Signal::Concluded {
                outcome,
                text: text.into(),
                reason,
                failure,
            }
        }
    };
    Some((work_id, signal))
}

/// A message another bot wrote to this conversation.
pub fn bot_message_trigger(message_id: &str) -> String {
    format!("bot_message:{message_id}")
}

/// One due time of a schedule. It also keys the job and is the reply's `in_reply_to`.
pub fn schedule_trigger(schedule_id: &str, slot_ms: i64) -> String {
    format!("schedule:{schedule_id}:{slot_ms}")
}

/// `(schedule_id, slot_ms)` of a schedule trigger in its single canonical spelling.
pub fn parse_schedule_trigger(trigger: &str) -> Option<(&str, i64)> {
    let (schedule_id, slot) = trigger.strip_prefix("schedule:")?.split_once(':')?;
    vibeface_protocol::validate_id(schedule_id).ok()?;
    let slot_ms: i64 = slot.parse().ok().filter(|slot| *slot >= 0)?;
    (slot_ms.to_string() == slot).then_some((schedule_id, slot_ms))
}

/// The newest due time at or before `now`; earlier missed times are never run. A one-shot
/// schedule has a single time.
pub fn latest_slot(schedule: &Schedule, now: i64) -> Option<i64> {
    let since = now
        .checked_sub(schedule.first_run_at_ms)
        .filter(|since| *since >= 0)?;
    let Some(every) = schedule.every_minutes else {
        return Some(schedule.first_run_at_ms);
    };
    let interval = i64::from(every.max(1)) * 60_000;
    Some(schedule.first_run_at_ms + since / interval * interval)
}

/// The first due time at or after `now`; `None` while paused or once a one-shot time passed.
pub fn next_run(schedule: &Schedule, now: i64) -> Option<i64> {
    if !schedule.enabled {
        return None;
    }
    let Some(latest) = latest_slot(schedule, now) else {
        return Some(schedule.first_run_at_ms);
    };
    if latest == now {
        return Some(latest);
    }
    let every = schedule.every_minutes?;
    Some(latest.saturating_add(i64::from(every) * 60_000))
}

/// Whether the schedule still has a time ahead: repeating, or a one-shot not yet due. Only these
/// count toward a bot's limit, show in lists and reach the prompt.
pub fn ahead(schedule: &Schedule, now: i64) -> bool {
    schedule.every_minutes.is_some() || schedule.first_run_at_ms >= now
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ManagerAttempt {
    pub id: String,
    pub boot: String,
    pub permission_version: u64,
}

/// The running turn a user message was steered into, and the provider running it.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SteerTarget {
    pub provider: Provider,
    /// The attempt of the reply the message joined.
    pub attempt_id: String,
    pub thread_id: String,
    pub turn_id: String,
}

/// What a bot is doing on the device that schedules it, for its avatar in the bot list.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub enum Busy {
    /// A message waits for delivery, in the queue, or claimed with no reply started yet.
    Thinking,
    /// A reply is running or being stopped.
    Replying,
}

/// How the reply a message joined ended.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SteerOutcome {
    /// The turn holding the message completed.
    Answered,
    /// The message is in a turn that stopped, failed or was interrupted.
    Unfinished,
    /// The thread could not be read, or still showed the turn running, in time; whether it
    /// joined is unknown. Stopping the message after its reply ended also ends it here.
    Unknown,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum JobState {
    Queued,
    /// Sent, or about to be sent, as `turn/steer` into `into`. It is settled from the thread
    /// once that reply's attempt ended, never sent again. After that, `unread_since_ms` is when
    /// reading the thread first failed and `running_since_ms` when a read first still showed the
    /// target turn running.
    Steering {
        into: SteerTarget,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        unread_since_ms: Option<i64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        running_since_ms: Option<i64>,
    },
    /// Joined the reply `into`; it gets no reply of its own.
    Steered {
        into: SteerTarget,
        outcome: SteerOutcome,
    },
    Claimed {
        attempt: ManagerAttempt,
    },
    Running {
        attempt: ManagerAttempt,
    },
    Cancelling {
        attempt: ManagerAttempt,
    },
    Reconciling {
        attempt: ManagerAttempt,
        failure: ExecFailure,
    },
    Completed {
        attempt: ManagerAttempt,
    },
    Failed {
        attempt: ManagerAttempt,
        failure: ExecFailure,
    },
    Cancelled {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        attempt: Option<ManagerAttempt>,
        evidence: String,
    },
    Interrupted {
        attempt: ManagerAttempt,
        failure: ExecFailure,
    },
}

impl JobState {
    pub fn name(&self) -> &'static str {
        match self {
            Self::Queued => "queued",
            Self::Steering { .. } => "steering",
            Self::Steered { .. } => "steered",
            Self::Claimed { .. } => "claimed",
            Self::Running { .. } => "running",
            Self::Cancelling { .. } => "cancelling",
            Self::Reconciling { .. } => "reconciling",
            Self::Completed { .. } => "completed",
            Self::Failed { .. } => "failed",
            Self::Cancelled { .. } => "cancelled",
            Self::Interrupted { .. } => "interrupted",
        }
    }
    pub fn attempt(&self) -> Option<&ManagerAttempt> {
        match self {
            Self::Queued | Self::Steering { .. } | Self::Steered { .. } => None,
            Self::Cancelled { attempt, .. } => attempt.as_ref(),
            Self::Claimed { attempt }
            | Self::Running { attempt }
            | Self::Cancelling { attempt }
            | Self::Reconciling { attempt, .. }
            | Self::Completed { attempt }
            | Self::Failed { attempt, .. }
            | Self::Interrupted { attempt, .. } => Some(attempt),
        }
    }
    pub fn terminal(&self) -> bool {
        matches!(
            self,
            Self::Completed { .. }
                | Self::Steered { .. }
                | Self::Failed { .. }
                | Self::Cancelled { .. }
                | Self::Interrupted { .. }
        )
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RuntimeSelection {
    pub provider: Provider,
    pub model: Option<String>,
    pub reasoning_effort: Option<String>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TurnExecution {
    NotStarted,
    Started,
    Unknown,
}

/// Safe submitted configuration and execution evidence. Native identifiers and credentials stay local.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TurnRuntime {
    pub provider: Provider,
    pub model: Option<String>,
    pub reasoning_effort: Option<String>,
    pub generation: String,
    pub session_generation: u64,
    pub delivery_id: String,
    pub stage: ProviderStage,
    pub execution: TurnExecution,
    pub attempted_at_ms: i64,
    pub switched_from: Option<RuntimeSelection>,
    pub switch_reason: Option<FailureKind>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SchedulerJob {
    pub conversation_id: String,
    /// `message:<message_id>`, `work:<work_id>`, `task:<work_id>`,
    /// `bot_message:<message_id>` or `schedule:<schedule_id>:<slot_ms>`.
    pub trigger: String,
    pub kind: TriggerKind,
    pub sequence: u64,
    /// The version vector the turn reads, taken again when it is claimed; later events are not
    /// part of its prompt.
    pub basis: VersionVector,
    /// The user message this turn ultimately answers, an agent task's own work ID, a peer
    /// message, or a schedule run's trigger.
    pub reply_to: String,
    /// The bot reply's message ID, fixed at admission so retries reuse it.
    pub message_id: String,
    pub runtime: Option<TurnRuntime>,
    #[serde(skip)]
    pub pending_check: bool,
    #[serde(skip)]
    pub tool_calls: bool,
    #[serde(flatten)]
    pub state: JobState,
}

impl SchedulerJob {
    pub fn retryable(&self) -> bool {
        matches!(&self.state, JobState::Failed { failure, .. }
            if !matches!(failure.kind, FailureKind::Restarted | FailureKind::Lost | FailureKind::Unknown))
            && self
                .runtime
                .as_ref()
                .is_some_and(|runtime| runtime.execution == TurnExecution::NotStarted)
            && !self.pending_check
            && !self.tool_calls
    }

    pub fn receipt(&self, network: &str) -> AdmissionReceipt {
        AdmissionReceipt {
            network_id: network.into(),
            conversation_id: self.conversation_id.clone(),
            trigger: self.trigger.clone(),
            state: self.state.name().into(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct AdmissionReceipt {
    pub network_id: String,
    pub conversation_id: String,
    pub trigger: String,
    pub state: String,
}

/// A user message or work result for the conversation's owner, kept until the owner returns a
/// receipt for its trigger. The owner is read at delivery time, so a switch redirects it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OutboxEntry {
    pub conversation_id: String,
    pub trigger: String,
}

impl OutboxEntry {
    pub fn receipt(&self, network: &str) -> AdmissionReceipt {
        AdmissionReceipt {
            network_id: network.into(),
            conversation_id: self.conversation_id.clone(),
            trigger: self.trigger.clone(),
            state: "pending_delivery".into(),
        }
    }
}

/// A queued user message that can join the running reply `into`.
#[derive(Debug, Clone)]
pub struct SteerCandidate {
    pub conversation_id: String,
    pub trigger: String,
    pub message_id: String,
    pub text: String,
    pub into: SteerTarget,
}

#[derive(Debug, Clone)]
pub struct ClaimedJob {
    pub job: SchedulerJob,
    pub previous_native_id: Option<String>,
    /// The tool token the resumed native session's Shell already carries.
    pub previous_token: Option<String>,
    /// What the resumed native session has already seen; the prompt carries only the rest.
    pub covers: Option<VersionVector>,
    /// The bot and works the resumed native session was last shown; empty with no `covers`.
    pub shown: Shown,
    pub session_generation: u64,
    pub handoff_revision: u64,
    pub handoff_source: VersionVector,
    pub previous_runtime: Option<RuntimeSelection>,
}

/// Digests of the bot and of each work a Codex thread was last shown, kept on this Node with
/// the session, so a resumed turn sends only what changed.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Shown {
    pub bot: Option<String>,
    pub works: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "state", content = "detail", rename_all = "snake_case")]
pub enum JobOutcome {
    Completed,
    Failed(ExecFailure),
    Cancelled(String),
    /// `process_unconfirmed` records a pending check for the SDK process in the same transaction.
    Interrupted {
        failure: ExecFailure,
        process_unconfirmed: bool,
    },
}

#[derive(Debug, Clone)]
pub(crate) struct SchedulerSession {
    pub conversation_id: String,
    pub next_sequence: u64,
    pub active: Option<String>,
    /// The SDK's own session, cached for this conversation on this Node only. `covers` is the
    /// basis it has seen and `owner_epoch` the ownership it was built under.
    pub native_id: Option<String>,
    /// The tool token set in the native session's Shell when it was created. A Codex thread
    /// keeps the environment it was loaded with, so every turn of it presents this token.
    pub native_token: Option<String>,
    pub covers: VersionVector,
    /// Moves with `covers`: what the native session's turns were shown of the bot and works.
    pub shown: Shown,
    pub owner_epoch: u64,
    pub sdk_config: Option<String>,
    pub permission_version: u64,
    pub session_generation: u64,
    pub handoff_revision: u64,
    pub handoff_source: VersionVector,
}

impl SchedulerSession {
    pub fn new(conversation: &str) -> Self {
        Self {
            conversation_id: conversation.into(),
            next_sequence: 1,
            active: None,
            native_id: None,
            native_token: None,
            covers: VersionVector::new(),
            shown: Shown::default(),
            owner_epoch: 0,
            sdk_config: None,
            permission_version: 1,
            session_generation: 0,
            handoff_revision: 0,
            handoff_source: VersionVector::new(),
        }
    }
}

/// A coordination turn whose result is unknown. Provider history can settle its outcome;
/// process exit alone cannot, so unavailable history leaves the check for the user.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PendingCheck {
    pub check_id: String,
    pub conversation_id: String,
    pub trigger: String,
    pub attempt_id: String,
    pub detail: String,
    pub process_groups: Vec<u32>,
    pub codex_turn: Option<ReplyTurn>,
    pub created_at_ms: i64,
}

/// Where a reply ran on an App Server: `server` as `CodexTarget::server` names it, the thread,
/// and the turn once it is known.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ReplyTurn {
    pub server: String,
    pub thread: String,
    pub turn: Option<String>,
}

impl PendingCheck {
    pub fn id(attempt_id: &str) -> String {
        format!("manager:{attempt_id}")
    }
}
