//! Read-only scheduler pages, including the current owner's explicitly routed observations.
use super::schedule_delivery::SchedulerProjection;
use super::*;
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use serde::{Deserialize, Serialize};
use vibeface_protocol::history::{
    HISTORY_RESPONSE_BYTES, HistoryPage, HistoryRequest, HistorySection,
};

impl SchedulerProjection {
    pub(super) fn history_page(
        &self,
        store: &Store,
        network: &str,
        conversation: &str,
        section: HistorySection,
        cursor: Option<&str>,
        limit: usize,
        manager: &Value,
    ) -> Result<HistoryPage<Value>> {
        store.conversation_scheduler_page(
            network,
            conversation,
            &self.local,
            section,
            cursor,
            limit,
            |store, job| {
                if section == HistorySection::ScopeJobs {
                    self.scope_job(store, network, job)
                } else {
                    let mut job = job.clone();
                    job.pending_check |=
                        store.scheduler_trigger_has_check(conversation, &job.trigger)?;
                    let mut value = self.job(store, network, &job, &[])?;
                    if value["context"].is_object() {
                        value["context"]["session_status"] = Value::Null;
                    }
                    value["manager"] = manager.clone();
                    value["replied"] = json!(store.scheduler_reply_is_present(
                        network,
                        conversation,
                        &job.message_id
                    )?);
                    Ok(value)
                }
            },
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::{
        test_support::{access, signed_main_input},
        triage::tests::{bot, node},
    };

    #[tokio::test]
    async fn scheduler_history_rpc_is_bounded_read_only_and_owner_scoped() -> Result<()> {
        let (runtime, admin, directory) = node().await?;
        let result = async {
            bot(&runtime, &admin, "history", "History", &[]).await?;
            let network = runtime.network_id().await?;
            for index in 0..12 {
                let id = format!("m{index:03}");
                let body = signed_main_input(&admin, &network, "history", &id, &id, vec![], index + 1)?;
                runtime.append(&network, "history", body).await?;
            }
            runtime.store.call("scheduler_history_rpc.fixture", |store| {
                for index in 0..12 {
                    let id = format!("m{index:03}");
                    store.connection.execute(
                        "INSERT INTO scheduler_jobs(conversation_id,trigger,kind,sequence,basis,reply_to,message_id,state,detail,tool_calls,scope)
                         VALUES('history',?1,'human',?2,'{}',?3,?4,'cancelled',?5,0,'main')",
                        rusqlite::params![format!("message:{id}"), index, id, format!("reply:{id}"), "原文\\\"\n".repeat(20_000)])?;
                }
                // An old attempt for the same trigger still blocks retry of its current job.
                store.connection.execute(
                    "INSERT INTO pending_checks VALUES('older-attempt','history','message:m000','requires readback',1)", [])?;
                Ok(())
            }).await?;
            let before = runtime.store.read("scheduler_history_rpc.before", |store| {
                Ok(store.connection.query_row("SELECT total_changes()", [], |row| row.get::<_, u64>(0))?)
            }).await?;
            let principal = access(network.clone(), true, None);
            let snapshot = runtime.rpc(principal.clone(), json!({"method":"conversation", "conversation_id":"history"})).await?;
            assert!(serde_json::to_vec(&snapshot)?.len() < HISTORY_RESPONSE_BYTES);
            for section in ["jobs", "scope_jobs", "checks", "deliveries", "unanswered", "owner_jobs", "owner_scope_jobs", "owner_memory_conflicts"] {
                let mut cursor: Option<String> = None;
                let mut items = Vec::new();
                for _ in 0..100 {
                    let page = runtime.rpc(principal.clone(), json!({"method":"conversation_history",
                        "conversation_id":"history","section":section,"cursor":cursor,"limit":3})).await?;
                    assert!(serde_json::to_vec(&page)?.len() < HISTORY_RESPONSE_BYTES);
                    assert_eq!(page["section"], section);
                    items.splice(0..0, page["items"].as_array().context("typed history items")?.clone());
                    cursor = page["next_cursor"].as_str().map(str::to_owned);
                    if cursor.is_none() { break; }
                }
                assert!(cursor.is_none(), "section {section} must terminate");
                match section {
                    "jobs" | "scope_jobs" | "owner_jobs" | "owner_scope_jobs" => {
                        assert_eq!(items.len(), 12);
                        assert_eq!(items[0]["trigger"], "message:m000");
                        assert_eq!(items[11]["trigger"], "message:m011");
                        if section.ends_with("jobs") && !section.ends_with("scope_jobs") {
                            assert_eq!(items[0]["pending_check"], true);
                        }
                    }
                    "checks" => assert_eq!(items.len(), 1),
                    _ => assert!(items.is_empty()),
                }
            }
            let report = runtime.owner_jobs(&network, "history").await?;
            assert!(serde_json::to_vec(&report)?.len() < HISTORY_RESPONSE_BYTES);
            for section in ["owner_jobs", "owner_scope_jobs", "owner_memory_conflicts"] {
                assert!(report["history"][section].is_object());
                assert!(report["history_starts"][section].is_string());
            }
            let first = runtime.rpc(principal.clone(), json!({"method":"conversation_history",
                "conversation_id":"history","section":"owner_jobs","limit":1})).await?;
            let cursor = first["next_cursor"].as_str().context("owner cursor")?;
            let unavailable = unavailable_owner_observation(&network, "history", &runtime.identity.device_id()?, 1)?;
            assert_eq!(unavailable["jobs"], json!([]));
            assert_eq!(unavailable["scope_jobs"], json!([]));
            assert_eq!(unavailable["memory_conflicts"], json!([]));
            assert_eq!(unavailable["history"]["owner_jobs"]["complete"], false);
            let resume = unavailable["history"]["owner_jobs"]["next_cursor"].as_str().context("retryable owner start")?;
            let resumed = runtime.rpc(principal.clone(), json!({"method":"conversation_history",
                "conversation_id":"history","section":"owner_jobs","cursor":resume,"limit":1})).await?;
            assert_eq!(resumed["items"], first["items"], "an unavailable owner exposes a complete first-page retry path");
            assert!(runtime.rpc(principal.clone(), json!({"method":"conversation_history",
                "conversation_id":"history","section":"owner_scope_jobs","cursor":cursor})).await.is_err());
            let mut wrapped: OwnerCursor = serde_json::from_slice(&URL_SAFE_NO_PAD.decode(cursor)?)?;
            wrapped.owner_epoch += 1;
            let stale_epoch = URL_SAFE_NO_PAD.encode(serde_json::to_vec(&wrapped)?);
            assert!(runtime.rpc(principal.clone(), json!({"method":"conversation_history",
                "conversation_id":"history","section":"owner_jobs","cursor":stale_epoch})).await.is_err());
            wrapped.owner_epoch -= 1;
            wrapped.owner = "former-owner".into();
            let cursor = URL_SAFE_NO_PAD.encode(serde_json::to_vec(&wrapped)?);
            assert!(runtime.rpc(principal.clone(), json!({"method":"conversation_history",
                "conversation_id":"history","section":"owner_jobs","cursor":cursor})).await.is_err());
            assert!(runtime.rpc(access(network.clone(), false, None), json!({"method":"conversation_history",
                "conversation_id":"history","section":"owner_jobs"})).await.is_err());
            let after = runtime.store.read("scheduler_history_rpc.after", |store| {
                Ok(store.connection.query_row("SELECT total_changes()", [], |row| row.get::<_, u64>(0))?)
            }).await?;
            assert_eq!(before, after, "history and owner metadata reads must not write scheduler state");
            Ok(())
        }.await;
        drop(runtime);
        std::fs::remove_dir_all(directory)?;
        result
    }
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct OwnerCursor {
    network: String,
    conversation: String,
    owner: String,
    owner_epoch: u64,
    section: HistorySection,
    cursor: Option<String>,
}

fn owner_cursor(
    network: &str,
    conversation: &str,
    owner: &str,
    owner_epoch: u64,
    section: HistorySection,
    cursor: Option<&str>,
) -> Result<String> {
    Ok(URL_SAFE_NO_PAD.encode(serde_json::to_vec(&OwnerCursor {
        network: network.into(),
        conversation: conversation.into(),
        owner: owner.into(),
        owner_epoch,
        section,
        cursor: cursor.map(str::to_owned),
    })?))
}

pub(super) fn unavailable_owner_observation(
    network: &str,
    conversation: &str,
    owner: &str,
    owner_epoch: u64,
) -> Result<Value> {
    let mut value = json!({"owner":owner,"reachable":false,
        "jobs":[],"scope_jobs":[],"memory_conflicts":[],
        "memory_staged_count":null,"context":null,"manager":null,
        "history":{},"history_starts":{}});
    for (name, section) in [
        ("owner_jobs", HistorySection::OwnerJobs),
        ("owner_scope_jobs", HistorySection::OwnerScopeJobs),
        (
            "owner_memory_conflicts",
            HistorySection::OwnerMemoryConflicts,
        ),
    ] {
        let cursor = owner_cursor(network, conversation, owner, owner_epoch, section, None)?;
        value["history"][name] = json!({"complete":false,"next_cursor":cursor});
        value["history_starts"][name] = json!(cursor);
    }
    Ok(value)
}

impl Runtime {
    pub(super) async fn scheduler_owner_report(&self, network: &str, scope: &str) -> Result<Value> {
        let local = self.identity.device_id()?;
        let projection = self.scheduler_projection()?;
        let manager = self.scheduler_manager_metadata();
        let context = self
            .scheduler_context_observation(
                network,
                &crate::scheduling::SessionKey {
                    conversation_id: scope.into(),
                    scope: crate::scheduling::SessionScope::Main,
                },
            )
            .await?;
        let access = Access {
            network_id: network.into(),
            human: true,
            conversation_id: Some(scope.into()),
            manager: None,
        };
        let input = json!({"conversation_id":scope});
        let query_network = network.to_owned();
        let query_scope = scope.to_owned();
        self.read_display(&access, &input, "scheduler_owner_report", move |store| {
            let conversation = store
                .conversation(&query_network, &query_scope)?
                .context("conversation not found")?;
            ensure!(
                conversation.owner.as_deref() == Some(local.as_str()),
                "conversation owner changed; refresh the conversation"
            );
            let jobs = projection.history_page(
                store,
                &query_network,
                &query_scope,
                HistorySection::Jobs,
                None,
                50,
                &manager,
            )?;
            let scopes = projection.history_page(
                store,
                &query_network,
                &query_scope,
                HistorySection::ScopeJobs,
                None,
                50,
                &manager,
            )?;
            let conflicts =
                store.conversation_memory_conflicts_page(&query_network, &query_scope, None, 50)?;
            let mut value = json!({"jobs":jobs.items,"scope_jobs":scopes.items,
                "manager":manager,"context":context,
                "memory_staged_count":store.conversation_memory_staged_count(&query_scope)?,
                "memory_conflicts":conflicts.items,"history":{}});
            let sections = [
                (
                    "owner_jobs",
                    "jobs",
                    HistorySection::OwnerJobs,
                    HistorySection::Jobs,
                    jobs.next_cursor,
                ),
                (
                    "owner_scope_jobs",
                    "scope_jobs",
                    HistorySection::OwnerScopeJobs,
                    HistorySection::ScopeJobs,
                    scopes.next_cursor,
                ),
                (
                    "owner_memory_conflicts",
                    "memory_conflicts",
                    HistorySection::OwnerMemoryConflicts,
                    HistorySection::MemoryConflicts,
                    conflicts.next_cursor,
                ),
            ];
            let mut starts = Vec::new();
            for (name, field, owner_section, local_section, next) in sections {
                let next = next
                    .map(|next| {
                        owner_cursor(
                            &query_network,
                            &query_scope,
                            &local,
                            conversation.owner_epoch,
                            owner_section,
                            Some(&next),
                        )
                    })
                    .transpose()?;
                value["history"][name] = json!({"complete":next.is_none(),"next_cursor":next});
                let start = store.conversation_history_start_cursor(
                    &query_network,
                    &query_scope,
                    local_section,
                )?;
                let start = owner_cursor(
                    &query_network,
                    &query_scope,
                    &local,
                    conversation.owner_epoch,
                    owner_section,
                    Some(&start),
                )?;
                value["history_starts"][name] = json!(start);
                starts.push((name, field, start));
            }
            // A large single record may consume a whole page. Defer complete sections to
            // their start cursor rather than cutting content or hiding omitted records.
            starts.sort_by_key(|(_, field, _)| {
                std::cmp::Reverse(
                    serde_json::to_vec(&value[*field]).map_or(usize::MAX, |bytes| bytes.len()),
                )
            });
            for (name, field, start) in starts {
                if serde_json::to_vec(&value)?.len() < HISTORY_RESPONSE_BYTES {
                    break;
                }
                value[field] = json!([]);
                value["history"][name] = json!({"complete":false,"next_cursor":start});
            }
            ensure!(
                serde_json::to_vec(&value)?.len() < HISTORY_RESPONSE_BYTES,
                "owner metadata exceeds the response byte limit"
            );
            Ok(value)
        })
        .await
    }

    /// Reads only this owner's local observations. The authenticated peer dispatcher calls
    /// this directly; it never contacts another owner and cannot recursively forward reads.
    pub(super) async fn owner_history(&self, network: &str, input: &Value) -> Result<Value> {
        let mut parameters = input.clone();
        let fields = parameters
            .as_object_mut()
            .context("owner history request must be an object")?;
        fields.remove("type");
        fields.remove("method");
        let request: HistoryRequest = serde_json::from_value(parameters)?;
        let section = match request.section {
            HistorySection::OwnerJobs => HistorySection::Jobs,
            HistorySection::OwnerScopeJobs => HistorySection::ScopeJobs,
            HistorySection::OwnerMemoryConflicts => HistorySection::MemoryConflicts,
            _ => bail!("not an owner history section"),
        };
        let local = self.identity.device_id()?;
        let cursor = request
            .cursor
            .as_deref()
            .map(|encoded| -> Result<OwnerCursor> {
                ensure!(encoded.len() <= 16384, "owner history cursor is too large");
                let cursor: OwnerCursor = serde_json::from_slice(
                    &URL_SAFE_NO_PAD
                        .decode(encoded)
                        .context("invalid owner history cursor")?,
                )?;
                ensure!(
                    cursor.network == network
                        && cursor.conversation == request.conversation_id
                        && cursor.owner == local
                        && cursor.section == request.section,
                    "conversation owner changed; refresh the conversation"
                );
                Ok(cursor)
            })
            .transpose()?;
        let projection = self.scheduler_projection()?;
        let manager = self.scheduler_manager_metadata();
        let query_network = network.to_owned();
        let query_local = local.clone();
        // Peer authentication is established by the transport. Reuse the display fence so
        // membership, conversation existence, ownership and page all share one read.
        let access = Access {
            network_id: network.into(),
            human: true,
            conversation_id: Some(request.conversation_id.clone()),
            manager: None,
        };
        let requested_section = request.section;
        self.read_display(&access, input, "owner_history", move |store| {
            let conversation = store
                .conversation(&query_network, &request.conversation_id)?
                .context("conversation not found")?;
            ensure!(
                conversation.owner.as_deref() == Some(query_local.as_str())
                    && cursor
                        .as_ref()
                        .is_none_or(|cursor| cursor.owner_epoch == conversation.owner_epoch),
                "conversation owner changed; refresh the conversation"
            );
            let cursor = cursor.as_ref().and_then(|cursor| cursor.cursor.as_deref());
            let mut page = if section == HistorySection::MemoryConflicts {
                serde_json::to_value(store.conversation_memory_conflicts_page(
                    &query_network,
                    &request.conversation_id,
                    cursor,
                    request.limit,
                )?)?
            } else {
                serde_json::to_value(projection.history_page(
                    store,
                    &query_network,
                    &request.conversation_id,
                    section,
                    cursor,
                    request.limit,
                    &manager,
                )?)?
            };
            page["section"] = json!(requested_section);
            if let Some(next) = page["next_cursor"].as_str() {
                page["next_cursor"] = json!(owner_cursor(
                    &query_network,
                    &request.conversation_id,
                    &query_local,
                    conversation.owner_epoch,
                    requested_section,
                    Some(next)
                )?);
            }
            ensure!(
                serde_json::to_vec(&page)?.len() < HISTORY_RESPONSE_BYTES,
                "owner history record exceeds the response byte limit"
            );
            Ok(page)
        })
        .await
    }

    pub(super) async fn conversation_owner_history(
        &self,
        access: &Access,
        input: &Value,
    ) -> Result<Value> {
        let network = access.network_id.clone();
        let scope = field(input, "conversation_id")?.to_owned();
        let query_network = network.clone();
        let query_scope = scope.clone();
        let (owner, owner_epoch, projection_authority) = self
            .read_display(
                access,
                input,
                "conversation_owner_history.authorize",
                move |store| {
                    let conversation = store
                        .conversation(&query_network, &query_scope)?
                        .context("conversation not found")?;
                    Ok((
                        conversation.owner.context("conversation has no owner")?,
                        conversation.owner_epoch,
                        store.conversation_history_authority(&query_network, &query_scope)?,
                    ))
                },
            )
            .await?;
        if owner == self.identity.device_id()? {
            return self.owner_history(&network, input).await;
        }
        let mut request = input.clone();
        let fields = request
            .as_object_mut()
            .context("owner history request must be an object")?;
        fields.remove("method");
        fields.insert("type".into(), json!("scheduler_history"));
        let mut page = tokio::time::timeout(
            Duration::from_secs(3),
            self.peer_request(&network, &owner, request),
        )
        .await
        .context("conversation owner history timed out")??;
        ensure!(
            serde_json::to_vec(&page)?.len() < HISTORY_RESPONSE_BYTES,
            "owner history exceeds the response byte limit"
        );
        // Ownership or membership may have changed while the peer was answering.
        let query_network = network;
        self.read_display(
            access,
            input,
            "conversation_owner_history.recheck",
            move |store| {
                let conversation = store
                    .conversation(&query_network, &scope)?
                    .context("conversation not found")?;
                let authority = store.conversation_history_authority(&query_network, &scope)?;
                ensure!(
                    conversation.owner.as_deref() == Some(owner.as_str())
                        && conversation.owner_epoch == owner_epoch
                        && authority == projection_authority,
                    "conversation owner changed; refresh the conversation"
                );
                // Clients compare pages with their local snapshot authority. The inner cursor
                // remains bound to the owner's authority, which may have unsynced fork evidence.
                page["history_authority"] = json!(authority);
                Ok(page)
            },
        )
        .await
    }
}
