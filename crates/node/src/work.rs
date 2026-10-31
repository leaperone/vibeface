//! The target Node's work state machine. It is the only authority for a work unit:
//! `transition` is the only source of Work events in the DAG, and the DAG projection,
//! the pending-check list and the Web state all derive from it.
use crate::{crypto::digest, model::*};
use serde::{Deserialize, Serialize};
use vibeface_bridge::{failure::ExecFailure, turn::TurnStatus};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct WorkSpec {
    pub work_id: String,
    pub conversation: String,
    /// The scheduler trigger whose turn dispatched this work.
    pub trigger: String,
    pub target: DeviceId,
    pub dispatcher: DeviceId,
    pub call_ref: String,
    pub kind: WorkKind,
    pub cwd: String,
    pub title: String,
    pub prompt: String,
    pub continues: Option<String>,
    pub parent: Option<String>,
    /// An existing App Server thread the prompt goes into: it steers the running turn or starts
    /// one. `None` starts a turn in a new thread, or in the thread of `continues`.
    pub thread: Option<String>,
}

impl WorkSpec {
    /// Retries of the same call keep the same identity, so they never start a second turn.
    pub fn id(conversation: &str, trigger: &str, call_ref: &str) -> String {
        format!(
            "work-{}",
            digest(&serde_json::to_vec(&(conversation, trigger, call_ref)).expect("strings"))
        )
    }
}

/// A dispatch to another Node, kept by the dispatcher until the target answers. Only the target
/// writes `WorkDispatched`, so the DAG never names work the target did not accept.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Dispatch {
    pub spec: WorkSpec,
    #[serde(flatten)]
    pub state: DispatchState,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum DispatchState {
    Pending,
    Accepted,
    Rejected {
        reason: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        failure: Option<FailureKind>,
    },
    /// The target can no longer answer (it was removed); whether it took the work is unknown.
    Unverified {
        reason: String,
    },
}

impl DispatchState {
    /// How the dispatch reads to the bot and in the conversation until the DAG names the work.
    pub fn name(&self) -> &'static str {
        match self {
            Self::Pending => "dispatch_waiting",
            Self::Accepted => "dispatch_accepted",
            Self::Rejected { .. } => "dispatch_rejected",
            Self::Unverified { .. } => "dispatch_unverified",
        }
    }
    /// Refused, or left unknown: the DAG will not report it, so the dispatcher does.
    pub fn ended(&self) -> bool {
        matches!(self, Self::Rejected { .. } | Self::Unverified { .. })
    }
    pub fn reason(&self) -> Option<&str> {
        match self {
            Self::Rejected { reason, .. } | Self::Unverified { reason } => Some(reason),
            Self::Pending | Self::Accepted => None,
        }
    }
    pub fn failure(&self) -> Option<FailureKind> {
        match self {
            Self::Rejected { failure, .. } => *failure,
            _ => None,
        }
    }
}

/// A stop the user asked for, stored before anything is sent. For another Node's work it is
/// delivered until that Node takes it or the work ends; for this Node's own work it is kept and
/// retried until the work ends. `timed_out` only shows that `deadline_ms` passed unconfirmed; it
/// never ends the retries.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct StopRequest {
    pub work_id: String,
    pub conversation: String,
    pub target: DeviceId,
    pub requested_at_ms: i64,
    pub deadline_ms: i64,
    pub timed_out: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Handle {
    pub thread_id: String,
    pub turn_id: String,
    /// The App Server epoch; requests from an older epoch can no longer be answered.
    pub epoch: u64,
}

/// How long the Agent that registered a work may send its `turn/start`; afterwards it must not.
pub const SEND_WINDOW_MS: i64 = 60_000;
/// After the send window plus this grace, the Node settles a work that never showed up.
pub const RECONCILE_GRACE_MS: i64 = 30_000;
/// Two persisted progress updates of one work are at least this far apart.
pub const PROGRESS_INTERVAL_MS: i64 = 5_000;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum WorkState {
    Accepted,
    /// The thread is journaled before the Agent sends `turn/start`, so the Node can look for
    /// the turn by its `clientUserMessageId` (the work ID).
    Starting {
        thread: Option<String>,
    },
    Running {
        handle: Handle,
    },
    Interrupting {
        handle: Handle,
    },
    Finished {
        outcome: WorkOutcome,
    },
    /// The real result cannot be proven. `probe` says a later App Server probe may settle it;
    /// otherwise only the user does. Without a known turn, `thread` is the journaled thread a
    /// late send may still have started it in.
    Unverified {
        handle: Option<Handle>,
        thread: Option<String>,
        evidence: String,
        probe: bool,
    },
}

impl WorkState {
    pub fn handle(&self) -> Option<&Handle> {
        match self {
            Self::Running { handle } | Self::Interrupting { handle } => Some(handle),
            Self::Unverified { handle, .. } => handle.as_ref(),
            _ => None,
        }
    }
    /// The thread the work runs or may have started in.
    pub fn thread(&self) -> Option<&str> {
        match self {
            Self::Starting { thread } => thread.as_deref(),
            Self::Unverified {
                handle: None,
                thread,
                ..
            } => thread.as_deref(),
            state => state.handle().map(|handle| handle.thread_id.as_str()),
        }
    }
    pub fn finished(&self) -> bool {
        matches!(self, Self::Finished { .. })
    }
    pub fn name(&self) -> &'static str {
        match self {
            Self::Accepted => "accepted",
            Self::Starting { .. } => "starting",
            Self::Running { .. } => "running",
            Self::Interrupting { .. } => "interrupting",
            Self::Finished { .. } => "finished",
            Self::Unverified { .. } => "unverified",
        }
    }
}

/// The target Node's execution state of one work. What the work is lives in the DAG; this keeps
/// only what running and reconciling it needs: its thread and turn, the App Server epoch, the
/// answer text so far and the send window.
#[derive(Debug, Clone, PartialEq)]
pub struct WorkRecord {
    pub work_id: String,
    pub conversation: String,
    pub kind: WorkKind,
    pub state: WorkState,
    /// Latest final-answer text of the turn; reported once in `WorkFinished`.
    pub text: String,
    /// The Agent may send `turn/start` only before this time.
    pub send_by_ms: i64,
    /// The last persisted progress summary and when it was written.
    pub progress: String,
    pub progress_at_ms: i64,
}

/// What a probe of the App Server found for the journaled thread.
#[derive(Debug, Clone, PartialEq)]
pub enum Probe {
    /// The turn exists and is still running under the current App Server epoch.
    Running { turn_id: String, epoch: u64 },
    Terminal {
        turn_id: String,
        status: TurnStatus,
        text: String,
        error: Option<ExecFailure>,
    },
    /// The thread has no turn: nothing started.
    NotStarted,
    /// The thread or turn cannot be found.
    Missing,
}

impl WorkRecord {
    /// A work that never reached `Running` is settled by the Node only after the Agent's window.
    pub fn send_window_over(&self, at: i64) -> bool {
        at >= self.send_by_ms + RECONCILE_GRACE_MS
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum Signal {
    ThreadCreated(String),
    TurnStarted {
        turn_id: String,
        epoch: u64,
    },
    /// Nothing started: the Agent reported a refusal, or the window passed before it bound a thread.
    StartRejected(ExecFailure),
    Text(String),
    /// A plan or commentary update; persisted at most every `PROGRESS_INTERVAL_MS`.
    Progress {
        summary: String,
        at_ms: i64,
    },
    InterruptRequested,
    TurnCompleted {
        turn_id: String,
        status: TurnStatus,
        text: String,
        error: Option<ExecFailure>,
    },
    /// The App Server restarted; live requests from the old epoch are gone.
    Disconnected,
    Probed(Probe),
    InterruptTimedOut,
    /// The DAG already holds a settled result (the user's signed choice); no event is implied.
    Settled(WorkOutcome),
    /// The coordination process of an unverified agent task is gone; its turn did not finish.
    ProcessExited,
    /// An agent task's coordination turn ended; its reply is the result. `failure` names the
    /// execution failure that ended it, if one did.
    Concluded {
        outcome: WorkOutcome,
        text: String,
        reason: Option<String>,
        failure: Option<FailureKind>,
    },
}

pub fn accept(spec: WorkSpec, at: i64) -> (WorkRecord, EventBody) {
    let event = EventBody::WorkDispatched {
        work_id: spec.work_id.clone(),
        trigger: spec.trigger.clone(),
        dispatcher: spec.dispatcher.clone(),
        call_ref: spec.call_ref.clone(),
        kind: spec.kind,
        cwd: spec.cwd.clone(),
        title: spec.title.clone(),
        prompt: spec.prompt.clone(),
        continues: spec.continues.clone(),
        parent: spec.parent.clone(),
        thread: spec.thread.clone(),
    };
    let state = match spec.thread {
        Some(thread) => WorkState::Starting {
            thread: Some(thread),
        },
        None => WorkState::Accepted,
    };
    (
        WorkRecord {
            work_id: spec.work_id,
            conversation: spec.conversation,
            kind: spec.kind,
            state,
            text: String::new(),
            send_by_ms: at + SEND_WINDOW_MS,
            progress: String::new(),
            progress_at_ms: 0,
        },
        event,
    )
}

fn bounded(text: &str) -> String {
    text.chars().take(16_384).collect()
}

fn finish(
    record: &mut WorkRecord,
    outcome: WorkOutcome,
    reason: Option<String>,
    failure: Option<FailureKind>,
) -> Option<EventBody> {
    record.state = WorkState::Finished { outcome };
    Some(EventBody::WorkFinished {
        work_id: record.work_id.clone(),
        outcome,
        result_text: bounded(&record.text),
        failure_reason: reason.map(|reason| bounded(&reason)),
        failure,
        attestation: None,
    })
}

/// Ends the work with an execution failure, its detail as the reason.
fn finish_failed(
    record: &mut WorkRecord,
    outcome: WorkOutcome,
    failure: Option<ExecFailure>,
) -> Option<EventBody> {
    let (reason, kind) = failure.map(|f| (f.detail, f.kind)).unzip();
    finish(record, outcome, reason, kind)
}

const DISCONNECTED: &str = "和执行设备断开了。等它重连后确认结果。";
/// The Agent bound a thread, then no turn showed up by the window's end. Its `turn/start` may
/// still be in flight, so the work is neither started nor refused.
const SEND_UNSEEN: &str = "发送时限过了还没看到这次回合，不知道是否已开始";

fn unverified(
    record: &mut WorkRecord,
    handle: Option<Handle>,
    evidence: &str,
    probe: bool,
) -> Option<EventBody> {
    let thread = match handle {
        Some(_) => None,
        None => record.state.thread().map(str::to_owned),
    };
    record.state = WorkState::Unverified {
        handle,
        thread,
        evidence: evidence.into(),
        probe,
    };
    Some(EventBody::WorkFinished {
        work_id: record.work_id.clone(),
        outcome: WorkOutcome::Unverified,
        result_text: bounded(&record.text),
        failure_reason: Some(bounded(evidence)),
        failure: None,
        attestation: None,
    })
}

fn progress(record: &WorkRecord, phase: WorkPhase, summary: &str) -> Option<EventBody> {
    let handle = record.state.handle();
    Some(EventBody::WorkProgress {
        work_id: record.work_id.clone(),
        phase,
        summary: summary.into(),
        thread_id: handle.map(|handle| handle.thread_id.clone()),
        turn_id: handle.map(|handle| handle.turn_id.clone()),
    })
}

fn turn_outcome(status: &TurnStatus) -> Option<WorkOutcome> {
    match status {
        TurnStatus::Completed => Some(WorkOutcome::Completed),
        TurnStatus::Failed => Some(WorkOutcome::Failed),
        TurnStatus::Interrupted => Some(WorkOutcome::Interrupted),
        TurnStatus::InProgress | TurnStatus::Other(_) => None,
    }
}

/// Applies one signal; returns the DAG event it implies, if any. Invalid signals are ignored.
pub fn transition(record: &mut WorkRecord, signal: Signal) -> Option<EventBody> {
    use WorkState::*;
    let state = record.state.clone();
    match (state, signal) {
        (Accepted | Starting { thread: None }, Signal::ThreadCreated(thread)) => {
            record.state = Starting {
                thread: Some(thread),
            };
            None
        }
        (
            Starting {
                thread: Some(thread_id),
            },
            Signal::TurnStarted { turn_id, epoch },
        ) => {
            record.state = Running {
                handle: Handle {
                    thread_id,
                    turn_id,
                    epoch,
                },
            };
            progress(record, WorkPhase::Started, "执行已开始")
        }
        (
            Accepted
            | Starting { .. }
            | Unverified {
                handle: None,
                thread: Some(_),
                probe: true,
                ..
            },
            Signal::StartRejected(failure),
        ) => finish_failed(record, WorkOutcome::Rejected, Some(failure)),
        (Running { .. } | Interrupting { .. }, Signal::Text(text)) => {
            record.text = bounded(&text);
            None
        }
        (Running { .. }, Signal::Progress { summary, at_ms }) => {
            let summary: String = summary.chars().take(500).collect();
            if summary.trim().is_empty()
                || summary == record.progress
                || at_ms < record.progress_at_ms + PROGRESS_INTERVAL_MS
            {
                return None;
            }
            record.progress = summary;
            record.progress_at_ms = at_ms;
            progress(record, WorkPhase::Running, &record.progress.clone())
        }
        (Running { handle }, Signal::InterruptRequested) => {
            record.state = Interrupting { handle };
            progress(record, WorkPhase::Interrupting, "正在停止")
        }
        (
            Running { handle } | Interrupting { handle },
            Signal::TurnCompleted {
                turn_id,
                status,
                text,
                error,
            },
        ) if turn_id == handle.turn_id => {
            if !text.is_empty() {
                record.text = bounded(&text);
            }
            match turn_outcome(&status) {
                Some(outcome) => finish_failed(record, outcome, error),
                None => unverified(
                    record,
                    Some(handle),
                    &format!("执行器返回未知状态：{}", status.as_str()),
                    true,
                ),
            }
        }
        (Interrupting { handle }, Signal::InterruptTimedOut) => unverified(
            record,
            Some(handle),
            "停止请求未得到执行器确认；无法确认是否仍在运行",
            true,
        ),
        (Running { handle } | Interrupting { handle }, Signal::Disconnected) => {
            record.state = Unverified {
                handle: Some(handle),
                thread: None,
                evidence: DISCONNECTED.into(),
                probe: true,
            };
            None
        }
        // A send whose turn was not seen in time may still start; it is then followed as usual.
        (
            Unverified {
                handle: None,
                thread: Some(thread),
                probe: true,
                ..
            },
            Signal::TurnStarted { turn_id, epoch },
        ) => {
            record.state = Running {
                handle: Handle {
                    thread_id: thread,
                    turn_id,
                    epoch,
                },
            };
            progress(record, WorkPhase::Started, "执行已开始")
        }
        (
            Starting {
                thread: Some(thread),
            }
            | Unverified {
                handle: None,
                thread: Some(thread),
                probe: true,
                ..
            },
            Signal::Probed(probe),
        ) => match probe {
            // Reported once, when the window ends; a later probe that finds nothing adds nothing.
            Probe::NotStarted | Probe::Missing if matches!(record.state, Unverified { .. }) => None,
            Probe::NotStarted | Probe::Missing => unverified(record, None, SEND_UNSEEN, true),
            Probe::Running { turn_id, epoch } => {
                let summary = match record.state {
                    Unverified { .. } => "找到了这次回合，继续跟进",
                    _ => "设备重启后已继续跟进",
                };
                record.state = Running {
                    handle: Handle {
                        thread_id: thread,
                        turn_id,
                        epoch,
                    },
                };
                progress(record, WorkPhase::Running, summary)
            }
            Probe::Terminal {
                turn_id,
                status,
                text,
                error,
            } => {
                record.state = Running {
                    handle: Handle {
                        thread_id: thread,
                        turn_id: turn_id.clone(),
                        epoch: 0,
                    },
                };
                transition(
                    record,
                    Signal::TurnCompleted {
                        turn_id,
                        status,
                        text,
                        error,
                    },
                )
            }
        },
        (
            Running { handle }
            | Interrupting { handle }
            | Unverified {
                handle: Some(handle),
                probe: true,
                ..
            },
            Signal::Probed(probe),
        ) => match probe {
            Probe::Running { turn_id, epoch } if turn_id == handle.turn_id => {
                let was_unverified = matches!(record.state, Unverified { .. });
                record.state = Running {
                    handle: Handle { epoch, ..handle },
                };
                was_unverified
                    .then(|| progress(record, WorkPhase::Running, "设备重启后已继续跟进"))?
            }
            Probe::Terminal {
                turn_id,
                status,
                text,
                error,
            } if turn_id == handle.turn_id => {
                record.state = Running { handle };
                transition(
                    record,
                    Signal::TurnCompleted {
                        turn_id,
                        status,
                        text,
                        error,
                    },
                )
            }
            _ => {
                // Report once; a disconnect alone has not been reported yet.
                if matches!(&record.state, Unverified { evidence, .. } if evidence != DISCONNECTED)
                {
                    return None;
                }
                unverified(
                    record,
                    Some(handle),
                    "执行器中找不到原始回合；无法确认结果",
                    true,
                )
            }
        },
        (Unverified { handle: None, .. }, Signal::ProcessExited) => finish(
            record,
            WorkOutcome::Interrupted,
            Some("Bot 进程已退出，任务没有做完".into()),
            Some(FailureKind::Lost),
        ),
        (Unverified { .. } | Accepted | Starting { .. }, Signal::Settled(outcome)) => {
            record.state = Finished { outcome };
            None
        }
        (
            Accepted,
            Signal::Concluded {
                outcome,
                text,
                reason,
                failure,
            },
        ) => {
            record.text = bounded(&text);
            match outcome {
                WorkOutcome::Unverified => unverified(
                    record,
                    None,
                    reason.as_deref().unwrap_or("Bot 进程没有确认退出"),
                    false,
                ),
                outcome => finish(record, outcome, reason, failure),
            }
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record() -> WorkRecord {
        accept(
            WorkSpec {
                work_id: "w".into(),
                conversation: "c".into(),
                trigger: "message:m".into(),
                target: "t".into(),
                dispatcher: "t".into(),
                call_ref: "r".into(),
                kind: WorkKind::Agent(crate::providers::Provider::Codex),
                cwd: "/tmp".into(),
                title: "do".into(),
                prompt: "do".into(),
                continues: None,
                parent: None,
                thread: None,
            },
            0,
        )
        .0
    }

    fn running() -> WorkRecord {
        let mut work = record();
        transition(&mut work, Signal::ThreadCreated("th".into()));
        let started = transition(
            &mut work,
            Signal::TurnStarted {
                turn_id: "tu".into(),
                epoch: 1,
            },
        );
        assert!(matches!(
            started,
            Some(EventBody::WorkProgress {
                phase: WorkPhase::Started,
                ..
            })
        ));
        work
    }

    #[test]
    fn progress_is_persisted_only_when_new_and_spaced() {
        let mut work = running();
        let update = |summary: &str, at_ms| Signal::Progress {
            summary: summary.into(),
            at_ms,
        };
        assert!(transition(&mut work, update("读代码", 10_000)).is_some());
        assert!(transition(&mut work, update("读代码", 20_000)).is_none());
        assert!(transition(&mut work, update("改代码", 12_000)).is_none());
        assert!(matches!(
            transition(&mut work, update("改代码", 15_000)),
            Some(EventBody::WorkProgress { ref summary, .. }) if summary == "改代码"
        ));
    }

    #[test]
    fn missing_turn_and_unconfirmed_interrupt_become_unverified() {
        let mut work = running();
        transition(&mut work, Signal::Disconnected);
        assert!(matches!(
            transition(&mut work, Signal::Probed(Probe::Missing)),
            Some(EventBody::WorkFinished {
                outcome: WorkOutcome::Unverified,
                ..
            })
        ));
        assert!(transition(&mut work, Signal::Settled(WorkOutcome::Completed)).is_none());
        assert_eq!(
            work.state,
            WorkState::Finished {
                outcome: WorkOutcome::Completed
            }
        );

        let mut work = running();
        transition(&mut work, Signal::InterruptRequested);
        let completed = |status: &str| Signal::TurnCompleted {
            turn_id: "tu".into(),
            status: TurnStatus::of(status),
            text: String::new(),
            error: None,
        };
        let mut timed_out = work.clone();
        assert!(matches!(
            transition(&mut timed_out, Signal::InterruptTimedOut),
            Some(EventBody::WorkFinished {
                outcome: WorkOutcome::Unverified,
                ..
            })
        ));
        let mut failed = work.clone();
        assert!(matches!(
            transition(
                &mut failed,
                Signal::TurnCompleted {
                    turn_id: "tu".into(),
                    status: TurnStatus::Failed,
                    text: String::new(),
                    error: Some(ExecFailure::new(FailureKind::Quota, "429")),
                }
            ),
            Some(EventBody::WorkFinished {
                outcome: WorkOutcome::Failed,
                failure: Some(FailureKind::Quota),
                ..
            })
        ));
        assert!(matches!(
            transition(&mut work, completed("interrupted")),
            Some(EventBody::WorkFinished {
                outcome: WorkOutcome::Interrupted,
                ..
            })
        ));
        assert!(transition(&mut work, completed("completed")).is_none());
    }

    #[test]
    fn restart_probes_settle_journaled_work_without_starting_it_again() {
        let mut work = record();
        assert!(!work.send_window_over(SEND_WINDOW_MS));
        assert!(work.send_window_over(SEND_WINDOW_MS + RECONCILE_GRACE_MS));
        transition(&mut work, Signal::ThreadCreated("th".into()));
        assert!(matches!(
            transition(
                &mut work,
                Signal::Probed(Probe::Terminal {
                    turn_id: "tu".into(),
                    status: TurnStatus::Completed,
                    text: "done".into(),
                    error: None,
                })
            ),
            Some(EventBody::WorkFinished {
                outcome: WorkOutcome::Completed,
                ref result_text,
                ..
            }) if result_text == "done"
        ));
        let mut work = running();
        transition(&mut work, Signal::Disconnected);
        assert!(matches!(
            transition(
                &mut work,
                Signal::Probed(Probe::Running {
                    turn_id: "tu".into(),
                    epoch: 2
                })
            ),
            Some(EventBody::WorkProgress { .. })
        ));
        assert_eq!(work.state.handle().map(|handle| handle.epoch), Some(2));
        let mut work = running();
        transition(&mut work, Signal::Disconnected);
        assert!(
            matches!(
                transition(&mut work, Signal::Probed(Probe::Missing)),
                Some(EventBody::WorkFinished {
                    outcome: WorkOutcome::Unverified,
                    ..
                })
            ),
            "a disconnect then a missing turn is reported once"
        );
        assert!(transition(&mut work, Signal::Probed(Probe::Missing)).is_none());
    }

    #[test]
    fn a_send_unseen_by_the_window_is_unverified_and_a_late_turn_is_followed() {
        let mut work = record();
        transition(&mut work, Signal::ThreadCreated("th".into()));
        assert!(matches!(
            transition(&mut work, Signal::Probed(Probe::NotStarted)),
            Some(EventBody::WorkFinished {
                outcome: WorkOutcome::Unverified,
                ..
            })
        ));
        assert_eq!(work.state.thread(), Some("th"));
        assert!(
            transition(&mut work, Signal::Probed(Probe::NotStarted)).is_none(),
            "an unseen send is reported once"
        );
        let mut late = work.clone();
        assert!(matches!(
            transition(
                &mut late,
                Signal::TurnStarted {
                    turn_id: "tu".into(),
                    epoch: 1
                }
            ),
            Some(EventBody::WorkProgress {
                phase: WorkPhase::Started,
                ..
            })
        ));
        assert!(matches!(
            transition(
                &mut late,
                Signal::TurnCompleted {
                    turn_id: "tu".into(),
                    status: TurnStatus::Completed,
                    text: "done".into(),
                    error: None,
                }
            ),
            Some(EventBody::WorkFinished {
                outcome: WorkOutcome::Completed,
                ..
            })
        ));
        assert!(matches!(
            transition(
                &mut work,
                Signal::Probed(Probe::Running {
                    turn_id: "tu".into(),
                    epoch: 2
                })
            ),
            Some(EventBody::WorkProgress { .. })
        ));
        assert_eq!(work.state.handle().map(|handle| handle.epoch), Some(2));

        let mut refused = work.clone();
        refused.state = WorkState::Unverified {
            handle: None,
            thread: Some("th".into()),
            evidence: String::new(),
            probe: true,
        };
        assert!(matches!(
            transition(
                &mut refused,
                Signal::StartRejected(ExecFailure::new(FailureKind::Rejected, "refused"))
            ),
            Some(EventBody::WorkFinished {
                outcome: WorkOutcome::Rejected,
                failure: Some(FailureKind::Rejected),
                failure_reason: Some(ref reason),
                ..
            }) if reason == "refused"
        ));

        let mut unbound = record();
        assert!(matches!(
            transition(
                &mut unbound,
                Signal::StartRejected(ExecFailure::new(FailureKind::TimedOut, "window over"))
            ),
            Some(EventBody::WorkFinished {
                outcome: WorkOutcome::Rejected,
                ..
            })
        ));
    }
}
