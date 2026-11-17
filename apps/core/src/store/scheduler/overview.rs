//! Batch hydration of an overview's selected job identities and context excerpts.
use super::*;
use std::collections::BTreeMap;

pub(crate) fn jobs(connection: &Connection, keys: &Value) -> Result<Vec<SchedulerJob>> {
    query_jobs_ordered(
        connection,
        "(conversation_id,trigger) IN (SELECT json_extract(value,'$.conversation_id'),json_extract(value,'$.trigger') FROM json_each(?1))",
        [serde_json::to_string(keys)?],
        "sequence,rowid",
    )
}

pub(crate) fn context_input(
    message_id: String,
    stored_state: String,
    detail: String,
    source: String,
) -> Result<Option<Value>> {
    let state: JobState = serde_json::from_str(&detail)?;
    ensure!(
        state.name() == stored_state,
        "scheduler job steer state mismatch"
    );
    let (into, receipt, stage) = match &state {
        JobState::Steering {
            into,
            receipt,
            uncertain,
            ..
        } => (
            into,
            receipt,
            if receipt.is_some() {
                "provider_accepted"
            } else if *uncertain {
                "unknown"
            } else {
                "submitting"
            },
        ),
        JobState::Steered {
            into,
            receipt,
            outcome,
        } => (
            into,
            receipt,
            if *outcome == SteerOutcome::Unknown
                || receipt
                    .as_ref()
                    .and_then(|receipt| receipt.actual_turn_id.as_ref())
                    .is_none()
            {
                "unknown"
            } else {
                "associated"
            },
        ),
        _ => return Ok(None),
    };
    let mut value = json!({"message_id":message_id,"source_event":source,"stage":stage,
        "expected_turn_id":into.turn_id,"actual_turn_id":receipt.as_ref().and_then(|receipt| receipt.actual_turn_id.clone())});
    if let Some(receipt) = receipt {
        value["accepted_at_ms"] = json!(receipt.accepted_at_ms);
    }
    Ok(Some(value))
}

pub(crate) fn contexts(
    connection: &Connection,
    jobs: &[SchedulerJob],
) -> Result<BTreeMap<(String, String), ContextProjection>> {
    let keys = jobs.iter().filter_map(|job| job.state.attempt().map(|attempt| json!({
        "conversation":job.conversation_id,"trigger":job.trigger,"scope":job.scope.storage_key(),"attempt":attempt.id
    }))).collect::<Vec<_>>();
    let mut statement = connection.prepare(
        "WITH selected AS (SELECT json_extract(value,'$.conversation') AS conversation,json_extract(value,'$.trigger') AS trigger,
            json_extract(value,'$.scope') AS scope,json_extract(value,'$.attempt') AS attempt FROM json_each(?1))
         SELECT k.conversation,k.trigger,k.scope,s.native_id,json_extract(s.shown,'$.context'),
            EXISTS(SELECT 1 FROM json_each(s.covers)),
            (SELECT count(*) FROM scheduler_jobs j WHERE j.conversation_id=k.conversation
                AND j.state IN ('steering','steered') AND json_extract(j.detail,'$.into.attempt_id')=k.attempt),
            (SELECT json_group_array(json_object('message',reply_to,'state',state,'detail',detail,'source',event_hash)) FROM (
                SELECT j.reply_to,j.state,j.detail,m.event_hash FROM scheduler_jobs j
                JOIN messages m ON m.network_id=json_extract(s.shown,'$.context.network_id')
                    AND m.conversation_id=j.conversation_id AND m.message_id=j.reply_to
                WHERE j.conversation_id=k.conversation AND j.state IN ('steering','steered')
                    AND json_extract(j.detail,'$.into.attempt_id')=k.attempt ORDER BY j.sequence,j.rowid LIMIT 64))
         FROM selected k JOIN scheduler_sessions s ON s.conversation_id=k.conversation AND s.scope=k.scope
         WHERE json_extract(s.shown,'$.context.attempt_id')=k.attempt")?;
    let mut rows = statement.query([serde_json::to_string(&keys)?])?;
    let mut contexts = BTreeMap::new();
    while let Some(row) = rows.next()? {
        let conversation: String = row.get(0)?;
        let trigger: String = row.get(1)?;
        let scope = SessionScope::from_storage_key(&row.get::<_, String>(2)?)?;
        let native: Option<String> = row.get(3)?;
        let mut context: ContextProjection = serde_json::from_str(&row.get::<_, String>(4)?)?;
        let covers: bool = row.get(5)?;
        let total: usize = row.get(6)?;
        let inputs: Vec<Value> = serde_json::from_str(&row.get::<_, String>(7)?)?;
        let pending_generation = context
            .session_status
            .as_ref()
            .is_some_and(|status| status.pending_generation);
        context.session_status = Some(SessionStatusProjection {
            resumable: !matches!(scope, SessionScope::Routine { .. }) && native.is_some() && covers,
            pending_generation,
        });
        context.scope = scope;
        context.inputs.clear();
        for input in inputs {
            let string = |key: &str| {
                input[key]
                    .as_str()
                    .context("invalid context input")
                    .map(str::to_owned)
            };
            if let Some(value) = context_input(
                string("message")?,
                string("state")?,
                string("detail")?,
                string("source")?,
            )? {
                context.inputs.push(value);
            }
        }
        context.inputs_omitted = total.saturating_sub(context.inputs.len());
        contexts.insert((conversation, trigger), context);
    }
    Ok(contexts)
}

pub(crate) struct JobFacts {
    workspace: super::super::workspace::overview::OverviewFacts,
    task_providers: BTreeMap<String, Provider>,
}
impl JobFacts {
    pub(crate) fn read(
        store: &Store,
        network: &str,
        local: &str,
        jobs: &[SchedulerJob],
    ) -> Result<Self> {
        let work_ids = jobs
            .iter()
            .filter_map(|job| match &job.scope {
                SessionScope::Task { work_id } => Some(work_id),
                _ => None,
            })
            .collect::<Vec<_>>();
        let mut statement = store.connection.prepare(
            "SELECT w.work_id,json_extract(w.dispatch,'$.resolved.provider') FROM json_each(?2) ids
            JOIN works w ON w.network_id=?1 AND w.work_id=ids.value WHERE w.dispatch IS NOT NULL",
        )?;
        let mut rows = statement.query(params![network, serde_json::to_string(&work_ids)?])?;
        let mut task_providers = BTreeMap::new();
        while let Some(row) = rows.next()? {
            task_providers.insert(
                row.get(0)?,
                super::super::enum_from(&row.get::<_, String>(1)?)?,
            );
        }
        Ok(Self {
            workspace: super::super::workspace::overview::OverviewFacts::read(
                &store.connection,
                network,
                local,
                jobs,
            )?,
            task_providers,
        })
    }
    pub(crate) fn task_provider(&self, work: &str) -> Option<Provider> {
        self.task_providers.get(work).copied()
    }
    pub(crate) fn hold(
        &self,
        store: &Store,
        network: &str,
        local: &str,
        job: &SchedulerJob,
        provider: Provider,
        limits: &crate::config::SchedulingLimits,
    ) -> Result<Option<QueueHoldReason>> {
        job_hold_with(
            &store.connection,
            &store.blob_dir,
            network,
            local,
            job,
            provider,
            limits,
            Some(&self.workspace),
        )
    }
}
