use super::{Store, event_about_from};
use crate::{
    crypto::digest,
    model::{Role, Subject},
    work::{StopRequest, WorkRequest, WorkSpec},
    workspace::InputTarget,
};
use anyhow::{Context as _, Result, ensure};
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

const INBOUND: &str = "peer-inbound:";
const OUTBOUND: &str = "peer-outbound:";
const MAX_PENDING: usize = 1024;

/// A definite business refusal; persistence and other unknown failures stay retryable.
#[derive(Debug)]
pub(crate) struct AdmissionRejected(String);

impl AdmissionRejected {
    pub(crate) fn new(detail: impl Into<String>) -> Self {
        Self(detail.into())
    }
}

impl std::fmt::Display for AdmissionRejected {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for AdmissionRejected {}

pub(crate) fn authorize_peer_node_from(
    connection: &Connection,
    network: &str,
    device: &str,
) -> Result<()> {
    super::membership_from(connection, network)?
        .node(device)
        .map(|_| ())
        .map_err(|error| AdmissionRejected::new(error.to_string()).into())
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum PeerCommand {
    Deliver {
        conversation_id: String,
        trigger: String,
    },
    Dispatch {
        spec: Box<WorkRequest>,
    },
    Stop {
        conversation_id: String,
        work_id: String,
    },
}

impl PeerCommand {
    pub(crate) fn conversation(&self) -> &str {
        match self {
            Self::Deliver {
                conversation_id, ..
            }
            | Self::Stop {
                conversation_id, ..
            } => conversation_id,
            Self::Dispatch { spec } => &spec.conversation,
        }
    }

    pub(crate) fn id(&self, network: &str) -> String {
        let (kind, subject) = match self {
            Self::Deliver { trigger, .. } => ("deliver", trigger.as_str()),
            Self::Dispatch { spec } => ("dispatch", spec.work_id.as_str()),
            Self::Stop { work_id, .. } => ("stop", work_id.as_str()),
        };
        format!(
            "peer-{}",
            digest(
                &serde_json::to_vec(&(network, self.conversation(), kind, subject))
                    .expect("strings")
            )
        )
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PeerEnvelope {
    pub command_id: String,
    pub target: String,
    pub owner_epoch: u64,
    pub revision: Option<u64>,
    pub sequence: u64,
    pub causal_refs: Vec<String>,
    pub payload: PeerCommand,
}

impl PeerEnvelope {
    pub(crate) fn lane(&self, network: &str) -> String {
        serde_json::to_string(&(network, self.payload.conversation(), &self.target))
            .expect("strings")
    }

    pub(crate) fn validate(&self, network: &str, origin: &str, local: &str) -> Result<()> {
        ensure!(
            self.command_id == self.payload.id(network),
            "command ID does not match its intent"
        );
        vibeface_protocol::validate_id(self.payload.conversation()).map_err(anyhow::Error::msg)?;
        ensure!(
            !self.target.is_empty() && self.target.len() <= 512,
            "invalid command target"
        );
        let max_refs = match &self.payload {
            PeerCommand::Dispatch { spec } if spec.trigger.starts_with("assignment:") => 1024,
            _ => 32,
        };
        ensure!(
            self.causal_refs.len() <= max_refs
                && self
                    .causal_refs
                    .iter()
                    .all(|hash| hash.len() == 64 && hash.bytes().all(|c| c.is_ascii_hexdigit())),
            "invalid command causal references"
        );
        match &self.payload {
            PeerCommand::Deliver { trigger, .. } => {
                let (kind, subject) = trigger
                    .split_once(':')
                    .context("invalid delivery trigger")?;
                ensure!(
                    matches!(kind, "message" | "bot_message" | "work" | "assignment"),
                    "invalid delivery trigger"
                );
                vibeface_protocol::validate_id(subject).map_err(anyhow::Error::msg)?;
            }
            PeerCommand::Dispatch { spec } => {
                if spec.trigger.starts_with("assignment:") {
                    ensure!(
                        self.revision.is_some_and(|revision| revision > 0)
                            && self.sequence == 0
                            && !self.causal_refs.is_empty()
                            && self.causal_refs.windows(2).all(|pair| pair[0] < pair[1]),
                        "invalid assignment Work source frame"
                    );
                }
                ensure!(
                    spec.dispatcher == origin && spec.target == local,
                    "a dispatch names its sender and this Node"
                );
                ensure!(
                    spec.kind == crate::work::RequestedWorkKind::Task,
                    "another Node takes only Bot tasks"
                );
                ensure!(
                    spec.work_id == WorkSpec::id(&spec.conversation, &spec.trigger, &spec.call_ref),
                    "work ID does not match its call"
                );
                ensure!(
                    self.target == format!("work:{}", spec.work_id),
                    "dispatch target does not match its work"
                );
            }
            PeerCommand::Stop { work_id, .. } => {
                vibeface_protocol::validate_id(work_id).map_err(anyhow::Error::msg)?;
                ensure!(
                    self.target == format!("work:{work_id}"),
                    "stop target does not match its work"
                );
            }
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum PeerAdmissionState {
    Saved,
    WaitingDependencies,
    OwnerAccepted,
    Rejected,
}

impl PeerAdmissionState {
    fn pending(self) -> bool {
        matches!(self, Self::Saved | Self::WaitingDependencies)
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct PeerAdmission {
    pub network_id: String,
    pub origin: String,
    pub envelope: PeerEnvelope,
    pub state: PeerAdmissionState,
    pub response: Value,
    pub queued_at_ms: i64,
}

impl PeerAdmission {
    pub(crate) fn reply(&self) -> Value {
        let mut reply = self.response.clone();
        if let PeerCommand::Stop { work_id, .. } = &self.envelope.payload {
            reply["work_id"] = json!(work_id);
            reply["interrupt_requested"] = json!(true);
            reply["confirmed"] = json!(false);
            reply["waiting_for_device"] = json!(self.state != PeerAdmissionState::OwnerAccepted);
        }
        reply["admission"] = json!({
            "command_id": self.envelope.command_id,
            "target": self.envelope.target,
            "revision": self.envelope.revision,
            "state": self.state,
        });
        reply
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct PeerOutbound {
    pub target_node: String,
    pub envelope: PeerEnvelope,
    pub ingress_saved: bool,
}

fn key(prefix: &str, network: &str, command_id: &str) -> String {
    format!(
        "{prefix}{}",
        digest(&serde_json::to_vec(&(network, command_id)).expect("strings"))
    )
}

pub(super) fn input_delivery_evidence_from(
    connection: &Connection,
    network: &str,
    conversation: &str,
    message_id: &str,
) -> Result<bool> {
    let command_id = PeerCommand::Deliver {
        conversation_id: conversation.into(),
        trigger: crate::scheduling::message_trigger(message_id),
    }
    .id(network);
    Ok(connection.query_row(
        "SELECT EXISTS(SELECT 1 FROM identity_meta WHERE key IN (?1,?2))",
        params![
            key(INBOUND, network, &command_id),
            key(OUTBOUND, network, &command_id)
        ],
        |row| row.get(0),
    )?)
}

fn load<T: serde::de::DeserializeOwned>(connection: &Connection, key: &str) -> Result<Option<T>> {
    let value: Option<String> = connection
        .query_row(
            "SELECT value FROM identity_meta WHERE key=?",
            [key],
            |row| row.get(0),
        )
        .optional()?;
    value
        .map(|value| Ok(serde_json::from_str(&value)?))
        .transpose()
}

fn save(connection: &Connection, key: &str, value: &impl Serialize) -> Result<()> {
    connection.execute(
        "INSERT INTO identity_meta(key,value) VALUES(?,?) ON CONFLICT(key) DO UPDATE SET value=excluded.value",
        params![key, serde_json::to_string(value)?],
    )?;
    Ok(())
}

fn pending_prefix(network: &str) -> String {
    format!("peer-pending:{}:", digest(network.as_bytes()))
}

fn pending_key(network: &str, command_id: &str) -> String {
    format!("{}{command_id}", pending_prefix(network))
}

fn inbound(connection: &Connection, network: &str) -> Result<Vec<PeerAdmission>> {
    let prefix = pending_prefix(network);
    let mut statement = connection.prepare(
        "SELECT command.value FROM identity_meta pending JOIN identity_meta command ON command.key=pending.value
         WHERE pending.key>=?1 AND pending.key<?2 ORDER BY command.rowid",
    )?;
    let rows = statement.query_map(params![prefix, format!("{prefix}~")], |row| {
        row.get::<_, String>(0)
    })?;
    rows.map(|row| Ok(serde_json::from_str(&row?)?)).collect()
}

pub(crate) fn peer_cancelled_from(
    connection: &Connection,
    network: &str,
    conversation: &str,
    work_id: &str,
) -> Result<bool> {
    let payload = PeerCommand::Stop {
        conversation_id: conversation.into(),
        work_id: work_id.into(),
    };
    let Some(entry) =
        load::<PeerAdmission>(connection, &key(INBOUND, network, &payload.id(network)))?
    else {
        return Ok(false);
    };
    let epoch = super::conversation_from(connection, network, conversation)?
        .map(|conversation| conversation.owner_epoch);
    Ok(entry.state == PeerAdmissionState::OwnerAccepted
        || (entry.state != PeerAdmissionState::Rejected
            && epoch == Some(entry.envelope.owner_epoch)
            && super::authorize_node_from(connection, network, &entry.origin).is_ok()))
}

pub(crate) fn held_answer_subject_from(
    connection: &Connection,
    network: &str,
    conversation: &str,
    message_id: &str,
) -> Result<Option<Subject>> {
    let Some(answer) = super::message_from(connection, network, conversation, message_id)? else {
        return Ok(None);
    };
    let Some(question_id) = answer.answers else {
        return Ok(None);
    };
    ensure!(
        answer.role == Role::User && message_id == super::questions::answer_id(&question_id),
        "answer identity does not match its question"
    );
    let question = super::questions::question_from(connection, network, &question_id)?
        .context("answer question has not arrived")?;
    let subject: Subject = serde_json::from_value(question["about"].clone())?;
    if subject.request_id.is_none() {
        return Ok(None);
    }
    ensure!(
        question["conversation_id"] == conversation && question["author"] == subject.node,
        "held question belongs to another conversation or request Node"
    );
    Ok(Some(subject))
}

fn held_delivery_subject_from(
    connection: &Connection,
    network: &str,
    payload: &PeerCommand,
) -> Result<Option<Subject>> {
    let PeerCommand::Deliver {
        conversation_id,
        trigger,
    } = payload
    else {
        return Ok(None);
    };
    let Some(("message", message_id)) = trigger.split_once(':') else {
        return Ok(None);
    };
    held_answer_subject_from(connection, network, conversation_id, message_id)
}

fn held_answer_target(subject: &Subject) -> String {
    format!(
        "held_request:{}",
        digest(&serde_json::to_vec(&(&subject.node, &subject.request_id)).expect("strings"))
    )
}

pub(crate) fn input_target_key(target: &InputTarget) -> String {
    match target {
        InputTarget::BotMain { .. } => "main".into(),
        InputTarget::Task { task_id } | InputTarget::ChannelTask { task_id, .. } => {
            format!("workspace_task:{task_id}")
        }
        InputTarget::Work { work_id } => format!("work:{work_id}"),
        InputTarget::Routine { trigger, .. } => format!("routine:{trigger}"),
        InputTarget::ChannelMember { channel_id, bot_id } => {
            format!("channel_member:{channel_id}:{bot_id}")
        }
    }
}

impl Store {
    pub(crate) fn save_peer_command(
        &mut self,
        network: &str,
        origin: &str,
        envelope: &PeerEnvelope,
        at: i64,
    ) -> Result<PeerAdmission> {
        self.authorize_node(network, origin)?;
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let key = key(INBOUND, network, &envelope.command_id);
        if let Some(existing) = load::<PeerAdmission>(&tx, &key)? {
            ensure!(
                existing.envelope == *envelope,
                "command ID reused with a different intent"
            );
            return Ok(existing);
        }
        ensure!(
            inbound(&tx, network)?
                .iter()
                .filter(|entry| entry.state.pending())
                .count()
                < MAX_PENDING,
            "peer ingress is full; command was not saved"
        );
        let admission = PeerAdmission {
            network_id: network.into(),
            origin: origin.into(),
            envelope: envelope.clone(),
            state: PeerAdmissionState::Saved,
            response: json!({}),
            queued_at_ms: at,
        };
        save(&tx, &key, &admission)?;
        tx.execute(
            "INSERT INTO identity_meta(key,value) VALUES(?,?)",
            params![pending_key(network, &envelope.command_id), key],
        )?;
        tx.commit()?;
        Ok(admission)
    }

    pub(crate) fn pending_peer_commands(&self, network: &str) -> Result<Vec<PeerAdmission>> {
        let mut admissions: Vec<_> = inbound(&self.connection, network)?
            .into_iter()
            .filter(|entry| entry.state.pending())
            .collect();
        admissions.sort_by_key(|entry| (entry.envelope.sequence, entry.queued_at_ms));
        Ok(admissions)
    }

    pub(crate) fn finish_peer_command(
        &mut self,
        network: &str,
        command_id: &str,
        state: PeerAdmissionState,
        response: Value,
    ) -> Result<PeerAdmission> {
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let key = key(INBOUND, network, command_id);
        let mut admission: PeerAdmission = load(&tx, &key)?.context("peer command not found")?;
        if admission.state.pending() {
            admission.state = state;
            admission.response = response;
            save(&tx, &key, &admission)?;
            if !state.pending() {
                tx.execute(
                    "DELETE FROM identity_meta WHERE key=?",
                    [pending_key(network, command_id)],
                )?;
            }
        }
        tx.commit()?;
        Ok(admission)
    }

    pub(crate) fn peer_cancelled(
        &self,
        network: &str,
        conversation: &str,
        work_id: &str,
    ) -> Result<bool> {
        peer_cancelled_from(&self.connection, network, conversation, work_id)
    }

    pub(crate) fn peer_dispatch_envelope(
        &self,
        network: &str,
        request: &WorkRequest,
    ) -> Result<Option<PeerEnvelope>> {
        let payload = PeerCommand::Dispatch {
            spec: Box::new(request.clone()),
        };
        let entry: Option<PeerAdmission> = load(
            &self.connection,
            &key(INBOUND, network, &payload.id(network)),
        )?;
        if let Some(entry) = &entry {
            ensure!(
                entry.envelope.payload == payload,
                "work ID reused with a different dispatch"
            );
        }
        Ok(entry.map(|entry| entry.envelope))
    }

    pub(crate) fn peer_delivery_target(
        &self,
        network: &str,
        conversation: &str,
        trigger: &str,
    ) -> Result<String> {
        let mut trigger = trigger.to_owned();
        for _ in 0..64 {
            match trigger.split_once(':') {
                Some(("message", message)) => {
                    if let Some(subject) =
                        held_answer_subject_from(&self.connection, network, conversation, message)?
                    {
                        return Ok(held_answer_target(&subject));
                    }
                    return Ok(self
                        .input_route(network, conversation, message)?
                        .map_or_else(|| "main".into(), |route| input_target_key(&route.target)));
                }
                Some(("work", work_id)) => {
                    if let Some(work) =
                        super::reduce::work_from(&self.connection, network, work_id)?
                    {
                        ensure!(
                            work.conversation_id == conversation,
                            "work belongs to another conversation"
                        );
                        trigger = work.trigger;
                    } else if let Some(dispatch) = self.dispatch(work_id)? {
                        ensure!(
                            dispatch.spec.conversation == conversation,
                            "dispatch belongs to another conversation"
                        );
                        trigger = dispatch.spec.trigger;
                    } else {
                        anyhow::bail!("delivery work source not found");
                    }
                }
                Some(("task", work_id)) => return Ok(format!("work:{work_id}")),
                Some(("assignment", assignment_id)) => {
                    let assignment =
                        self.delivery_assignment(network, conversation, assignment_id)?;
                    return Ok(format!("workspace_task:{}", assignment.task_id));
                }
                Some(("schedule", _)) => return Ok(format!("routine:{trigger}")),
                _ => return Ok("main".into()),
            }
        }
        anyhow::bail!("delivery causal chain is too deep")
    }

    pub(crate) fn peer_source(
        &self,
        network: &str,
        conversation: &str,
        trigger: &str,
    ) -> Result<(Option<u64>, u64, Vec<String>)> {
        match trigger.split_once(':') {
            Some(("assignment", assignment_id)) => {
                let assignment = self.delivery_assignment(network, conversation, assignment_id)?;
                let mut refs = vec![assignment.event_hash];
                if let Some(source) = assignment.source_event
                    && refs[0] != source
                {
                    refs.push(source);
                }
                Ok((Some(assignment.started_revision), 0, refs))
            }
            Some(("message" | "bot_message", message_id)) => {
                let message = self
                    .message(network, conversation, message_id)?
                    .context("message not found")?;
                let route = self.input_route(network, conversation, message_id)?;
                if held_answer_subject_from(&self.connection, network, conversation, message_id)?
                    .is_some()
                {
                    let question_id = message.answers.as_deref().expect("held answer question");
                    let question = event_about_from(
                        &self.connection,
                        network,
                        conversation,
                        "question_asked",
                        question_id,
                    )?
                    .context("held question source missing")?;
                    return Ok((
                        None,
                        message.created_at_ms.max(0) as u64,
                        vec![message.event_hash, question.hash],
                    ));
                }
                Ok((
                    route.as_ref().and_then(|route| route.expected_revision),
                    route.map_or(message.created_at_ms.max(0) as u64, |route| route.sequence),
                    vec![message.event_hash],
                ))
            }
            Some(("work", work_id)) => {
                let event = event_about_from(
                    &self.connection,
                    network,
                    conversation,
                    "work_finished",
                    work_id,
                )?
                .context("work result not found")?;
                Ok((
                    None,
                    event.event.created_at_ms.max(0) as u64,
                    vec![event.hash],
                ))
            }
            Some(("schedule", _)) => Ok((None, 0, vec![])),
            _ => anyhow::bail!("invalid command source"),
        }
    }

    fn bound_peer_command(
        &self,
        network: &str,
        payload: &PeerCommand,
    ) -> Result<Option<PeerOutbound>> {
        load(
            &self.connection,
            &key(OUTBOUND, network, &payload.id(network)),
        )
    }

    fn delivery_assignment(
        &self,
        network: &str,
        conversation: &str,
        assignment_id: &str,
    ) -> Result<crate::workspace::Assignment> {
        let assignment = self
            .workspace_assignment(network, assignment_id)?
            .context("assignment not found")?;
        let bot = super::bot_settings::find_bot(&self.connection, network, &assignment.bot_id)?;
        ensure!(
            bot.conversation_id == conversation,
            "assignment belongs to another Bot conversation"
        );
        Ok(assignment)
    }

    pub(crate) fn peer_delivery_receipt_matches(
        &self,
        network: &str,
        conversation: &str,
        trigger: &str,
        received: &str,
    ) -> Result<bool> {
        if trigger == received {
            return Ok(true);
        }
        let Some(("assignment", assignment_id)) = trigger.split_once(':') else {
            return Ok(false);
        };
        let assignment = self.delivery_assignment(network, conversation, assignment_id)?;
        let Some(source) = assignment.trigger_message_id else {
            return Ok(false);
        };
        if received != crate::scheduling::message_trigger(&source) {
            return Ok(false);
        }
        let Some(route) = self.input_route(network, conversation, &source)? else {
            return Ok(false);
        };
        Ok(route.expected_revision == Some(assignment.started_revision)
            && matches!(&route.target,
                InputTarget::Task { task_id } | InputTarget::ChannelTask { task_id, .. } if *task_id == assignment.task_id))
    }

    fn bind_peer_command(
        &mut self,
        network: &str,
        target_node: &str,
        target: String,
        payload: PeerCommand,
        source: (Option<u64>, u64, Vec<String>),
    ) -> Result<PeerOutbound> {
        let held = held_delivery_subject_from(&self.connection, network, &payload)?;
        let owner_epoch = if held.is_some() {
            0
        } else {
            match &payload {
                PeerCommand::Deliver {
                    conversation_id,
                    trigger,
                } if trigger.starts_with("assignment:") => {
                    let assignment_id =
                        trigger.strip_prefix("assignment:").expect("matched prefix");
                    self.delivery_assignment(network, conversation_id, assignment_id)?
                        .bot_owner_epoch
                }
                PeerCommand::Dispatch { spec } if spec.trigger.starts_with("assignment:") => {
                    self.delivery_assignment(
                        network,
                        &spec.conversation,
                        spec.trigger
                            .strip_prefix("assignment:")
                            .expect("matched prefix"),
                    )?
                    .bot_owner_epoch
                }
                _ => {
                    self.conversation(network, payload.conversation())?
                        .context("conversation not found")?
                        .owner_epoch
                }
            }
        };
        let command_id = payload.id(network);
        let outbound = PeerOutbound {
            target_node: target_node.into(),
            envelope: PeerEnvelope {
                command_id: command_id.clone(),
                target,
                owner_epoch,
                revision: source.0,
                sequence: source.1,
                causal_refs: source.2,
                payload,
            },
            ingress_saved: false,
        };
        save(
            &self.connection,
            &key(OUTBOUND, network, &command_id),
            &outbound,
        )?;
        Ok(outbound)
    }

    pub(crate) fn peer_delivery_command(
        &mut self,
        network: &str,
        conversation: &str,
        trigger: &str,
    ) -> Result<PeerOutbound> {
        let payload = PeerCommand::Deliver {
            conversation_id: conversation.into(),
            trigger: trigger.into(),
        };
        if let Some(bound) = self.bound_peer_command(network, &payload)? {
            return Ok(bound);
        }
        let owner = match trigger.split_once(':') {
            Some(("assignment", assignment_id)) => {
                self.delivery_assignment(network, conversation, assignment_id)?
                    .execution_node
            }
            Some(("message", message_id)) => {
                if let Some(subject) =
                    held_answer_subject_from(&self.connection, network, conversation, message_id)?
                {
                    subject.node
                } else {
                    match self.input_route(network, conversation, message_id)? {
                        Some(route) => {
                            self.resolve_input_target(network, &route.target)?
                                .owner_node
                        }
                        None => self
                            .conversation(network, conversation)?
                            .and_then(|conversation| conversation.owner)
                            .context("conversation has no scheduling Node")?,
                    }
                }
            }
            _ => self
                .conversation(network, conversation)?
                .and_then(|conversation| conversation.owner)
                .context("conversation has no scheduling Node")?,
        };
        self.bind_peer_command(
            network,
            &owner,
            self.peer_delivery_target(network, conversation, trigger)?,
            payload,
            self.peer_source(network, conversation, trigger)?,
        )
    }

    pub(crate) fn peer_delivery_saved(
        &self,
        network: &str,
        conversation: &str,
        trigger: &str,
    ) -> Result<bool> {
        let payload = PeerCommand::Deliver {
            conversation_id: conversation.into(),
            trigger: trigger.into(),
        };
        Ok(self
            .bound_peer_command(network, &payload)?
            .is_some_and(|bound| bound.ingress_saved))
    }

    pub(crate) fn mark_peer_saved(&mut self, network: &str, command_id: &str) -> Result<()> {
        let key = key(OUTBOUND, network, command_id);
        let mut outbound: PeerOutbound =
            load(&self.connection, &key)?.context("outbound command not found")?;
        outbound.ingress_saved = true;
        save(&self.connection, &key, &outbound)
    }

    pub(crate) fn peer_dispatch_command(
        &mut self,
        network: &str,
        spec: &WorkRequest,
    ) -> Result<PeerOutbound> {
        let payload = PeerCommand::Dispatch {
            spec: Box::new(spec.clone()),
        };
        if let Some(bound) = self.bound_peer_command(network, &payload)? {
            ensure!(
                bound.envelope.payload == payload,
                "work ID reused with a different dispatch"
            );
            return Ok(bound);
        }
        let source = match spec.trigger.split_once(':') {
            Some(("assignment", assignment_id)) => {
                let (revision, refs) = super::workspace::assignment_work_source_from(
                    &self.connection,
                    network,
                    assignment_id,
                )?;
                (Some(revision), 0, refs)
            }
            _ => self.peer_source(network, &spec.conversation, &spec.trigger)?,
        };
        self.bind_peer_command(
            network,
            &spec.target,
            format!("work:{}", spec.work_id),
            payload,
            source,
        )
    }

    pub(crate) fn peer_stop_command(
        &mut self,
        network: &str,
        request: &StopRequest,
    ) -> Result<PeerOutbound> {
        let payload = PeerCommand::Stop {
            conversation_id: request.conversation.clone(),
            work_id: request.work_id.clone(),
        };
        if let Some(bound) = self.bound_peer_command(network, &payload)? {
            return Ok(bound);
        }
        let source = if let Some(dispatch) = self.dispatch(&request.work_id)? {
            self.peer_source(network, &request.conversation, &dispatch.spec.trigger)?
        } else {
            let event = event_about_from(
                &self.connection,
                network,
                &request.conversation,
                "work_dispatched",
                &request.work_id,
            )?
            .context("work dispatch not found")?;
            (
                None,
                event.event.created_at_ms.max(0) as u64,
                vec![event.hash],
            )
        };
        self.bind_peer_command(
            network,
            &request.target,
            format!("work:{}", request.work_id),
            payload,
            source,
        )
    }

    fn peer_original_metadata(&self, network: &str, envelope: &PeerEnvelope) -> Result<bool> {
        let source = match &envelope.payload {
            PeerCommand::Deliver {
                conversation_id,
                trigger,
            } => {
                ensure!(
                    self.peer_delivery_target(network, conversation_id, trigger)?
                        == envelope.target,
                    AdmissionRejected::new("delivery target does not match its signed input")
                );
                Some(self.peer_source(network, conversation_id, trigger)?)
            }
            PeerCommand::Dispatch { spec } => {
                if let Some(("assignment", assignment_id)) = spec.trigger.split_once(':') {
                    let assignment =
                        self.delivery_assignment(network, &spec.conversation, assignment_id)?;
                    ensure!(
                        assignment.execution_node == spec.dispatcher
                            && envelope.owner_epoch == assignment.bot_owner_epoch
                            && envelope.sequence == 0
                            && envelope.target == format!("work:{}", spec.work_id),
                        AdmissionRejected::new("Work does not match its frozen assignment")
                    );
                    let revision = envelope
                        .revision
                        .context("assignment Work revision is missing")?;
                    ensure!(
                        revision >= assignment.started_revision,
                        AdmissionRejected::new("Work revision predates its assignment")
                    );
                    if !super::workspace::assignment_work_source_matches_from(
                        &self.connection,
                        network,
                        assignment_id,
                        revision,
                        &envelope.causal_refs,
                    )? {
                        return Ok(false);
                    }
                    None
                } else {
                    Some(self.peer_source(network, &spec.conversation, &spec.trigger)?)
                }
            }
            PeerCommand::Stop { .. } => None,
        };
        if let Some((revision, sequence, refs)) = source {
            ensure!(
                revision == envelope.revision
                    && sequence == envelope.sequence
                    && refs == envelope.causal_refs,
                AdmissionRejected::new("command metadata does not match its signed source")
            );
        }
        Ok(true)
    }

    pub(crate) fn peer_owner_proof(
        &self,
        network: &str,
        local: &str,
        envelope: &PeerEnvelope,
    ) -> Result<Option<Value>> {
        let original: PeerAdmission = load(
            &self.connection,
            &key(INBOUND, network, &envelope.command_id),
        )?
        .context("original peer command not found")?;
        ensure!(
            original.network_id == network && original.envelope == *envelope,
            "command no longer matches its complete saved intent"
        );
        if original.state == PeerAdmissionState::OwnerAccepted {
            return Ok(Some(original.response));
        }
        for source in &envelope.causal_refs {
            if self.event(network, source)?.is_none() {
                return Ok(None);
            }
        }
        match &envelope.payload {
            PeerCommand::Deliver {
                conversation_id,
                trigger,
            } => {
                if let Some(("message", message_id)) = trigger.split_once(':')
                    && held_answer_subject_from(
                        &self.connection,
                        network,
                        conversation_id,
                        message_id,
                    )?
                    .is_some()
                {
                    let Some(receipt) =
                        self.held_answer_receipt(network, conversation_id, message_id)?
                    else {
                        return Ok(None);
                    };
                    if !self.peer_original_metadata(network, envelope)? {
                        return Ok(None);
                    }
                    return Ok(Some(json!({"receipt":receipt})));
                }
                let mut job = self.scheduler_job(conversation_id, trigger)?;
                if job.is_none()
                    && let Some(("assignment", assignment_id)) = trigger.split_once(':')
                {
                    let assignment = self.workspace_assignment(network, assignment_id)?;
                    if let Some(source) =
                        assignment.and_then(|assignment| assignment.trigger_message_id)
                    {
                        let source = crate::scheduling::message_trigger(&source);
                        if self.peer_delivery_receipt_matches(
                            network,
                            conversation_id,
                            trigger,
                            &source,
                        )? {
                            job = self.scheduler_job(conversation_id, &source)?;
                        }
                    }
                }
                let Some(job) = job else {
                    return Ok(None);
                };
                if !self.peer_original_metadata(network, envelope)? {
                    return Ok(None);
                }
                Ok(Some(json!({"receipt":job.receipt(network)})))
            }
            PeerCommand::Dispatch { spec } => {
                let Some(admitted) = self.accepted_work(network, spec)? else {
                    return Ok(None);
                };
                if !self.peer_original_metadata(network, envelope)? {
                    return Ok(None);
                }
                Ok(Some(
                    json!({"work_id":admitted.record.work_id,"state":admitted.record.state.name()}),
                ))
            }
            PeerCommand::Stop {
                conversation_id,
                work_id,
            } => {
                let held = self.stop_requests()?.iter().any(|stop| {
                    stop.work_id == *work_id
                        && stop.conversation == *conversation_id
                        && stop.target == local
                });
                let cancelling = self
                    .scheduler_job(conversation_id, &crate::scheduling::task_trigger(work_id))?
                    .is_some_and(|job| {
                        matches!(
                            job.state,
                            crate::scheduling::JobState::Cancelling { .. }
                                | crate::scheduling::JobState::Cancelled { .. }
                        )
                    });
                if !held && !cancelling {
                    return Ok(None);
                }
                let work = self
                    .work(network, work_id)?
                    .filter(|work| work.conversation_id == *conversation_id)
                    .context("stopped work source not found")?;
                ensure!(
                    work.target_node == local,
                    "stop fact belongs to another execution Node"
                );
                let dispatch = event_about_from(
                    &self.connection,
                    network,
                    conversation_id,
                    "work_dispatched",
                    work_id,
                )?;
                let dispatch_matches = dispatch.is_some_and(|event| {
                    envelope.revision.is_none()
                        && envelope.sequence == event.event.created_at_ms.max(0) as u64
                        && envelope.causal_refs == [event.hash]
                });
                if !dispatch_matches {
                    let parent = self.peer_source(network, conversation_id, &work.trigger)?;
                    ensure!(
                        parent.0 == envelope.revision
                            && parent.1 == envelope.sequence
                            && parent.2 == envelope.causal_refs,
                        "stop metadata does not match its saved work intent"
                    );
                }
                Ok(Some(
                    json!({"work_id":work_id,"interrupt_requested":true,"confirmed":false,"retrying":true}),
                ))
            }
        }
    }

    pub(crate) fn peer_command_dependencies(
        &self,
        network: &str,
        envelope: &PeerEnvelope,
    ) -> Result<bool> {
        for hash in &envelope.causal_refs {
            let effective: Option<bool> = self
                .connection
                .query_row(
                    "SELECT effective FROM events WHERE network_id=? AND hash=?",
                    params![network, hash],
                    |row| row.get(0),
                )
                .optional()?;
            let Some(effective) = effective else {
                return Ok(false);
            };
            ensure!(
                effective,
                AdmissionRejected::new("command source is no longer authorized")
            );
        }
        let Some(conversation) = self.conversation(network, envelope.payload.conversation())?
        else {
            return Ok(false);
        };
        let held = held_delivery_subject_from(&self.connection, network, &envelope.payload)?;
        ensure!(
            if held.is_some() {
                envelope.owner_epoch == 0
            } else {
                conversation.owner_epoch == envelope.owner_epoch
            },
            AdmissionRejected::new("command belongs to another owner epoch")
        );
        if !self.peer_original_metadata(network, envelope)? {
            return Ok(false);
        }
        if let PeerCommand::Dispatch { spec } = &envelope.payload
            && let Some(("assignment", assignment_id)) = spec.trigger.split_once(':')
        {
            let assignment =
                self.delivery_assignment(network, &spec.conversation, assignment_id)?;
            let task = self
                .workspace_task(network, &assignment.task_id)?
                .context("assignment task is missing")?;
            ensure!(
                envelope.revision == Some(task.revision)
                    && super::workspace::assignment_work_allowed_from(
                        &self.connection,
                        network,
                        assignment_id,
                        &spec.dispatcher,
                        &spec.conversation
                    )?,
                AdmissionRejected::new("assignment no longer authorizes this Work revision")
            );
        }
        Ok(true)
    }
}
