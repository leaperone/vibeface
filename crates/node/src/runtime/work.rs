//! Drives the work state machine from executor results and events. Every change goes through
//! `Store::apply_work_signal`, so the journal, the DAG and the UI agree.
use super::executor::{Executor as _, request_text, validate_response};
use super::*;
use crate::work::{Probe, Signal, WorkRecord, WorkSpec, WorkState};

const INTERRUPT_TIMEOUT: Duration = Duration::from_secs(30);

fn live(record: &WorkRecord) -> bool {
    !matches!(
        record.state,
        WorkState::Finished { .. } | WorkState::Unverified { probe: false, .. }
    )
}

impl Runtime {
    fn executor_epoch(&self) -> u64 {
        self.executor_epoch
            .load(std::sync::atomic::Ordering::SeqCst)
    }

    pub(super) async fn signal_work(
        self: &Arc<Self>,
        network: &str,
        work_id: &str,
        signal: Signal,
    ) -> Result<WorkRecord> {
        let (record, receipt) = self.store.lock().await.apply_work_signal(
            &self.identity,
            network,
            work_id,
            signal,
            now_ms(),
        )?;
        self.publish_changed(network, &record.spec.conversation, work_id);
        if receipt.is_some() {
            self.spawn_scheduler(network, &record.spec.conversation)?;
        }
        if matches!(record.state, WorkState::Interrupting { .. }) {
            let node = self.clone();
            let (network, work_id) = (network.to_owned(), work_id.to_owned());
            tokio::spawn(async move {
                tokio::time::sleep(INTERRUPT_TIMEOUT).await;
                let result = node.store.lock().await.apply_work_signal(
                    &node.identity,
                    &network,
                    &work_id,
                    Signal::InterruptTimedOut,
                    now_ms(),
                );
                if let Ok((record, _)) = result {
                    node.publish_changed(&network, &record.spec.conversation, &work_id);
                }
            });
        }
        self.observe_live_threads(network).await;
        Ok(record)
    }

    /// Threads of live work stay subscribed across App Server reconnects.
    async fn observe_live_threads(&self, network: &str) {
        let threads = self
            .store
            .lock()
            .await
            .work_records(network)
            .unwrap_or_default()
            .into_iter()
            .filter(live)
            .filter_map(|record| match &record.state {
                WorkState::Starting {
                    thread: Some(thread),
                } => Some(thread.clone()),
                state => state.handle().map(|handle| handle.thread_id.clone()),
            })
            .collect();
        let _ = self.executor.executor.observe(threads);
    }

    /// Records the dispatch and starts the turn once. A retry of the same `work_id` returns the
    /// existing record and never starts a second turn.
    pub(super) async fn dispatch_work(
        self: &Arc<Self>,
        network: &str,
        spec: WorkSpec,
    ) -> Result<WorkRecord> {
        ensure!(
            spec.target == self.identity.device_id()?,
            "dispatching to another device arrives with cross-device delivery"
        );
        self.allowed_path(&spec.cwd)?;
        let (record, created) =
            self.store
                .lock()
                .await
                .accept_work(&self.identity, network, spec, now_ms())?;
        self.publish_changed(network, &record.spec.conversation, &record.spec.work_id);
        if created {
            let node = self.clone();
            let (network, work_id) = (network.to_owned(), record.spec.work_id.clone());
            tokio::spawn(async move {
                if let Err(error) = node.start_work(&network, &work_id).await {
                    tracing::warn!(%error, work_id, "work start failed");
                }
            });
        }
        Ok(record)
    }

    async fn start_work(self: &Arc<Self>, network: &str, work_id: &str) -> Result<()> {
        let record = self
            .store
            .lock()
            .await
            .work_record(network, work_id)?
            .context("work not found")?;
        let executor = &self.executor.executor;
        let thread = match &record.state {
            WorkState::Accepted => match executor.start_thread(&record.spec.cwd).await {
                Ok(thread) => {
                    self.signal_work(network, work_id, Signal::ThreadCreated(thread.clone()))
                        .await?;
                    thread
                }
                Err(error) if error.uncertain => {
                    self.signal_work(network, work_id, Signal::StartUnknown(error.message))
                        .await?;
                    return Ok(());
                }
                Err(error) => {
                    self.signal_work(network, work_id, Signal::StartRejected(error.message))
                        .await?;
                    return Ok(());
                }
            },
            WorkState::Starting {
                thread: Some(thread),
            } => thread.clone(),
            _ => return Ok(()),
        };
        let epoch = self.executor_epoch();
        let signal = match executor
            .start_turn(&thread, &record.spec.cwd, &record.spec.prompt)
            .await
        {
            Ok(turn_id) => Signal::TurnStarted { turn_id, epoch },
            Err(error) if error.uncertain => match executor.probe(&thread, None, epoch).await {
                Ok(probe) => Signal::Probed(probe),
                Err(_) => Signal::StartUnknown(error.message),
            },
            Err(error) => Signal::StartRejected(error.message),
        };
        let record = self.signal_work(network, work_id, signal).await?;
        // Requests and completions that arrived before the handle was journaled.
        if let Some(handle) = record.state.handle().cloned() {
            let snapshot = executor.snapshot().await;
            for request in snapshot["pending_requests"]
                .as_array()
                .into_iter()
                .flatten()
            {
                if request["params"]["threadId"] == handle.thread_id.as_str()
                    && request["params"]["turnId"] == handle.turn_id.as_str()
                {
                    self.route_turn_event(network, request.clone()).await?;
                }
            }
            if let Ok(probe @ Probe::Terminal { .. }) = executor
                .probe(&handle.thread_id, Some(&handle.turn_id), epoch)
                .await
            {
                self.signal_work(network, work_id, Signal::Probed(probe))
                    .await?;
            }
        }
        Ok(())
    }

    /// Reads the executor for one work; never starts or replays it.
    pub(super) async fn probe_work(self: &Arc<Self>, network: &str, work_id: &str) -> Result<()> {
        let record = self
            .store
            .lock()
            .await
            .work_record(network, work_id)?
            .context("work not found")?;
        let epoch = self.executor_epoch();
        let executor = &self.executor.executor;
        let probe = match &record.state {
            WorkState::Accepted => return self.start_work(network, work_id).await,
            WorkState::Starting {
                thread: Some(thread),
            } => executor.probe(thread, None, epoch).await,
            state => match state.handle() {
                Some(handle) => {
                    executor
                        .probe(&handle.thread_id, Some(&handle.turn_id), epoch)
                        .await
                }
                None => return Ok(()),
            },
        };
        match probe {
            Ok(probe) => {
                self.signal_work(network, work_id, Signal::Probed(probe))
                    .await?;
                Ok(())
            }
            Err(error) => bail!("executor probe failed: {error}"),
        }
    }

    pub(super) async fn interrupt_work(
        self: &Arc<Self>,
        network: &str,
        work_id: &str,
    ) -> Result<Value> {
        let record = self
            .store
            .lock()
            .await
            .work_record(network, work_id)?
            .context("work not found")?;
        if let WorkState::Running { handle } | WorkState::WaitingApproval { handle, .. } =
            &record.state
        {
            self.executor
                .executor
                .interrupt(&handle.thread_id, &handle.turn_id)
                .await
                .map_err(|error| anyhow::anyhow!("interrupt was not confirmed: {error}"))?;
            self.signal_work(network, work_id, Signal::InterruptRequested)
                .await?;
        }
        // Stopping a work stops what it delegated.
        let children: Vec<String> = self
            .store
            .lock()
            .await
            .work_records(network)?
            .into_iter()
            .filter(|child| child.spec.parent.as_deref() == Some(work_id) && live(child))
            .map(|child| child.spec.work_id)
            .collect();
        for child in children {
            Box::pin(self.interrupt_work(network, &child)).await?;
        }
        Ok(json!({"work_id":work_id,"interrupt_requested":true,"confirmed":false}))
    }

    /// The user's answer; requests from an earlier executor epoch can no longer be answered.
    pub(super) async fn answer_approval(
        self: &Arc<Self>,
        network: &str,
        scope: &str,
        work_id: &str,
        request_id: &str,
        response: &Value,
    ) -> Result<Value> {
        let record = self
            .store
            .lock()
            .await
            .work_record(network, work_id)?
            .context("work not found")?;
        ensure!(
            record.spec.conversation == scope,
            "work belongs to another conversation"
        );
        let WorkState::WaitingApproval { handle, approvals } = &record.state else {
            bail!("this work is not waiting for an answer");
        };
        let approval = approvals
            .iter()
            .find(|a| a.request_id == request_id)
            .context("request was already answered or expired")?;
        ensure!(
            handle.epoch == self.executor_epoch(),
            "the executor restarted; this request expired"
        );
        ensure!(!approval.answering, "an answer is already being sent");
        let decision = validate_response(&approval.method, &approval.params, response)?;
        let rpc_id = approval.rpc_id.clone();
        self.signal_work(
            network,
            work_id,
            Signal::ApprovalAnswering(request_id.into()),
        )
        .await?;
        match self
            .executor
            .executor
            .answer(rpc_id, response.clone())
            .await
        {
            Ok(()) => {
                self.signal_work(
                    network,
                    work_id,
                    Signal::ApprovalAnswered {
                        request_id: request_id.into(),
                        decision,
                    },
                )
                .await?;
                Ok(json!({"work_id":work_id,"request_id":request_id,"answered":true}))
            }
            Err(error) => {
                self.signal_work(
                    network,
                    work_id,
                    Signal::ApprovalAnswerFailed(request_id.into()),
                )
                .await?;
                bail!("answer was not delivered: {error}")
            }
        }
    }

    pub(super) async fn resolve_work(
        self: &Arc<Self>,
        network: &str,
        scope: &str,
        work_id: &str,
        evidence: &str,
    ) -> Result<()> {
        let record = self
            .store
            .lock()
            .await
            .work_record(network, work_id)?
            .context("work not found")?;
        ensure!(
            record.spec.conversation == scope,
            "work belongs to another conversation"
        );
        ensure!(
            matches!(record.state, WorkState::Unverified { .. }),
            "only unverified work needs confirmation"
        );
        self.signal_work(network, work_id, Signal::UserResolved(evidence.into()))
            .await?;
        Ok(())
    }

    /// Maps App Server notifications and requests onto the work that owns their thread and turn.
    pub(super) async fn route_executor_event(
        self: &Arc<Self>,
        network: &str,
        event: Value,
    ) -> Result<()> {
        let method = event["method"].as_str().unwrap_or_default();
        let params = &event["params"];
        let records = self.store.lock().await.work_records(network)?;
        match method {
            "bridge.codex_connected" => {
                let epoch = self.store.lock().await.next_executor_epoch()?;
                self.executor_epoch
                    .store(epoch, std::sync::atomic::Ordering::SeqCst);
                for record in records.into_iter().filter(live) {
                    if let Err(error) = self.probe_work(network, &record.spec.work_id).await {
                        tracing::debug!(%error, "work probe after reconnect failed");
                    }
                }
                return Ok(());
            }
            "bridge.codex_disconnected" => {
                for record in records
                    .into_iter()
                    .filter(|r| r.state.handle().is_some() && live(r))
                {
                    self.signal_work(network, &record.spec.work_id, Signal::Disconnected)
                        .await?;
                }
                return Ok(());
            }
            "serverRequest/resolved" => {
                let rpc = params["requestId"].clone();
                for record in records {
                    if let WorkState::WaitingApproval { approvals, .. } = &record.state
                        && approvals.iter().any(|a| a.rpc_id == rpc)
                    {
                        self.signal_work(
                            network,
                            &record.spec.work_id,
                            Signal::ApprovalResolved(rpc.clone()),
                        )
                        .await?;
                    }
                }
                return Ok(());
            }
            _ => {}
        }
        drop(records);
        self.route_turn_event(network, event).await
    }

    async fn route_turn_event(self: &Arc<Self>, network: &str, event: Value) -> Result<()> {
        let method = event["method"].as_str().unwrap_or_default();
        let params = &event["params"];
        let records = self.store.lock().await.work_records(network)?;
        let thread = params["threadId"].as_str();
        let turn = params["turnId"]
            .as_str()
            .or_else(|| params["turn"]["id"].as_str());
        let Some(record) = records.into_iter().find(|record| {
            record.state.handle().is_some_and(|h| {
                Some(h.thread_id.as_str()) == thread && Some(h.turn_id.as_str()) == turn
            })
        }) else {
            return Ok(());
        };
        let work_id = record.spec.work_id.clone();
        self.publish_progress(
            network,
            json!({"type":"native_progress","conversation_id":record.spec.conversation,"work_id":work_id,
                "target_node":self.identity.device_id()?,"sequence":self.progress_sequence.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1,
                "progress":observation::native_progress(&event)}),
        );
        let signal = match method {
            "item/completed"
                if params["item"]["type"] == "agentMessage"
                    && (params["item"]["phase"].is_null()
                        || params["item"]["phase"] == "final_answer") =>
            {
                Signal::Text(params["item"]["text"].as_str().unwrap_or_default().into())
            }
            "turn/completed" => Signal::TurnCompleted {
                turn_id: turn.unwrap_or_default().into(),
                status: params["turn"]["status"]
                    .as_str()
                    .unwrap_or("unknown")
                    .into(),
                text: String::new(),
                error: params["turn"]["error"]["message"]
                    .as_str()
                    .map(str::to_owned),
            },
            method
                if event.get("id").is_some()
                    && (method.ends_with("requestApproval")
                        || method == "item/tool/requestUserInput") =>
            {
                Signal::ApprovalRequested {
                    rpc_id: event["id"].clone(),
                    method: method.into(),
                    params: params.clone(),
                    kind: method.rsplit('/').nth(1).unwrap_or(method).into(),
                    summary: request_text(method, params),
                    epoch: self.executor_epoch(),
                }
            }
            _ => return Ok(()),
        };
        self.signal_work(network, &work_id, signal).await?;
        Ok(())
    }

    pub(super) fn start_executor(self: &Arc<Self>, mut events: mpsc::UnboundedReceiver<Value>) {
        self.executor.start();
        let node = self.clone();
        tokio::spawn(async move {
            while let Some(event) = events.recv().await {
                let networks = node.store.lock().await.networks().unwrap_or_default();
                for network in networks {
                    if let Err(error) = node.route_executor_event(&network, event.clone()).await {
                        tracing::warn!(%error, "could not apply an executor event");
                    }
                }
            }
        });
    }
}
