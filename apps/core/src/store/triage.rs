use super::Store;
use crate::{
    manager::{ManagerError, StartEvidence},
    triage::AgentEvent,
};
use anyhow::{Context as _, Result, ensure};
use rusqlite::{Connection, TransactionBehavior, params};
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
        let event = serde_json::from_value(value).context("invalid stored triage event")?;
        Ok(Self { event, claim })
    }

    fn encode(&self) -> Result<String> {
        let mut value = serde_json::to_value(&self.event)?;
        if let Some(claim) = &self.claim {
            value
                .as_object_mut()
                .context("triage event must be an object")?
                .insert("_triage".into(), serde_json::to_value(claim)?);
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
}

impl TriageAttempt {
    pub(crate) fn id(&self) -> &str {
        &self.id
    }
    pub(crate) fn events(&self) -> &[AgentEvent] {
        &self.events
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

fn queued(connection: &Connection) -> Result<Vec<StoredTriageEvent>> {
    let mut statement =
        connection.prepare("SELECT event FROM triage_queue ORDER BY ts_ms,event_id")?;
    let rows = statement.query_map([], |row| row.get::<_, String>(0))?;
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

impl Store {
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
        let row = StoredTriageEvent { event, claim: None };
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
        Ok(queued(&self.connection)?
            .into_iter()
            .filter(|row| row.eligible(context))
            .map(|row| row.event)
            .collect())
    }

    pub(crate) fn unknown_triage_events(
        &self,
        active: Option<(&str, &str)>,
    ) -> Result<Vec<AgentEvent>> {
        Ok(queued(&self.connection)?
            .into_iter()
            .filter(|row| row.claim.is_some() && !row.eligible(active))
            .map(|row| row.event)
            .collect())
    }

    pub(crate) fn recover_triage(&mut self) -> Result<()> {
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        for mut row in queued(&tx)? {
            if let Some(claim) = &mut row.claim {
                claim.state = ClaimState::Unknown;
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
        for mut row in queued(&tx)?
            .into_iter()
            .filter(|row| row.event.request_id.as_deref() == Some(request))
        {
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
        let Some(mut row) = queued(&tx)?
            .into_iter()
            .find(|row| row.event.event_id == event)
        else {
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
        let mut rows: Vec<_> = queued(&tx)?
            .into_iter()
            .filter(|row| row.claim.is_none())
            .take(max)
            .collect();
        if rows.is_empty() {
            return Ok(TriageAdmission::Empty);
        }
        let key = format!("triage:{network}");
        let status = budget(&tx, &key, limit, now)?;
        if status.paused_reason.is_some() {
            return Ok(TriageAdmission::Paused(status));
        }
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
            params![key, status.day, status.used + 1],
        )?;
        tx.commit()?;
        Ok(TriageAdmission::Claimed(TriageAttempt {
            id,
            boot: boot.into(),
            budget_key: key,
            day: status.day,
            units: 1,
            events: rows.into_iter().map(|row| row.event).collect(),
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
        for mut row in queued(&tx)? {
            if !row.eligible(Some((&attempt.boot, &attempt.id))) {
                continue;
            }
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
        let mut status = TriageStatus {
            budget: budget(&self.connection, &format!("triage:{network}"), limit, now)?,
            pending_events: 0,
            inflight_events: 0,
            unknown_events: 0,
        };
        for row in queued(&self.connection)? {
            if row.claim.is_none() {
                status.pending_events += 1;
            } else if row.eligible(active.map(|id| (boot, id))) {
                status.inflight_events += 1;
            } else {
                status.unknown_events += 1;
            }
        }
        Ok(status)
    }
}
