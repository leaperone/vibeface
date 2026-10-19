use super::*;
use crate::work::WorkState;

const ARTIFACT_PAGE: usize = 65_536;

fn artifact_page(hash: &str, bytes: &[u8], offset: u64, limit: u64) -> Result<Value> {
    ensure!(
        (4..=ARTIFACT_PAGE as u64).contains(&limit),
        "artifact limit must be between 4 and 65536 bytes"
    );
    let text = std::str::from_utf8(bytes).context("artifact is not UTF-8 text")?;
    let start = usize::try_from(offset).context("artifact offset is out of range")?;
    ensure!(
        start <= bytes.len() && text.is_char_boundary(start),
        "artifact offset is not a valid UTF-8 boundary"
    );
    let mut end = start.saturating_add(limit as usize).min(bytes.len());
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    let next = (end < bytes.len()).then_some(end);
    Ok(
        json!({"blob_hash":hash,"text":&text[start..end],"offset":offset,"next_offset":next,"size":bytes.len()}),
    )
}

impl Runtime {
    pub(super) async fn read_artifact(&self, access: &Access, input: &Value) -> Result<Value> {
        ensure!(access.human, "artifact access requires the user");
        let scope = field(input, "conversation_id")?;
        self.require_scope(access, scope).await?;
        let hash = field(input, "blob_hash")?;
        let (_, bytes) = self
            .store
            .lock()
            .await
            .read_blob(&access.network_id, scope, hash)?;
        artifact_page(
            hash,
            &bytes,
            input["offset"].as_u64().unwrap_or(0),
            input["limit"].as_u64().unwrap_or(ARTIFACT_PAGE as u64),
        )
    }

    pub(super) async fn conversation_snapshot(
        self: &Arc<Self>,
        access: &Access,
        input: &Value,
    ) -> Result<Value> {
        let scope = field(input, "conversation_id")?;
        self.require_scope(access, scope).await?;
        let network = &access.network_id;
        let (jobs, checks) = self.scheduler_snapshot_jobs(network, scope).await?;
        let store = self.store.lock().await;
        let mut pending_checks = checks
            .iter()
            .map(|check| {
                let mut value = json!(check);
                value["kind"] = json!("manager_process");
                value
            })
            .collect::<Vec<_>>();
        let records = store
            .work_records(network)?
            .into_iter()
            .filter(|record| record.spec.conversation == scope)
            .map(|record| (record.spec.work_id.clone(), record))
            .collect::<BTreeMap<_, _>>();
        let mut works = Vec::new();
        for work in store.works(network, scope)? {
            let mut value = json!(work);
            let record = records.get(&work.work_id);
            // Answer forms come from the target's journal; other devices see only the summary.
            if let Some(WorkState::WaitingApproval { approvals, .. }) = record.map(|r| &r.state) {
                for approval in value["open_approvals"].as_array_mut().into_iter().flatten() {
                    if let Some(pending) = approvals
                        .iter()
                        .find(|p| approval["request_id"] == p.request_id)
                    {
                        approval["form"] = executor::request_form(&pending.method, &pending.params);
                        approval["answering"] = json!(pending.answering);
                    }
                }
            }
            if let Some(WorkState::Unverified {
                evidence, probe, ..
            }) = record.map(|r| &r.state)
            {
                value["state"] = json!("unverified");
                pending_checks.push(json!({"check_id":format!("work:{}",work.work_id),"kind":"work",
                    "conversation_id":scope,"work_id":work.work_id,"detail":evidence,"probe":probe}));
            } else if let Some(record) = record {
                value["state"] = json!(record.state.name());
            }
            works.push(value);
        }
        let settings = self.settings_snapshot();
        let manager = settings.manager.as_ref();
        Ok(json!({
            "conversation":store.conversation(network, scope)?,
            "bot":store.bot_for_conversation(network, scope)?,
            "messages":store.messages(network, scope)?,
            "works":works,
            "autonomy":store.autonomy(network, scope)?,
            "pending_checks":pending_checks,
            "local_node":self.identity.peer_id()?,
            "scheduler":{"jobs":jobs,"provider":manager.map(|m|&m.provider),"model":manager.map(|m|&m.model),
                "capabilities":manager.and_then(|m| match m.provider.as_str() {
                    "codex_sdk"=>Some(crate::manager::ManagerKind::CodexSdk.capabilities()),
                    "cursor_sdk"=>Some(crate::manager::ManagerKind::CursorSdk.capabilities()),
                    _=>None})},
        }))
    }
}
