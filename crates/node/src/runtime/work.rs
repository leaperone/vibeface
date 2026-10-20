//! Drives the work state machine from executor results and events. Every change goes through
//! `Store::apply_work_signal`, so the journal, the DAG and the UI agree.
use super::executor::{Executor as _, request_text, validate_response};
use super::*;
use crate::scheduling::task_trigger;
use crate::work::{
    Dispatch, DispatchState, Probe, Signal, StopRequest, WorkRecord, WorkSpec, WorkState,
};

const INTERRUPT_TIMEOUT: Duration = Duration::from_secs(30);
/// How long a stop request for an unreachable device keeps retrying before it shows as unconfirmed.
const STOP_DELIVERY_TIMEOUT: Duration = Duration::from_secs(30);
/// The user waits this long for the first stop attempt; a stop is idempotent, so retries are safe.
const STOP_ATTEMPT_TIMEOUT: Duration = Duration::from_secs(5);

fn live(record: &WorkRecord) -> bool {
    !matches!(
        record.state,
        WorkState::Finished { .. } | WorkState::Unverified { probe: false, .. }
    )
}

/// The first changed path outside `cwd`; relative paths start at `cwd` and `..` is resolved by name.
fn escaped(changes: &Value, cwd: &str) -> Option<String> {
    let inside = |path: &str| {
        let mut resolved = std::path::PathBuf::new();
        for part in Path::new(cwd).join(path).components() {
            match part {
                std::path::Component::ParentDir => {
                    resolved.pop();
                }
                std::path::Component::CurDir => {}
                part => resolved.push(part),
            }
        }
        resolved.starts_with(cwd)
    };
    changes
        .as_array()?
        .iter()
        .flat_map(|change| {
            [
                change["path"].as_str(),
                change["kind"]["move_path"].as_str(),
            ]
        })
        .flatten()
        .find(|path| !inside(path))
        .map(str::to_owned)
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

    /// Every dispatch, local or from another Node, passes the same checks: the trigger chain
    /// carries the user's signed message or their permission to continue, the directory is in
    /// this Node's roots, and an agent task has an Agent to run it.
    async fn authorize_dispatch(&self, network: &str, spec: &WorkSpec) -> Result<()> {
        self.allowed_path(&spec.cwd)?;
        let store = self.store.lock().await;
        let conversation = store
            .conversation(network, &spec.conversation)?
            .context("conversation not found")?;
        let from_owner = conversation.owner.as_deref() == Some(spec.dispatcher.as_str());
        match spec.trigger.split_once(':') {
            Some(("message", message_id)) => {
                ensure!(
                    from_owner,
                    "only the conversation's scheduling device dispatches its work"
                );
                let message = store
                    .message(network, &spec.conversation, message_id)?
                    .context("the user's message has not reached this device")?;
                ensure!(
                    message.role == Role::User,
                    "work must follow the user's own message"
                );
            }
            Some(("work", previous)) => {
                ensure!(
                    from_owner,
                    "only the conversation's scheduling device dispatches its work"
                );
                ensure!(
                    store.autonomy(network, &spec.conversation)?.mode == AutonomyMode::Continue,
                    "continuing after a result needs the user's permission in this conversation"
                );
                ensure!(
                    store
                        .works(network, &spec.conversation)?
                        .iter()
                        .any(|work| work.work_id == previous && work.outcome.is_some()),
                    "the result this work follows has not reached this device"
                );
            }
            Some(("task", task)) => {
                let task = store
                    .work_record(network, task)?
                    .context("agent task not found on this device")?;
                ensure!(
                    spec.dispatcher == spec.target
                        && spec.executor == Executor::Codex
                        && task.spec.executor == Executor::Agent
                        && task.spec.conversation == spec.conversation
                        && spec.parent.as_deref() == Some(task.spec.work_id.as_str())
                        && !task.state.finished(),
                    "an agent task only starts Codex work on its own device"
                );
            }
            _ => bail!("invalid work trigger"),
        }
        if let Some(original) = &spec.continues {
            let original = store
                .work_record(network, original)?
                .context("the work to continue is not on this device")?;
            ensure!(
                original.spec.conversation == spec.conversation
                    && original.spec.executor == spec.executor,
                "a continuation keeps the original conversation and executor"
            );
        }
        if spec.executor == Executor::Agent {
            ensure!(
                spec.dispatcher != spec.target,
                "an agent task goes to another device"
            );
            ensure!(
                self.config.roles.contains(&NodeRole::Scheduler)
                    && self.settings_snapshot().manager.is_some(),
                "this device has no assistant to take the task"
            );
        }
        Ok(())
    }

    /// Records the dispatch and starts it once. A retry of the same `work_id` returns the
    /// existing record and never starts a second turn.
    pub(super) async fn accept_dispatch(
        self: &Arc<Self>,
        network: &str,
        spec: WorkSpec,
    ) -> Result<WorkRecord> {
        if self
            .store
            .lock()
            .await
            .work_record(network, &spec.work_id)?
            .is_none()
        {
            self.authorize_dispatch(network, &spec).await?;
        }
        let (record, created) =
            self.store
                .lock()
                .await
                .accept_work(&self.identity, network, spec, now_ms())?;
        self.publish_changed(network, &record.spec.conversation, &record.spec.work_id);
        if created {
            match record.spec.executor {
                Executor::Codex => {
                    let node = self.clone();
                    let (network, work_id) = (network.to_owned(), record.spec.work_id.clone());
                    tokio::spawn(async move {
                        if let Err(error) = node.start_work(&network, &work_id).await {
                            tracing::warn!(%error, work_id, "work start failed");
                        }
                    });
                }
                Executor::Agent => self.spawn_scheduler(network, &record.spec.conversation)?,
            }
        }
        Ok(record)
    }

    /// Another Node's dispatch. The dispatcher may be ahead of this Node, so a refusal is retried
    /// once after syncing from it; a second refusal is final and the dispatcher reports it.
    pub(super) async fn accept_remote_dispatch(
        self: &Arc<Self>,
        network: &str,
        origin: &str,
        input: &Value,
    ) -> Result<Value> {
        let spec: WorkSpec = serde_json::from_value(input["spec"].clone())?;
        ensure!(
            spec.dispatcher == origin && spec.target == self.identity.device_id()?,
            "a dispatch names its sender and this device"
        );
        ensure!(
            spec.work_id == WorkSpec::id(&spec.conversation, &spec.trigger, &spec.call_ref),
            "work ID does not match its call"
        );
        let first = self.accept_dispatch(network, spec.clone()).await;
        let record = match first {
            Ok(record) => Ok(record),
            Err(_) => {
                self.sync_peer(network, origin).await?;
                self.accept_dispatch(network, spec).await
            }
        };
        Ok(match record {
            Ok(record) => {
                json!({"work_id":record.spec.work_id,"state":record.state.name()})
            }
            Err(error) => json!({"rejected":error.to_string()}),
        })
    }

    /// A dispatch to another Node waits in this Node's journal until the target answers; an
    /// unknown answer is retried with the same `work_id`, which the target accepts at most once.
    pub(super) async fn send_dispatch(
        self: &Arc<Self>,
        network: &str,
        spec: WorkSpec,
    ) -> Result<Dispatch> {
        let dispatch = self.store.lock().await.save_dispatch(
            network,
            &Dispatch {
                spec,
                state: DispatchState::Pending,
            },
        )?;
        if dispatch.state != DispatchState::Pending {
            return Ok(dispatch);
        }
        let request = json!({"type":"dispatch_work","spec":dispatch.spec});
        let state = match self
            .peer_request(network, &dispatch.spec.target, request)
            .await
        {
            Ok(reply) if reply["work_id"] == dispatch.spec.work_id.as_str() => {
                DispatchState::Accepted
            }
            Ok(reply) => DispatchState::Rejected {
                reason: reply["rejected"]
                    .as_str()
                    .unwrap_or("the device refused the work")
                    .into(),
            },
            Err(error) => {
                tracing::debug!(%error, "dispatch waits for its target");
                return Ok(dispatch);
            }
        };
        let dispatch = Dispatch { state, ..dispatch };
        self.store.lock().await.save_dispatch(network, &dispatch)?;
        self.publish_changed(network, &dispatch.spec.conversation, &dispatch.spec.work_id);
        Ok(dispatch)
    }

    async fn start_work(self: &Arc<Self>, network: &str, work_id: &str) -> Result<()> {
        let record = self
            .store
            .lock()
            .await
            .work_record(network, work_id)?
            .context("work not found")?;
        if record.spec.executor != Executor::Codex {
            return Ok(());
        }
        let executor = &self.executor.executor;
        // A finished work keeps its thread only in the shared history; dispatch already checked
        // that the original ran on this device.
        let continued = match &record.spec.continues {
            Some(original) => self
                .store
                .lock()
                .await
                .works(network, &record.spec.conversation)?
                .into_iter()
                .find(|work| &work.work_id == original)
                .and_then(|work| work.thread_id),
            None => None,
        };
        let thread = match &record.state {
            // A continuation starts a new turn on the original thread.
            WorkState::Accepted if continued.is_some() => {
                let thread = continued.expect("checked");
                self.signal_work(network, work_id, Signal::ThreadCreated(thread.clone()))
                    .await?;
                thread
            }
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
        if record.spec.executor != Executor::Codex {
            return Ok(());
        }
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
        match &record.state {
            WorkState::Running { handle } | WorkState::WaitingApproval { handle, .. } => {
                self.executor
                    .executor
                    .interrupt(&handle.thread_id, &handle.turn_id)
                    .await
                    .map_err(|error| anyhow::anyhow!("interrupt was not confirmed: {error}"))?;
                self.signal_work(network, work_id, Signal::InterruptRequested)
                    .await?;
            }
            // An agent task stops with its coordination turn.
            WorkState::Accepted if record.spec.executor == Executor::Agent => {
                let job = self.store.lock().await.request_manager_cancel(
                    &self.identity,
                    network,
                    &record.spec.conversation,
                    &task_trigger(work_id),
                )?;
                if let Some(crate::scheduling::JobState::Cancelling { attempt }) =
                    job.map(|job| job.state)
                {
                    self.fence_manager_attempt(&attempt.id).await?;
                }
                self.publish_changed(network, &record.spec.conversation, work_id);
            }
            _ => {}
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

    /// Stops a work wherever it runs. A target that cannot be reached keeps the request here,
    /// retried until it confirms, the work ends or the request times out.
    pub(super) async fn stop_work(
        self: &Arc<Self>,
        network: &str,
        scope: &str,
        work_id: &str,
    ) -> Result<Value> {
        let work = self
            .store
            .lock()
            .await
            .works(network, scope)?
            .into_iter()
            .find(|work| work.work_id == work_id)
            .context("work not found in this conversation")?;
        if work.target_node == self.identity.device_id()? {
            return self.interrupt_work(network, work_id).await;
        }
        let request = StopRequest {
            work_id: work_id.into(),
            conversation: scope.into(),
            target: work.target_node,
            requested_at_ms: now_ms(),
            deadline_ms: now_ms() + STOP_DELIVERY_TIMEOUT.as_millis() as i64,
            timed_out: false,
        };
        match tokio::time::timeout(STOP_ATTEMPT_TIMEOUT, self.deliver_stop(network, &request)).await
        {
            Ok(Ok(reply)) => Ok(reply),
            failed => {
                tracing::debug!(?failed, "stop request waits for its device");
                self.store
                    .lock()
                    .await
                    .save_stop_request(network, &request)?;
                self.publish_changed(network, scope, work_id);
                Ok(
                    json!({"work_id":work_id,"interrupt_requested":true,"confirmed":false,"waiting_for_device":true}),
                )
            }
        }
    }

    async fn deliver_stop(&self, network: &str, request: &StopRequest) -> Result<Value> {
        self.peer_request(
            network,
            &request.target,
            json!({"type":"interrupt_work","conversation_id":request.conversation,
                "work_id":request.work_id,"human":true}),
        )
        .await
    }

    /// Retries waiting dispatches and stop requests; a stop request past its deadline stays
    /// listed as unconfirmed until the work ends.
    pub(super) async fn deliver_work_requests(self: &Arc<Self>, network: &str) -> Result<()> {
        let dispatches = self.store.lock().await.dispatches(network)?;
        for dispatch in dispatches {
            if dispatch.state == DispatchState::Pending {
                self.send_dispatch(network, dispatch.spec).await?;
            }
        }
        let requests = self.store.lock().await.stop_requests(network)?;
        for mut request in requests {
            let ended = self
                .store
                .lock()
                .await
                .works(network, &request.conversation)?
                .iter()
                .any(|work| work.work_id == request.work_id && work.outcome.is_some());
            if ended {
                self.store
                    .lock()
                    .await
                    .forget_stop_request(network, &request.work_id)?;
                continue;
            }
            if request.timed_out {
                continue;
            }
            // Bounded like the first attempt, so an offline device cannot hold up the outbox.
            let attempt =
                tokio::time::timeout(STOP_ATTEMPT_TIMEOUT, self.deliver_stop(network, &request));
            if matches!(attempt.await, Ok(Ok(_))) {
                self.store
                    .lock()
                    .await
                    .forget_stop_request(network, &request.work_id)?;
            } else if now_ms() >= request.deadline_ms {
                request.timed_out = true;
                self.store
                    .lock()
                    .await
                    .save_stop_request(network, &request)?;
            } else {
                continue;
            }
            self.publish_changed(network, &request.conversation, &request.work_id);
        }
        Ok(())
    }

    /// The user's answer; requests from an earlier executor epoch can no longer be answered.
    pub(super) async fn answer_approval(
        self: &Arc<Self>,
        network: &str,
        scope: &str,
        work_id: &str,
        request_id: &str,
        response: &Value,
        attestation: Attestation,
    ) -> Result<Value> {
        let local = self.store.lock().await.work_record(network, work_id)?;
        let Some(record) = local else {
            // The device that ran the work records the answer; this entry only carries it there.
            let work = self
                .store
                .lock()
                .await
                .works(network, scope)?
                .into_iter()
                .find(|work| work.work_id == work_id)
                .context("work not found in this conversation")?;
            return self
                .peer_request(
                    network,
                    &work.target_node,
                    json!({"type":"answer_approval","conversation_id":scope,"work_id":work_id,
                        "request_id":request_id,"response":response,"attestation":attestation}),
                )
                .await;
        };
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
        // Checked before Codex hears the answer: a refused signature must not leave an unrecorded answer.
        let recorded = EventBody::ApprovalAnswered {
            work_id: work_id.into(),
            request_id: request_id.into(),
            decision,
            attestation: Some(attestation.clone()),
        };
        crate::crypto::verify_attestation(network, scope, &recorded)?;
        self.store
            .lock()
            .await
            .authorize_attestation(network, &recorded)?;
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
                        attestation,
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

    /// Answer forms of this Node's open requests for a work, for an entry that shows them.
    pub(super) async fn approval_forms(&self, network: &str, work_id: &str) -> Result<Value> {
        let record = self.store.lock().await.work_record(network, work_id)?;
        let forms = match record.map(|record| record.state) {
            Some(WorkState::WaitingApproval { approvals, .. }) => approvals
                .iter()
                .map(|approval| {
                    json!({"request_id":approval.request_id,"answering":approval.answering,
                        "form":executor::request_form(&approval.method, &approval.params)})
                })
                .collect(),
            _ => vec![],
        };
        Ok(json!({ "approvals": forms }))
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
        let item = &params["item"];
        // Recorded only: writes outside the directory are allowed and never raise an alert.
        if method == "item/completed"
            && item["type"] == "fileChange"
            && item["status"] == "completed"
            && let Some(path) = escaped(&item["changes"], &record.spec.cwd)
        {
            tracing::info!(work_id, path, "work changed a file outside its directory");
        }
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

#[cfg(test)]
mod tests {
    use serde_json::json;

    #[test]
    fn changes_outside_the_work_directory_are_found() {
        let changes = |path: &str| json!([{"path":"src/a.rs","kind":{"type":"add"}},{"path":path,"kind":{"type":"add"}}]);
        assert_eq!(super::escaped(&changes("/p/b.rs"), "/p"), None);
        assert_eq!(super::escaped(&changes("sub/../c.rs"), "/p"), None);
        assert_eq!(
            super::escaped(&changes("../q/x"), "/p").as_deref(),
            Some("../q/x")
        );
        assert_eq!(
            super::escaped(&changes("/pq/x"), "/p").as_deref(),
            Some("/pq/x")
        );
        let moved = json!([{"path":"a","kind":{"type":"update","move_path":"/etc/x"}}]);
        assert_eq!(super::escaped(&moved, "/p").as_deref(), Some("/etc/x"));
    }
}
