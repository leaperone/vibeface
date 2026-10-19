use super::*;

const AGENT_BLOB_BYTES: usize = 2 * MAX_BLOB_BYTES;
const AGENT_BLOB_OBJECTS: usize = 4;
const AGENT_BLOB_IDLE_TTL: Duration = Duration::from_secs(300);

#[derive(Clone, PartialEq, Eq, Hash)]
pub(super) struct AgentBlobKey {
    attempt_id: Option<String>,
    network_id: String,
    scope_id: String,
    hash: String,
    expected_size: u64,
}

pub(super) struct AgentBlobSnapshot {
    bytes: Vec<u8>,
    last_used: Instant,
}

fn task_message_id(scope: &str, attempt: &str, reference: &str) -> Result<String> {
    Ok(format!(
        "agent-task-{}",
        crypto::digest(&serde_json::to_vec(&(scope, attempt, reference))?)
    ))
}

impl Runtime {
    pub(super) async fn agent_tool(
        self: &Arc<Self>,
        access: Access,
        name: &str,
        args: Value,
        call_id: &str,
    ) -> Result<Value> {
        let scope = access
            .conversation_id
            .as_deref()
            .context("missing tool scope")?;
        ensure!(
            matches!(
                name,
                "nodes"
                    | "context"
                    | "send_task"
                    | "task_status"
                    | "call_lookup"
                    | "cancel_task"
                    | "observe_session"
            ),
            "unknown or unavailable coordination tool"
        );
        let write = matches!(name, "send_task" | "cancel_task" | "observe_session");
        ensure!(
            !write || access.manager.is_some(),
            "task changes require a live agent invocation"
        );
        let lease = tokio::time::timeout(
            Duration::from_secs(20),
            self.manager_tool_lease(&access, write),
        )
        .await
        .context("agent tool admission timed out")??;
        match name {
            "observe_session" => self.observe_session(&access, &args, call_id).await,
            "nodes" => self.rpc_inner(access, json!({"method":"nodes"})).await,
            "context" => self.agent_context(&access, &args).await,
            "task_status" => self.store.lock().await.agent_task_status(
                &access.network_id,
                scope,
                field(&args, "task_ref")?,
            ),
            "call_lookup" => {
                let manager = access
                    .manager
                    .as_ref()
                    .context("call lookup requires an agent invocation")?;
                let reference = field(&args, "call_ref")?;
                let store = self.store.lock().await;
                let task = store
                    .agent_task_for_message(
                        &access.network_id,
                        scope,
                        &task_message_id(scope, &manager.attempt_id, reference)?,
                    )?
                    .context("task call reference was not found in this invocation")?;
                store.agent_task_status(&access.network_id, scope, &task.hash)
            }
            "send_task" => {
                let manager = access
                    .manager
                    .as_ref()
                    .context("tasks require a live agent invocation")?;
                let reference = args["call_ref"]
                    .as_str()
                    .map(String::from)
                    .unwrap_or_else(|| format!("call-{}", crypto::digest(call_id.as_bytes())));
                vibeface_protocol::validate_id(&reference).map_err(anyhow::Error::msg)?;
                let message = AgentMessage {
                    message_id: task_message_id(scope, &manager.attempt_id, &reference)?,
                    target_node: field(&args, "target_node")?.into(),
                    origin_trigger: manager.trigger.clone(),
                    in_reply_to: None,
                    kind: AgentMessageKind::Task,
                    text: field(&args, "text")?.into(),
                };
                let (event, receipt) = self.store.lock().await.post_agent_task(
                    &self.identity,
                    &access.network_id,
                    scope,
                    message,
                    &manager.attempt_id,
                    now_ms(),
                )?;
                self.publish_changed(&access.network_id, scope, &event.hash);
                self.spawn_scheduler(access.network_id.clone(), scope.into(), event.hash.clone())?;
                Ok(json!({"task_ref":event.hash,"call_ref":reference,"receipt":receipt}))
            }
            "cancel_task" => {
                let reference = field(&args, "task_ref")?;
                let status = self.store.lock().await.agent_task_status(
                    &access.network_id,
                    scope,
                    reference,
                )?;
                let source = self.identity.peer_id()?;
                ensure!(
                    status["source_node"] == source,
                    "only the source device can cancel this task"
                );
                let target = field(&status["task"], "target_node")?;
                drop(lease);
                if target == source {
                    self.cancel_agent_task(&access.network_id, scope, reference, &source)
                        .await
                } else {
                    self.peer_request(
                        &access.network_id,
                        target,
                        json!({"type":"cancel_agent_task",
                        "conversation_id":scope,"task_ref":reference}),
                    )
                    .await
                }
            }
            _ => unreachable!(),
        }
    }

    pub(super) async fn cancel_agent_task(
        self: &Arc<Self>,
        network: &str,
        scope: &str,
        reference: &str,
        source: &str,
    ) -> Result<Value> {
        if self.store.lock().await.event(network, reference)?.is_none() {
            self.fetch_closure(network, scope, source, &[reference.into()])
                .await?;
        }
        let job = self
            .store
            .lock()
            .await
            .cancel_agent_task(&self.identity, network, scope, reference, source, now_ms())?
            .context("agent task was not found")?;
        if let crate::scheduling::JobState::Cancelling { attempt } = &job.state {
            self.interrupt_native_task(network, scope, reference)
                .await?;
            self.fence_manager_attempt(&attempt.id).await?;
        }
        self.publish_changed(network, scope, reference);
        Ok(json!({"task_ref":reference,"job":job}))
    }

    async fn agent_context(&self, access: &Access, args: &Value) -> Result<Value> {
        let scope = access
            .conversation_id
            .as_deref()
            .context("missing tool scope")?;
        let offset = args["offset"].as_u64().unwrap_or(0).min(usize::MAX as u64) as usize;
        let limit = args["limit"].as_u64().unwrap_or(20).clamp(1, 100) as usize;
        if let Some(hash) = args["blob_hash"].as_str() {
            return self.agent_blob_page(access, hash, offset, limit).await;
        }
        let context = self
            .store
            .lock()
            .await
            .context(&access.network_id, scope, &[])?;
        let events = context
            .events
            .into_iter()
            .filter(|event| event.scope_id == scope)
            .map(|event| {
                json!({
                    "author":event.author,"created_at_ms":event.created_at_ms,
                    "body":crate::scheduler::agent_event_body(&event.body),"blobs":event.blobs,
                })
            })
            .collect::<Vec<_>>();
        let end = offset.saturating_add(limit).min(events.len());
        ensure!(offset <= end, "invalid event offset");
        Ok(json!({"events":&events[offset..end],"next_offset":(end<events.len()).then_some(end)}))
    }

    async fn agent_blob_page(
        &self,
        access: &Access,
        hash: &str,
        offset: usize,
        limit_kib: usize,
    ) -> Result<Value> {
        let scope = access
            .conversation_id
            .as_deref()
            .context("missing tool scope")?;
        let attempt = access
            .manager
            .as_ref()
            .map(|manager| manager.attempt_id.clone());
        let scope_access = self.require_scope(access, scope).await;
        let mut snapshots = self.agent_blobs.lock().await;
        let now = Instant::now();
        snapshots.retain(|_, snapshot| {
            now.saturating_duration_since(snapshot.last_used) < AGENT_BLOB_IDLE_TTL
        });
        let result = async {
            scope_access?;
            let store = self.store.lock().await;
            let local = self.identity.peer_id()?;
            let current = store.public_state()?;
            ensure!(current.genesis.network_id == access.network_id, "wrong personal network");
            current.project()?.authorize_node(&local, None)?;
            store.authorize_node(&access.network_id, &local, None)?;
            ensure!(store.has_conversation(&access.network_id, scope)?, "conversation not found");
            Self::check_manager_permission(&store, access, false)?;
            let expected = store.context_blob_ref(&access.network_id, scope, hash)?;
            let size = usize::try_from(expected.size)?;
            ensure!(size <= MAX_BLOB_BYTES, "blob size limit exceeded");
            ensure!(offset <= size, "invalid blob offset");
            let key = AgentBlobKey {
                attempt_id: attempt.clone(),
                network_id: access.network_id.clone(),
                scope_id: scope.into(),
                hash: hash.into(),
                expected_size: expected.size,
            };
            if offset == 0 {
                drop(snapshots.remove(&key));
            }
            let count = snapshots.len();
            let used: usize = snapshots.values().map(|snapshot| snapshot.bytes.len()).sum();
            if let std::collections::hash_map::Entry::Vacant(entry) = snapshots.entry(key.clone()) {
                ensure!(
                    count < AGENT_BLOB_OBJECTS
                        && used.checked_add(size).is_some_and(|total| total <= AGENT_BLOB_BYTES),
                    Busy("Agent attachment snapshots are full; finish an existing read before retrying".into())
                );
                let bytes = store.get_blob_for_ref(&access.network_id, &expected)?
                    .context("blob not available")?;
                ensure!(bytes.len() == size, "blob size mismatch");
                entry.insert(AgentBlobSnapshot { bytes, last_used: now });
            }
            let snapshot = snapshots.get_mut(&key).unwrap();
            let end = offset.saturating_add(limit_kib.clamp(1, 100) * 1024).min(snapshot.bytes.len());
            let page = json!({"hash":hash,"offset":offset,"data_hex":hex::encode(&snapshot.bytes[offset..end]),"next_offset":(end<snapshot.bytes.len()).then_some(end),"size":snapshot.bytes.len()});
            snapshot.last_used = Instant::now();
            if end == snapshot.bytes.len() {
                drop(snapshots.remove(&key));
            }
            Ok(page)
        }.await;
        if result.is_err() {
            snapshots.retain(|key, _| {
                !(key.attempt_id == attempt
                    && key.network_id == access.network_id
                    && key.scope_id == scope
                    && key.hash == hash)
            });
        }
        result
    }

    pub(super) async fn clear_agent_blob_attempt(&self, attempt_id: &str) {
        self.agent_blobs
            .lock()
            .await
            .retain(|key, _| key.attempt_id.as_deref() != Some(attempt_id));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn attachment(
        node: &Runtime,
        access: &Access,
        bytes: &[u8],
        name: &str,
    ) -> Result<BlobRef> {
        let blob = BlobRef {
            hash: crypto::digest(bytes),
            size: bytes.len() as u64,
        };
        node.store
            .lock()
            .await
            .put_blob(&access.network_id, &blob, bytes)?;
        node.append_blobs(
            &access.network_id,
            access.conversation_id.as_deref().unwrap(),
            EventBody::MessagePosted {
                message_id: name.into(),
                role: "user".into(),
                text: name.into(),
                scheduler_node: None,
            },
            vec![blob.clone()],
        )
        .await?;
        Ok(blob)
    }

    async fn blob_fixture() -> Result<(
        PathBuf,
        Arc<Runtime>,
        Access,
        LocalIdentity,
        BlobRef,
        Vec<u8>,
    )> {
        let dir = crate::runtime::tests::fixture_under("network-reliability")?;
        let node = Arc::new(Runtime::open(&dir).await?);
        let owner = LocalIdentity::generate();
        let state = owner.sign_network_fixture(
            1,
            vec![node.identity.certificate(node.config.roles.clone())?],
        )?;
        let network = node.install_membership(state).await?.network_id;
        node.append(
            &network,
            "chat",
            EventBody::ConversationCreated {
                title: "blob pages".into(),
            },
        )
        .await?;
        let access = Access {
            network_id: network,
            human: false,
            conversation_id: Some("chat".into()),
            manager: None,
        };
        let bytes = (0..2500)
            .map(|index| (index % 251) as u8)
            .collect::<Vec<_>>();
        let blob = attachment(&node, &access, &bytes, "attachment").await?;
        Ok((dir, node, access, owner, blob, bytes))
    }

    async fn managed_blob_access(node: &Runtime, access: &Access, name: &str) -> Result<Access> {
        let target = node.identity.peer_id()?;
        let scope = access.conversation_id.as_deref().unwrap();
        let (trigger, attempt) = {
            let mut store = node.store.lock().await;
            let (message, _) = store.accept_message(
                &node.identity,
                &access.network_id,
                scope,
                EventBody::MessagePosted {
                    message_id: name.into(),
                    role: "user".into(),
                    text: name.into(),
                    scheduler_node: Some(target.clone()),
                },
                vec![],
                now_ms(),
                &target,
                &target,
                false,
            )?;
            let claimed = store
                .claim_next(
                    &access.network_id,
                    scope,
                    &target,
                    "blob-page-test",
                    "test-sdk",
                )?
                .context("management job was not claimed")?;
            let attempt = claimed
                .job
                .state
                .attempt()
                .context("manager attempt missing")?
                .id
                .clone();
            store.mark_started(&access.network_id, scope, &target, &message.hash, &attempt)?;
            (message.hash, attempt)
        };
        node.register_manager_attempt(name.into(), access.clone(), &attempt, &trigger, false)
            .await?;
        node.agent_tokens
            .lock()
            .await
            .get(name)
            .cloned()
            .context("management token missing")
    }

    #[tokio::test]
    async fn agent_blob_pages_keep_verified_bytes_until_refresh_and_reclaim_on_last_page()
    -> Result<()> {
        let (dir, node, access, _, blob, bytes) = blob_fixture().await?;
        let first = node
            .agent_tool(
                access.clone(),
                "context",
                json!({"blob_hash":blob.hash,"offset":0,"limit":1}),
                "first-callback",
            )
            .await?;
        assert_eq!(first["hash"], blob.hash);
        assert_eq!(first["offset"], 0);
        assert_eq!(first["size"], bytes.len());
        assert_eq!(first["next_offset"], 1024);
        assert_eq!(
            hex::decode(first["data_hex"].as_str().unwrap())?,
            bytes[..1024]
        );
        let corrupt = vec![0xee_u8; bytes.len()];
        rusqlite::Connection::open(dir.join("node.sqlite3"))?.execute(
            "UPDATE blobs SET content=? WHERE network_id=? AND hash=?",
            rusqlite::params![corrupt, access.network_id, blob.hash],
        )?;
        assert!(
            node.store
                .lock()
                .await
                .get_blob(&access.network_id, &blob.hash)
                .is_err()
        );
        let second = node
            .agent_tool(
                access.clone(),
                "context",
                json!({"blob_hash":blob.hash,"offset":1024,"limit":1}),
                "another-callback",
            )
            .await?;
        assert_eq!(second["next_offset"], 2048);
        assert_eq!(
            hex::decode(second["data_hex"].as_str().unwrap())?,
            bytes[1024..2048]
        );
        assert!(
            node.agent_context(
                &access,
                &json!({"blob_hash":blob.hash,"offset":0,"limit":1})
            )
            .await
            .is_err()
        );
        assert!(node.agent_blobs.lock().await.is_empty());
        node.store
            .lock()
            .await
            .put_blob(&access.network_id, &blob, &bytes)?;
        let mut received = Vec::new();
        for offset in [0, 1024, 2048] {
            let page = node
                .agent_context(
                    &access,
                    &json!({"blob_hash":blob.hash,"offset":offset,"limit":1}),
                )
                .await?;
            assert_eq!(page["offset"], offset);
            assert_eq!(page["size"], bytes.len());
            assert_eq!(
                page["next_offset"],
                if offset == 2048 {
                    Value::Null
                } else {
                    json!(offset + 1024)
                }
            );
            received.extend(hex::decode(page["data_hex"].as_str().unwrap())?);
        }
        assert_eq!(received, bytes);
        assert!(node.agent_blobs.lock().await.is_empty());
        drop(node);
        std::fs::remove_dir_all(dir)?;
        Ok(())
    }

    #[tokio::test]
    async fn agent_blob_snapshots_isolate_scope_attempt_and_drain_on_finish_or_fence() -> Result<()>
    {
        let (dir, node, access, _, blob, bytes) = blob_fixture().await?;
        let first = managed_blob_access(&node, &access, "first-manager").await?;
        node.agent_tool(
            first.clone(),
            "context",
            json!({"blob_hash":blob.hash,"limit":1}),
            "page",
        )
        .await?;
        node.append(
            &access.network_id,
            "other",
            EventBody::ConversationCreated {
                title: "other".into(),
            },
        )
        .await?;
        let mut other = access.clone();
        other.conversation_id = Some("other".into());
        assert!(
            node.agent_context(&other, &json!({"blob_hash":blob.hash,"limit":1}))
                .await
                .is_err()
        );
        assert_eq!(node.agent_blobs.lock().await.len(), 1);
        attachment(&node, &other, &bytes, "other-attachment").await?;
        let second = managed_blob_access(&node, &other, "second-manager").await?;
        node.agent_tool(
            second.clone(),
            "context",
            json!({"blob_hash":blob.hash,"limit":1}),
            "page",
        )
        .await?;
        assert_eq!(node.agent_blobs.lock().await.len(), 2);
        let mut wrong_attempt = first.clone();
        wrong_attempt.manager.as_mut().unwrap().attempt_id =
            second.manager.as_ref().unwrap().attempt_id.clone();
        assert!(
            node.agent_context(
                &wrong_attempt,
                &json!({"blob_hash":blob.hash,"offset":1024,"limit":1})
            )
            .await
            .is_err()
        );
        assert_eq!(node.agent_blobs.lock().await.len(), 2);
        node.finish_manager_attempt(&first.manager.as_ref().unwrap().attempt_id)
            .await;
        assert_eq!(node.agent_blobs.lock().await.len(), 1);
        assert!(
            node.agent_tool(
                first,
                "context",
                json!({"blob_hash":blob.hash,"offset":1024,"limit":1}),
                "finished"
            )
            .await
            .is_err()
        );
        let page = node
            .agent_tool(
                second.clone(),
                "context",
                json!({"blob_hash":blob.hash,"offset":1024,"limit":1}),
                "continued",
            )
            .await?;
        assert_eq!(
            hex::decode(page["data_hex"].as_str().unwrap())?,
            bytes[1024..2048]
        );
        node.fence_manager_attempt(&second.manager.as_ref().unwrap().attempt_id)
            .await?;
        assert!(node.agent_blobs.lock().await.is_empty());
        assert!(
            node.agent_tool(
                second.clone(),
                "context",
                json!({"blob_hash":blob.hash,"offset":2048,"limit":1}),
                "fenced"
            )
            .await
            .is_err()
        );
        node.finish_manager_attempt(&second.manager.as_ref().unwrap().attempt_id)
            .await;
        drop(node);
        std::fs::remove_dir_all(dir)?;
        Ok(())
    }

    #[tokio::test]
    async fn agent_blob_snapshot_admission_reclaims_idle_and_invalid_reads() -> Result<()> {
        let (dir, node, access, _, blob, bytes) = blob_fixture().await?;
        let mut blobs = vec![blob];
        for index in 1..=AGENT_BLOB_OBJECTS {
            let mut content = bytes.clone();
            content[0] = index as u8;
            blobs.push(attachment(&node, &access, &content, &format!("attachment-{index}")).await?);
        }
        for blob in &blobs[..AGENT_BLOB_OBJECTS] {
            node.agent_context(&access, &json!({"blob_hash":blob.hash,"limit":1}))
                .await?;
        }
        assert_eq!(node.agent_blobs.lock().await.len(), AGENT_BLOB_OBJECTS);
        let next = &blobs[AGENT_BLOB_OBJECTS];
        let error = node
            .agent_context(&access, &json!({"blob_hash":next.hash,"limit":1}))
            .await
            .unwrap_err();
        assert!(error.downcast_ref::<Busy>().is_some());
        assert_eq!(node.agent_blobs.lock().await.len(), AGENT_BLOB_OBJECTS);
        for snapshot in node.agent_blobs.lock().await.values_mut() {
            snapshot.last_used = Instant::now() - AGENT_BLOB_IDLE_TTL;
        }
        node.agent_context(&access, &json!({"blob_hash":next.hash,"limit":1}))
            .await?;
        assert_eq!(node.agent_blobs.lock().await.len(), 1);
        assert!(
            node.agent_context(
                &access,
                &json!({"blob_hash":next.hash,"offset":bytes.len()+1,"limit":1})
            )
            .await
            .is_err()
        );
        assert!(node.agent_blobs.lock().await.is_empty());
        drop(node);
        std::fs::remove_dir_all(dir)?;
        Ok(())
    }

    #[tokio::test]
    async fn agent_blob_snapshot_byte_budget_releases_the_final_page() -> Result<()> {
        let (dir, node, access, _, small, _) = blob_fixture().await?;
        let mut blobs = Vec::new();
        for marker in [0xa1, 0xa2] {
            let bytes = vec![marker; MAX_BLOB_BYTES];
            let blob = attachment(&node, &access, &bytes, &format!("large-{marker}")).await?;
            let page = node
                .agent_context(&access, &json!({"blob_hash":blob.hash,"limit":1}))
                .await?;
            assert_eq!(page["size"], MAX_BLOB_BYTES);
            blobs.push(blob);
        }
        let used: usize = node
            .agent_blobs
            .lock()
            .await
            .values()
            .map(|snapshot| snapshot.bytes.len())
            .sum();
        assert_eq!(used, AGENT_BLOB_BYTES);
        let error = node
            .agent_context(&access, &json!({"blob_hash":small.hash,"limit":1}))
            .await
            .unwrap_err();
        assert!(error.downcast_ref::<Busy>().is_some());
        let last = node
            .agent_context(
                &access,
                &json!({"blob_hash":blobs[0].hash,"offset":MAX_BLOB_BYTES-1024,"limit":1}),
            )
            .await?;
        assert_eq!(last["next_offset"], Value::Null);
        assert_eq!(
            hex::decode(last["data_hex"].as_str().unwrap())?,
            vec![0xa1; 1024]
        );
        node.agent_context(&access, &json!({"blob_hash":small.hash,"limit":1}))
            .await?;
        let snapshots = node.agent_blobs.lock().await;
        assert_eq!(snapshots.len(), 2);
        assert_eq!(
            snapshots
                .values()
                .map(|snapshot| snapshot.bytes.len())
                .sum::<usize>(),
            MAX_BLOB_BYTES + small.size as usize
        );
        drop(snapshots);
        drop(node);
        std::fs::remove_dir_all(dir)?;
        Ok(())
    }

    #[tokio::test]
    async fn agent_blob_pages_recheck_membership_after_waiting_for_cache() -> Result<()> {
        let (dir, node, access, owner, blob, _) = blob_fixture().await?;
        node.agent_context(&access, &json!({"blob_hash":blob.hash,"limit":1}))
            .await?;
        let snapshots = node.agent_blobs.lock().await;
        let args = json!({"blob_hash":blob.hash,"offset":1024,"limit":1});
        let mut request = Box::pin(node.agent_context(&access, &args));
        std::future::poll_fn(|cx| {
            assert!(std::future::Future::poll(request.as_mut(), cx).is_pending());
            std::task::Poll::Ready(())
        })
        .await;
        let mut certificate = node.identity.certificate(node.config.roles.clone())?;
        certificate.revoked = true;
        node.install_membership(owner.sign_network_fixture(2, vec![certificate])?)
            .await?;
        drop(snapshots);
        assert!(request.await.is_err());
        assert!(node.agent_blobs.lock().await.is_empty());
        drop(node);
        std::fs::remove_dir_all(dir)?;
        Ok(())
    }
}
