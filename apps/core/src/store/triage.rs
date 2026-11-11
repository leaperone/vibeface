use super::{Store, log, message_from, questions};
use crate::{
    manager::{ManagerError, StartEvidence},
    model::{EventBody, Message, Role},
    triage::{AgentEvent, AnswerAuthority, HeldAnswerControl, HeldAnswerRequest},
};
use anyhow::{Context as _, Result, ensure};
use rusqlite::{Connection, OptionalExtension as _, TransactionBehavior, params};
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum ClaimState {
    InFlight,
    Unknown,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct TriageClaim {
    attempt: String,
    boot: String,
    state: ClaimState,
}

struct StoredTriageEvent {
    event: AgentEvent,
    claim: Option<TriageClaim>,
    answer: Option<HeldAnswerControl>,
}

impl StoredTriageEvent {
    fn decode(encoded: &str) -> Result<Self> {
        let mut value: Value = serde_json::from_str(encoded)?;
        let fields = value
            .as_object_mut()
            .context("triage event must be an object")?;
        let claim = fields
            .remove("_triage")
            .map(serde_json::from_value)
            .transpose()?;
        let answer = fields
            .remove("_answer")
            .map(serde_json::from_value)
            .transpose()?;
        let event = serde_json::from_value(value).context("invalid stored triage event")?;
        Ok(Self {
            event,
            claim,
            answer,
        })
    }

    fn encode(&self) -> Result<String> {
        let mut value = serde_json::to_value(&self.event)?;
        if let Some(claim) = &self.claim {
            value
                .as_object_mut()
                .context("triage event must be an object")?
                .insert("_triage".into(), serde_json::to_value(claim)?);
        }
        if let Some(answer) = &self.answer {
            value
                .as_object_mut()
                .context("triage event must be an object")?
                .insert("_answer".into(), serde_json::to_value(answer)?);
        }
        Ok(serde_json::to_string(&value)?)
    }

    fn eligible(&self, context: Option<(&str, &str)>) -> bool {
        match (&self.claim, context) {
            (None, None) => true,
            (Some(claim), Some((boot, attempt))) => {
                claim.boot == boot
                    && claim.attempt == attempt
                    && claim.state == ClaimState::InFlight
            }
            _ => false,
        }
    }
}

pub(crate) struct TriageAttempt {
    id: String,
    boot: String,
    budget_key: String,
    day: String,
    units: u64,
    events: Vec<AgentEvent>,
    answer: Option<Box<HeldAnswerControl>>,
}

impl TriageAttempt {
    pub(crate) fn id(&self) -> &str {
        &self.id
    }
    pub(crate) fn events(&self) -> &[AgentEvent] {
        &self.events
    }
    pub(crate) fn answer(&self) -> Option<&HeldAnswerControl> {
        self.answer.as_deref()
    }
}

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub(crate) struct BudgetStatus {
    pub day: String,
    pub used: u64,
    pub limit: u64,
    pub retry_at_ms: i64,
    pub paused_reason: Option<&'static str>,
}

#[derive(Serialize)]
pub(crate) struct TriageStatus {
    pub budget: BudgetStatus,
    pub pending_events: usize,
    pub inflight_events: usize,
    pub unknown_events: usize,
}

pub(crate) enum TriageAdmission {
    Empty,
    Paused(BudgetStatus),
    Claimed(TriageAttempt),
}

fn queued(
    connection: &Connection,
    selection: &str,
    parameters: impl rusqlite::Params,
) -> Result<Vec<StoredTriageEvent>> {
    let mut statement =
        connection.prepare(&format!("SELECT event FROM triage_queue {selection}"))?;
    let rows = statement.query_map(parameters, |row| row.get::<_, String>(0))?;
    rows.map(|row| StoredTriageEvent::decode(&row?)).collect()
}

fn save(connection: &Connection, row: &StoredTriageEvent) -> Result<()> {
    connection.execute(
        "UPDATE triage_queue SET request_id=?,event=? WHERE ts_ms=? AND event_id=?",
        params![
            row.event.request_id,
            row.encode()?,
            row.event.ts_ms.max(0),
            row.event.event_id
        ],
    )?;
    Ok(())
}

fn budget(connection: &Connection, key: &str, limit: u64, now: i64) -> Result<BudgetStatus> {
    let (day, used): (String, u64) = connection.query_row(
        "SELECT date(?1 / 1000,'unixepoch'), COALESCE((SELECT used FROM budget_days WHERE network_id=?2 AND day=date(?1 / 1000,'unixepoch')),0)",
        params![now, key], |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    Ok(BudgetStatus {
        day,
        used,
        limit,
        retry_at_ms: (now.div_euclid(86_400_000) + 1) * 86_400_000,
        paused_reason: (used >= limit).then_some("budget_exhausted"),
    })
}

fn answer_input_from(
    connection: &Connection,
    control: &HeldAnswerControl,
) -> Result<(Value, Message)> {
    let question = questions::question_from(connection, &control.network, &control.question_id)?
        .context("the answer's question no longer exists")?;
    let (rank, author, closed): (String, String, Option<String>) = connection.query_row(
        "SELECT rank,author,closed FROM questions WHERE network_id=? AND question_id=?",
        params![control.network, control.question_id],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
    )?;
    ensure!(
        rank.rsplit(':').next() == Some(control.question_hash.as_str())
            && author == control.request.node
            && closed.is_none()
            && question["conversation_id"] == control.conversation_id
            && question["about"]["node"] == control.request.node
            && question["about"]["request_id"] == control.request.request_id
            && question["about"]["session_id"] == control.request.session_id
            && question["about"]["agent"] == serde_json::to_value(control.request.provider)?,
        "the answer no longer belongs to this request and question"
    );
    let message = message_from(
        connection,
        &control.network,
        &control.conversation_id,
        &control.answer_id,
    )?
    .context("the standing answer no longer exists")?;
    ensure!(
        control.answer_id == questions::answer_id(&control.question_id)
            && message.event_hash == control.answer_hash
            && message.role == Role::User
            && message.answers.as_deref() == Some(control.question_id.as_str()),
        "the standing answer changed"
    );
    let answer = log::event_from(connection, &control.network, &control.answer_hash)?
        .context("the answer event no longer exists")?;
    ensure!(
        matches!(
            &answer.body,
            EventBody::MessagePosted {
                attestation: Some(_),
                ..
            }
        ) || (control.authority == AnswerAuthority::DecisionToken
            && answer.author == control.request.node),
        "a held request needs the user's signed answer or an authorized decision token"
    );
    approval_answer_result(&question, &message.text, &control.request.response)?;
    Ok((question, message))
}

pub(crate) fn approval_answer_result(
    question: &Value,
    text: &str,
    response: &crate::triage::AnswerResponse,
) -> Result<Option<Value>> {
    let crate::triage::AnswerResponse::Approval { allow_once, deny } = response else {
        return Ok(None);
    };
    let choices: Vec<_> = question["choices"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|choice| choice["label"].as_str() == Some(text))
        .collect();
    ensure!(choices.len() == 1, "choose one of this approval's actions");
    let result = match choices[0]["action"].as_str() {
        Some("allow_once") => allow_once
            .as_ref()
            .context("this request does not allow a one-time approval")?,
        Some("deny") => deny,
        _ => anyhow::bail!("this choice has no trusted approval action"),
    };
    Ok(Some(result.clone()))
}

pub(super) fn queue_answer_control_from(
    connection: &Connection,
    network: &str,
    question: &str,
    request: &HeldAnswerRequest,
    authority: AnswerAuthority,
    now: i64,
) -> Result<bool> {
    let (scope, rank): (String, String) = connection.query_row(
        "SELECT conversation_id,rank FROM questions WHERE network_id=? AND question_id=?",
        params![network, question],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    let answer_id = questions::answer_id(question);
    let Some(message) = message_from(connection, network, &scope, &answer_id)? else {
        return Ok(false);
    };
    let control = HeldAnswerControl {
        network: network.into(),
        conversation_id: scope,
        question_id: question.into(),
        question_hash: rank
            .rsplit(':')
            .next()
            .context("invalid question rank")?
            .into(),
        answer_id,
        answer_hash: message.event_hash,
        request: request.clone(),
        authority,
    };
    answer_input_from(connection, &control)?;
    let event_id = format!("answer:{}:{}", control.question_hash, control.answer_hash);
    let first = connection.execute(
        "INSERT INTO triage_seen(event_id,ts_ms) VALUES(?,?) ON CONFLICT(event_id) DO NOTHING",
        params![event_id, now],
    )?;
    if first == 0 {
        return Ok(false);
    }
    let row = StoredTriageEvent {
        event: AgentEvent {
            event_id,
            node: request.node.clone(),
            agent: request.provider,
            event: crate::triage::Kind::NeedsInput,
            session_id: request.session_id.clone(),
            turn_id: request.turn_id.clone(),
            request_id: Some(request.request_id.clone()),
            ts_ms: now,
            ..Default::default()
        },
        claim: None,
        answer: Some(control),
    };
    connection.execute(
        "INSERT INTO triage_queue(ts_ms,event_id,request_id,event) VALUES(?,?,?,?)",
        params![
            row.event.ts_ms,
            row.event.event_id,
            row.event.request_id,
            row.encode()?
        ],
    )?;
    let receipt = crate::scheduling::AdmissionReceipt {
        network_id: network.into(),
        conversation_id: row
            .answer
            .as_ref()
            .context("answer control missing")?
            .conversation_id
            .clone(),
        trigger: crate::scheduling::message_trigger(
            &row.answer
                .as_ref()
                .context("answer control missing")?
                .answer_id,
        ),
        state: "answer_control_saved".into(),
    };
    connection.execute(
        "INSERT INTO triage_handled(event_id,ts_ms,result) VALUES(?,?,?) ON CONFLICT(event_id) DO NOTHING",
        params![row.event.event_id, now, serde_json::to_string(&serde_json::json!({"control":row.answer,"receipt":receipt}))?],
    )?;
    Ok(true)
}

pub(crate) fn held_answer_receipt_from(
    connection: &Connection,
    network: &str,
    conversation: &str,
    message_id: &str,
) -> Result<Option<crate::scheduling::AdmissionReceipt>> {
    let Some(message) = message_from(connection, network, conversation, message_id)? else {
        return Ok(None);
    };
    let Some(question_id) = &message.answers else {
        return Ok(None);
    };
    let rank: Option<String> = connection
        .query_row(
            "SELECT rank FROM questions WHERE network_id=? AND question_id=? AND conversation_id=?",
            params![network, question_id, conversation],
            |row| row.get(0),
        )
        .optional()?;
    let Some(rank) = rank else {
        return Ok(None);
    };
    let event_id = format!(
        "answer:{}:{}",
        rank.rsplit(':').next().context("invalid question rank")?,
        message.event_hash
    );
    let saved: Option<String> = connection
        .query_row(
            "SELECT result FROM triage_handled WHERE event_id=?",
            [event_id],
            |row| row.get(0),
        )
        .optional()?;
    let Some(saved) = saved else {
        return Ok(None);
    };
    let saved: Value = serde_json::from_str(&saved)?;
    let control: HeldAnswerControl = serde_json::from_value(saved["control"].clone())?;
    ensure!(
        control.network == network
            && control.conversation_id == conversation
            && control.answer_id == message_id
            && control.answer_hash == message.event_hash,
        "saved answer control belongs to another answer"
    );
    answer_input_from(connection, &control)?;
    Ok(Some(serde_json::from_value(saved["receipt"].clone())?))
}

impl Store {
    pub(crate) fn has_pending_answer_controls(&self) -> Result<bool> {
        Ok(self.connection.query_row(
            "SELECT EXISTS(SELECT 1 FROM triage_queue WHERE json_type(event,'$._triage') IS NULL AND json_type(event,'$._answer') IS NOT NULL)",
            [],
            |row| row.get(0),
        )?)
    }
    pub(crate) fn held_answer_receipt(
        &self,
        network: &str,
        conversation: &str,
        message_id: &str,
    ) -> Result<Option<crate::scheduling::AdmissionReceipt>> {
        held_answer_receipt_from(&self.connection, network, conversation, message_id)
    }

    pub(crate) fn queue_answer_control(
        &mut self,
        network: &str,
        question: &str,
        request: &HeldAnswerRequest,
        authority: AnswerAuthority,
        now: i64,
    ) -> Result<bool> {
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let queued = queue_answer_control_from(&tx, network, question, request, authority, now)?;
        tx.commit()?;
        Ok(queued)
    }

    pub(crate) fn answer_control_input(
        &self,
        control: &HeldAnswerControl,
    ) -> Result<(Value, Message)> {
        answer_input_from(&self.connection, control)
    }

    pub(crate) fn answer_control_attempt(
        &self,
        boot: &str,
        attempt: &str,
    ) -> Result<Option<HeldAnswerControl>> {
        Ok(queued(
            &self.connection,
            "WHERE json_extract(event,'$._triage.boot')=? AND json_extract(event,'$._triage.attempt')=? AND json_type(event,'$._answer') IS NOT NULL ORDER BY ts_ms,event_id LIMIT 1",
            params![boot, attempt],
        )?.into_iter().next().and_then(|row| row.answer))
    }

    pub(crate) fn validate_answer_control_claim(
        &self,
        boot: &str,
        attempt: &str,
        control: &HeldAnswerControl,
    ) -> Result<()> {
        ensure!(
            queued(
                &self.connection,
                "WHERE json_extract(event,'$._triage.boot')=? AND json_extract(event,'$._triage.attempt')=? AND json_extract(event,'$._triage.state')='in_flight' ORDER BY ts_ms,event_id",
                params![boot, attempt],
            )?.iter().any(|row| {
                row.eligible(Some((boot, attempt)))
                    && row.answer.as_ref() == Some(control)
                    && row.event.request_id.as_deref() == Some(control.request.request_id.as_str())
            }),
            "answer control claim is no longer live"
        );
        answer_input_from(&self.connection, control)?;
        Ok(())
    }

    pub(crate) fn queue_triage_event(&mut self, event: AgentEvent, now: i64) -> Result<bool> {
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let first = tx.execute(
            "INSERT INTO triage_seen(event_id,ts_ms) VALUES(?,?) ON CONFLICT(event_id) DO NOTHING",
            params![event.event_id, now],
        )?;
        if first == 0 {
            return Ok(false);
        }
        let row = StoredTriageEvent {
            event,
            claim: None,
            answer: None,
        };
        tx.execute(
            "INSERT INTO triage_queue(ts_ms,event_id,request_id,event) VALUES(?,?,?,?)",
            params![
                row.event.ts_ms.max(0),
                row.event.event_id,
                row.event.request_id,
                row.encode()?
            ],
        )?;
        tx.commit()?;
        Ok(true)
    }

    pub(crate) fn triage_events(&self, context: Option<(&str, &str)>) -> Result<Vec<AgentEvent>> {
        let rows = match context {
            None => queued(
                &self.connection,
                "WHERE json_type(event,'$._triage') IS NULL AND json_type(event,'$._answer') IS NULL ORDER BY ts_ms,event_id",
                [],
            )?,
            Some((boot, attempt)) => queued(
                &self.connection,
                "WHERE json_extract(event,'$._triage.boot')=? AND json_extract(event,'$._triage.attempt')=? AND json_extract(event,'$._triage.state')='in_flight' AND json_type(event,'$._answer') IS NULL ORDER BY ts_ms,event_id",
                params![boot, attempt],
            )?,
        };
        Ok(rows
            .into_iter()
            .filter(|row| row.eligible(context))
            .map(|row| row.event)
            .collect())
    }

    pub(crate) fn unknown_triage_events(
        &self,
        active: Option<(&str, &str)>,
    ) -> Result<Vec<AgentEvent>> {
        Ok(queued(
            &self.connection,
            "WHERE json_type(event,'$._triage') IS NOT NULL ORDER BY ts_ms,event_id",
            [],
        )?
        .into_iter()
        .filter(|row| !row.eligible(active))
        .map(|row| row.event)
        .collect())
    }

    pub(crate) fn recover_triage(&mut self) -> Result<()> {
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        for mut row in queued(&tx, "ORDER BY ts_ms,event_id", [])? {
            if let Some(claim) = &mut row.claim {
                claim.state = ClaimState::Unknown;
            } else if row.answer.is_some() {
                row.claim = Some(TriageClaim {
                    attempt: String::new(),
                    boot: String::new(),
                    state: ClaimState::Unknown,
                });
            }
            row.event.request_id = None;
            row.event.hold = None;
            save(&tx, &row)?;
        }
        tx.commit()?;
        Ok(())
    }

    pub(crate) fn finish_triage_request(&mut self, request: &str, reviewed: bool) -> Result<()> {
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        for mut row in queued(&tx, "WHERE request_id=? ORDER BY ts_ms,event_id", [request])? {
            if reviewed && row.claim.is_none() {
                tx.execute(
                    "DELETE FROM triage_queue WHERE ts_ms=? AND event_id=?",
                    params![row.event.ts_ms.max(0), row.event.event_id],
                )?;
                continue;
            }
            row.event.request_id = None;
            row.event.hold = None;
            save(&tx, &row)?;
        }
        tx.commit()?;
        Ok(())
    }

    pub(crate) fn attach_triage_request(&mut self, event: &str, request: &str) -> Result<bool> {
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let Some(mut row) = queued(&tx, "WHERE event_id=?", [event])?.into_iter().next() else {
            return Ok(false);
        };
        if row.event.request_id.as_deref() == Some(request) {
            return Ok(true);
        }
        if row.claim.is_some() {
            return Ok(false);
        }
        row.event.request_id = Some(request.into());
        save(&tx, &row)?;
        tx.commit()?;
        Ok(true)
    }

    pub(crate) fn admit_triage(
        &mut self,
        network: &str,
        boot: &str,
        max: usize,
        limit: u64,
        now: i64,
    ) -> Result<TriageAdmission> {
        ensure!(limit <= i64::MAX as u64, "invalid budget limit");
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let mut rows = queued(
            &tx,
            "WHERE json_type(event,'$._triage') IS NULL AND json_type(event,'$._answer') IS NOT NULL ORDER BY ts_ms,event_id LIMIT 1",
            [],
        )?;
        if rows.is_empty() {
            rows = queued(
                &tx,
                "WHERE json_type(event,'$._triage') IS NULL AND json_type(event,'$._answer') IS NULL ORDER BY ts_ms,event_id LIMIT ?",
                [i64::try_from(max).context("triage batch is too large")?],
            )?;
        }
        if rows.is_empty() {
            return Ok(TriageAdmission::Empty);
        }
        let key = format!("triage:{network}");
        let status = budget(&tx, &key, limit, now)?;
        if status.paused_reason.is_some() && rows[0].answer.is_none() {
            return Ok(TriageAdmission::Paused(status));
        }
        let used = status
            .used
            .checked_add(1)
            .context("triage budget overflow")?;
        ensure!(used <= i64::MAX as u64, "triage budget overflow");
        let id = uuid::Uuid::new_v4().simple().to_string();
        for row in &mut rows {
            row.claim = Some(TriageClaim {
                attempt: id.clone(),
                boot: boot.into(),
                state: ClaimState::InFlight,
            });
            save(&tx, row)?;
        }
        tx.execute(
            "INSERT INTO budget_days(network_id,day,used) VALUES(?,?,?) ON CONFLICT(network_id,day) DO UPDATE SET used=excluded.used",
            params![key, status.day, used],
        )?;
        tx.commit()?;
        let answer = rows
            .first()
            .and_then(|row| row.answer.clone())
            .map(Box::new);
        Ok(TriageAdmission::Claimed(TriageAttempt {
            id,
            boot: boot.into(),
            budget_key: key,
            day: status.day,
            units: 1,
            events: rows.into_iter().map(|row| row.event).collect(),
            answer,
        }))
    }

    pub(crate) fn finish_triage(
        &mut self,
        attempt: TriageAttempt,
        result: &Result<()>,
    ) -> Result<()> {
        let not_started = result
            .as_ref()
            .err()
            .and_then(|error| error.downcast_ref::<ManagerError>())
            .is_some_and(|error| error.start == StartEvidence::NotStarted);
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        if not_started {
            ensure!(
                tx.execute(
                    "UPDATE budget_days SET used=used-? WHERE network_id=? AND day=? AND used>=?",
                    params![
                        attempt.units,
                        attempt.budget_key,
                        attempt.day,
                        attempt.units
                    ],
                )? == 1,
                "triage reservation charge is missing"
            );
        }
        for mut row in queued(
            &tx,
            "WHERE json_extract(event,'$._triage.boot')=? AND json_extract(event,'$._triage.attempt')=? AND json_extract(event,'$._triage.state')='in_flight' ORDER BY ts_ms,event_id",
            params![attempt.boot, attempt.id],
        )? {
            ensure!(
                row.eligible(Some((&attempt.boot, &attempt.id))),
                "triage claim changed"
            );
            if result.is_ok() {
                tx.execute(
                    "DELETE FROM triage_queue WHERE ts_ms=? AND event_id=?",
                    params![row.event.ts_ms.max(0), row.event.event_id],
                )?;
            } else {
                if not_started {
                    row.claim = None;
                } else if let Some(claim) = &mut row.claim {
                    claim.state = ClaimState::Unknown;
                }
                save(&tx, &row)?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    pub(crate) fn triage_status(
        &self,
        network: &str,
        boot: &str,
        active: Option<&str>,
        limit: u64,
        now: i64,
    ) -> Result<TriageStatus> {
        let total: usize =
            self.connection
                .query_row("SELECT count(*) FROM triage_queue", [], |row| row.get(0))?;
        let pending_events: usize = self.connection.query_row(
            "SELECT count(*) FROM triage_queue WHERE json_type(event,'$._triage') IS NULL",
            [],
            |row| row.get(0),
        )?;
        let inflight_events: usize = self.connection.query_row(
            "SELECT count(*) FROM triage_queue WHERE json_extract(event,'$._triage.boot')=? AND json_extract(event,'$._triage.attempt')=? AND json_extract(event,'$._triage.state')='in_flight'",
            params![boot, active], |row| row.get(0),
        )?;
        Ok(TriageStatus {
            budget: budget(&self.connection, &format!("triage:{network}"), limit, now)?,
            pending_events,
            inflight_events,
            unknown_events: total - pending_events - inflight_events,
        })
    }
}
