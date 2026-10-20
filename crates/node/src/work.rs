//! The target Node's work state machine. It is the only authority for a work unit:
//! `transition` is the only source of Work events in the DAG, and the DAG projection,
//! the pending-check list and the Web state all derive from it.
use crate::{crypto::digest, model::*};
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct WorkSpec {
    pub work_id: String,
    pub conversation: String,
    /// The scheduler trigger whose turn dispatched this work.
    pub trigger: String,
    /// The coordination attempt that dispatched it; only that attempt may retry the call.
    pub attempt: String,
    pub target: DeviceId,
    pub dispatcher: DeviceId,
    pub call_ref: String,
    pub executor: Executor,
    pub cwd: String,
    pub prompt: String,
    pub continues: Option<String>,
    pub parent: Option<String>,
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
    Rejected { reason: String },
}

/// A stop request for another Node's work that has not reached it yet. It is retried until the
/// target confirms, the work ends, or `deadline_ms` passes.
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
    /// The executor host epoch; requests from an older epoch can no longer be answered.
    pub epoch: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PendingApproval {
    pub request_id: String,
    pub rpc_id: Value,
    pub method: String,
    pub params: Value,
    pub kind: String,
    pub summary: String,
    /// Set before the answer is sent; a resolution while set is this Node's own answer.
    pub answering: bool,
}

pub fn request_id(work_id: &str, epoch: u64, rpc_id: &Value) -> String {
    digest(&serde_json::to_vec(&(work_id, epoch, rpc_id)).expect("json"))
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum WorkState {
    Accepted,
    /// The thread ID is journaled before `turn/start`, so a crash can be reconciled.
    Starting {
        thread: Option<String>,
    },
    Running {
        handle: Handle,
    },
    WaitingApproval {
        handle: Handle,
        approvals: Vec<PendingApproval>,
    },
    Interrupting {
        handle: Handle,
    },
    Finished {
        outcome: WorkOutcome,
    },
    /// The real result cannot be proven. `probe` says a later executor probe may settle it;
    /// otherwise only the user does.
    Unverified {
        handle: Option<Handle>,
        evidence: String,
        probe: bool,
    },
}

impl WorkState {
    pub fn handle(&self) -> Option<&Handle> {
        match self {
            Self::Running { handle }
            | Self::WaitingApproval { handle, .. }
            | Self::Interrupting { handle } => Some(handle),
            Self::Unverified { handle, .. } => handle.as_ref(),
            _ => None,
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
            Self::WaitingApproval { .. } => "waiting_approval",
            Self::Interrupting { .. } => "interrupting",
            Self::Finished { .. } => "finished",
            Self::Unverified { .. } => "unverified",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct WorkRecord {
    pub spec: WorkSpec,
    pub state: WorkState,
    /// Latest final-answer text of the turn; reported once in `WorkFinished`.
    pub text: String,
}

/// What a probe of the executor found for the journaled thread.
#[derive(Debug, Clone, PartialEq)]
pub enum Probe {
    /// The turn exists and is still running under the current executor epoch.
    Running { turn_id: String, epoch: u64 },
    Terminal {
        turn_id: String,
        status: String,
        text: String,
        error: Option<String>,
    },
    /// The thread has no turn: nothing started.
    NotStarted,
    /// The thread or turn cannot be found.
    Missing,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Signal {
    ThreadCreated(String),
    TurnStarted {
        turn_id: String,
        epoch: u64,
    },
    /// The executor proved nothing started.
    StartRejected(String),
    /// The start may or may not have happened.
    StartUnknown(String),
    Text(String),
    ApprovalRequested {
        rpc_id: Value,
        method: String,
        params: Value,
        kind: String,
        summary: String,
        epoch: u64,
    },
    ApprovalAnswering(String),
    ApprovalAnswerFailed(String),
    ApprovalAnswered {
        request_id: String,
        decision: ApprovalDecision,
        attestation: Attestation,
    },
    /// The executor reports a request resolved; unless this Node was answering it, someone bypassed it.
    ApprovalResolved(Value),
    InterruptRequested,
    TurnCompleted {
        turn_id: String,
        status: String,
        text: String,
        error: Option<String>,
    },
    /// The executor host restarted; live requests from the old epoch are gone.
    Disconnected,
    Probed(Probe),
    InterruptTimedOut,
    /// The DAG already holds a settled result (the user's signed choice); no event is implied.
    Settled(WorkOutcome),
    /// An agent task's coordination turn ended; its reply is the result.
    Concluded {
        outcome: WorkOutcome,
        text: String,
        reason: Option<String>,
    },
}

pub fn accept(spec: WorkSpec) -> (WorkRecord, EventBody) {
    let event = EventBody::WorkDispatched {
        work_id: spec.work_id.clone(),
        trigger: spec.trigger.clone(),
        dispatcher: spec.dispatcher.clone(),
        call_ref: spec.call_ref.clone(),
        executor: spec.executor,
        cwd: spec.cwd.clone(),
        prompt: spec.prompt.clone(),
        continues: spec.continues.clone(),
        parent: spec.parent.clone(),
    };
    (
        WorkRecord {
            spec,
            state: WorkState::Accepted,
            text: String::new(),
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
) -> Option<EventBody> {
    record.state = WorkState::Finished { outcome };
    Some(EventBody::WorkFinished {
        work_id: record.spec.work_id.clone(),
        outcome,
        result_text: bounded(&record.text),
        failure_reason: reason.map(|reason| bounded(&reason)),
        attestation: None,
    })
}

const DISCONNECTED: &str = "执行器连接已断开，等待重新核对";

fn unverified(
    record: &mut WorkRecord,
    handle: Option<Handle>,
    evidence: &str,
    probe: bool,
) -> Option<EventBody> {
    record.state = WorkState::Unverified {
        handle,
        evidence: evidence.into(),
        probe,
    };
    Some(EventBody::WorkFinished {
        work_id: record.spec.work_id.clone(),
        outcome: WorkOutcome::Unverified,
        result_text: bounded(&record.text),
        failure_reason: Some(bounded(evidence)),
        attestation: None,
    })
}

fn progress(record: &WorkRecord, phase: WorkPhase, summary: &str) -> Option<EventBody> {
    let handle = record.state.handle();
    Some(EventBody::WorkProgress {
        work_id: record.spec.work_id.clone(),
        phase,
        summary: summary.into(),
        thread_id: handle.map(|handle| handle.thread_id.clone()),
        turn_id: handle.map(|handle| handle.turn_id.clone()),
    })
}

fn turn_outcome(status: &str) -> Option<WorkOutcome> {
    match status {
        "completed" => Some(WorkOutcome::Completed),
        "failed" => Some(WorkOutcome::Failed),
        "interrupted" => Some(WorkOutcome::Interrupted),
        _ => None,
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
        (Accepted | Starting { .. }, Signal::StartRejected(reason)) => {
            finish(record, WorkOutcome::Rejected, Some(reason))
        }
        (Accepted | Starting { .. }, Signal::StartUnknown(reason)) => {
            unverified(record, None, &reason, true)
        }
        (Running { .. } | WaitingApproval { .. } | Interrupting { .. }, Signal::Text(text)) => {
            record.text = bounded(&text);
            None
        }
        (
            Running { handle } | WaitingApproval { handle, .. },
            Signal::ApprovalRequested {
                rpc_id,
                method,
                params,
                kind,
                summary,
                epoch,
            },
        ) if epoch == handle.epoch => {
            let request_id = request_id(&record.spec.work_id, epoch, &rpc_id);
            let mut approvals = match &record.state {
                WaitingApproval { approvals, .. } => approvals.clone(),
                _ => Vec::new(),
            };
            if approvals.iter().any(|known| known.request_id == request_id) {
                return None;
            }
            approvals.push(PendingApproval {
                request_id: request_id.clone(),
                rpc_id,
                method,
                params,
                kind: kind.clone(),
                summary: summary.clone(),
                answering: false,
            });
            record.state = WaitingApproval { handle, approvals };
            Some(EventBody::ApprovalRequested {
                work_id: record.spec.work_id.clone(),
                request_id,
                kind,
                summary: bounded(&summary),
            })
        }
        (
            WaitingApproval {
                handle,
                mut approvals,
            },
            Signal::ApprovalAnswering(request),
        ) => {
            let approval = approvals.iter_mut().find(|a| a.request_id == request)?;
            approval.answering = true;
            record.state = WaitingApproval { handle, approvals };
            None
        }
        (
            WaitingApproval {
                handle,
                mut approvals,
            },
            Signal::ApprovalAnswerFailed(request),
        ) => {
            let approval = approvals.iter_mut().find(|a| a.request_id == request)?;
            approval.answering = false;
            record.state = WaitingApproval { handle, approvals };
            None
        }
        (
            WaitingApproval {
                handle,
                mut approvals,
            },
            Signal::ApprovalAnswered {
                request_id,
                decision,
                attestation,
            },
        ) => {
            let before = approvals.len();
            approvals.retain(|approval| approval.request_id != request_id);
            if approvals.len() == before {
                return None;
            }
            record.state = if approvals.is_empty() {
                Running { handle }
            } else {
                WaitingApproval { handle, approvals }
            };
            Some(EventBody::ApprovalAnswered {
                work_id: record.spec.work_id.clone(),
                request_id,
                decision,
                attestation: Some(attestation),
            })
        }
        (WaitingApproval { handle, approvals }, Signal::ApprovalResolved(rpc_id)) => {
            let approval = approvals.iter().find(|a| a.rpc_id == rpc_id)?;
            if approval.answering {
                return None;
            }
            unverified(
                record,
                Some(handle),
                "审批请求在本设备之外被处理；无法确认执行结果",
                false,
            )
        }
        (Running { handle } | WaitingApproval { handle, .. }, Signal::InterruptRequested) => {
            record.state = Interrupting { handle };
            progress(record, WorkPhase::Interrupting, "正在停止")
        }
        (
            Running { handle } | WaitingApproval { handle, .. } | Interrupting { handle },
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
                Some(outcome) => finish(record, outcome, error),
                None => unverified(
                    record,
                    Some(handle),
                    &format!("执行器返回未知状态：{status}"),
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
        (
            Running { handle } | WaitingApproval { handle, .. } | Interrupting { handle },
            Signal::Disconnected,
        ) => {
            record.state = Unverified {
                handle: Some(handle),
                evidence: DISCONNECTED.into(),
                probe: true,
            };
            None
        }
        (Accepted | Starting { thread: None }, Signal::Disconnected) => finish(
            record,
            WorkOutcome::Rejected,
            Some("执行器在启动前退出".into()),
        ),
        (
            Starting {
                thread: Some(thread),
            },
            Signal::Probed(probe),
        ) => match probe {
            Probe::NotStarted | Probe::Missing => {
                finish(record, WorkOutcome::Rejected, Some("执行未开始".into()))
            }
            Probe::Running { turn_id, epoch } => {
                record.state = Running {
                    handle: Handle {
                        thread_id: thread,
                        turn_id,
                        epoch,
                    },
                };
                progress(record, WorkPhase::Running, "执行已重新接管")
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
            | WaitingApproval { handle, .. }
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
                was_unverified.then(|| progress(record, WorkPhase::Running, "执行已重新接管"))?
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
        (Unverified { .. }, Signal::Settled(outcome)) => {
            record.state = Finished { outcome };
            None
        }
        (
            Accepted,
            Signal::Concluded {
                outcome,
                text,
                reason,
            },
        ) => {
            record.text = bounded(&text);
            match outcome {
                WorkOutcome::Unverified => unverified(
                    record,
                    None,
                    reason.as_deref().unwrap_or("助手进程没有确认退出"),
                    false,
                ),
                outcome => finish(record, outcome, reason),
            }
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn record() -> WorkRecord {
        accept(WorkSpec {
            work_id: "w".into(),
            conversation: "c".into(),
            trigger: "message:m".into(),
            attempt: "a".into(),
            target: "t".into(),
            dispatcher: "t".into(),
            call_ref: "r".into(),
            executor: Executor::Codex,
            cwd: "/tmp".into(),
            prompt: "do".into(),
            continues: None,
            parent: None,
        })
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

    fn approval(rpc: i64, epoch: u64) -> Signal {
        Signal::ApprovalRequested {
            rpc_id: json!(rpc),
            method: "item/commandExecution/requestApproval".into(),
            params: json!({}),
            kind: "command".into(),
            summary: "ls".into(),
            epoch,
        }
    }

    #[test]
    fn approvals_answer_once_and_stale_epochs_are_ignored() {
        let mut work = running();
        assert!(transition(&mut work, approval(7, 0)).is_none());
        let Some(EventBody::ApprovalRequested { request_id, .. }) =
            transition(&mut work, approval(7, 1))
        else {
            panic!("approval must be recorded");
        };
        assert_eq!(request_id, super::request_id("w", 1, &json!(7)));
        assert!(transition(&mut work, approval(7, 1)).is_none());
        transition(&mut work, Signal::ApprovalAnswering(request_id.clone()));
        assert!(transition(&mut work, Signal::ApprovalResolved(json!(7))).is_none());
        let answered = Signal::ApprovalAnswered {
            request_id: request_id.clone(),
            decision: ApprovalDecision::Decline,
            attestation: Attestation {
                device_id: "c".repeat(64),
                signature: "s".into(),
            },
        };
        assert!(transition(&mut work, answered.clone()).is_some());
        assert!(transition(&mut work, answered).is_none());
        assert_eq!(work.state.name(), "running");
    }

    #[test]
    fn bypassed_approval_and_unconfirmed_interrupt_become_unverified() {
        let mut work = running();
        transition(&mut work, approval(1, 1));
        let Some(EventBody::WorkFinished { outcome, .. }) =
            transition(&mut work, Signal::ApprovalResolved(json!(1)))
        else {
            panic!("bypass must be recorded");
        };
        assert_eq!(outcome, WorkOutcome::Unverified);
        assert!(transition(&mut work, Signal::Probed(Probe::Missing)).is_none());
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
            status: status.into(),
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
        transition(&mut work, Signal::ThreadCreated("th".into()));
        assert!(matches!(
            transition(&mut work.clone(), Signal::Probed(Probe::NotStarted)),
            Some(EventBody::WorkFinished {
                outcome: WorkOutcome::Rejected,
                ..
            })
        ));
        assert!(matches!(
            transition(
                &mut work,
                Signal::Probed(Probe::Terminal {
                    turn_id: "tu".into(),
                    status: "completed".into(),
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
}
