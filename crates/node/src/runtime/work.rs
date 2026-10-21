//! Drives the work state machine from App Server results and events. Every change goes through
//! `Store::apply_work_signal`, so the journal, the DAG and the UI agree.
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

/// A plan as one line per step, the current step marked, for the persisted progress.
fn plan_summary(params: &Value) -> String {
    params["plan"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|step| {
            let mark = match step["status"].as_str() {
                Some("completed") => "✓",
                Some("inProgress") => "→",
                _ => "·",
            };
            format!("{mark} {}", step["step"].as_str().unwrap_or_default())
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn live(record: &WorkRecord) -> bool {
    !matches!(
        record.state,
        WorkState::Finished { .. } | WorkState::Unverified { probe: false, .. }
    )
}

impl Runtime {
    fn codex_epoch(&self) -> u64 {
        self.codex_epoch.load(std::sync::atomic::Ordering::SeqCst)
    }

    pub(super) async fn signal_work(
        self: &Arc<Self>,
        network: &str,
        work_id: &str,
        signal: Signal,
    ) -> Result<WorkRecord> {
        let quiet = matches!(signal, Signal::Text(_) | Signal::Progress { .. });
        let (record, receipt) = self.store.lock().await.apply_work_signal(
            &self.identity,
            network,
            work_id,
            signal,
            now_ms(),
        )?;
        if !quiet {
            let thread_id = match &record.state {
                WorkState::Starting { thread } => thread.as_deref(),
                state => state.handle().map(|handle| handle.thread_id.as_str()),
            };
            let (conversation_id, state) = (record.conversation.as_str(), record.state.name());
            match &record.state {
                WorkState::Finished {
                    outcome: crate::model::WorkOutcome::Completed,
                } => tracing::info!(
                    event = "work.state",
                    conversation_id,
                    work_id,
                    thread_id,
                    state,
                    outcome = "completed",
                    "work finished"
                ),
                WorkState::Finished { outcome } => {
                    tracing::warn!(event = "work.state", conversation_id, work_id, thread_id, state, outcome = ?outcome, "work finished {outcome:?}")
                }
                WorkState::Unverified { evidence, .. } => tracing::warn!(
                    event = "work.state",
                    conversation_id,
                    work_id,
                    thread_id,
                    state,
                    evidence,
                    "work result unverified"
                ),
                _ => tracing::info!(
                    event = "work.state",
                    conversation_id,
                    work_id,
                    thread_id,
                    state,
                    "work {state}"
                ),
            }
        }
        self.publish_changed(network, &record.conversation, work_id);
        if receipt.is_some() {
            self.wake_scheduler()?;
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
                    node.publish_changed(&network, &record.conversation, &work_id);
                }
            });
        }
        self.observe_live_threads().await;
        Ok(record)
    }

    /// Threads of live work stay subscribed across App Server reconnects.
    async fn observe_live_threads(&self) {
        let threads = self
            .store
            .lock()
            .await
            .work_records()
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
        let _ = self.codex.observe(threads);
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
                    store
                        .works(network, &spec.conversation)?
                        .iter()
                        .any(|work| work.work_id == previous && work.outcome.is_some()),
                    "the result this work follows has not reached this device"
                );
            }
            Some(("routine", _)) => {
                ensure!(
                    from_owner,
                    "only the conversation's scheduling device dispatches its work"
                );
                let (routine_id, _) = crate::scheduling::parse_routine_trigger(&spec.trigger)
                    .context("invalid work trigger")?;
                ensure!(
                    store.routine(network, routine_id)?.is_some_and(|entry| {
                        entry.conversation_id == spec.conversation && entry.routine.enabled
                    }),
                    "the routine this work runs for is paused, deleted or has not reached this device"
                );
            }
            Some(("task", task)) => {
                let task = store
                    .work_record(task)?
                    .context("agent task not found on this device")?;
                ensure!(
                    spec.dispatcher == spec.target
                        && spec.kind == WorkKind::Codex
                        && task.kind == WorkKind::Task
                        && task.conversation == spec.conversation
                        && spec.parent.as_deref() == Some(task.work_id.as_str())
                        && !task.state.finished(),
                    "an agent task only starts Codex work on its own device"
                );
            }
            _ => bail!("invalid work trigger"),
        }
        if let Some(original) = &spec.continues {
            let original = store
                .work_record(original)?
                .context("the work to continue is not on this device")?;
            ensure!(
                original.conversation == spec.conversation && original.kind == spec.kind,
                "a continuation keeps the original conversation and kind"
            );
        }
        ensure!(
            spec.kind == WorkKind::Task || spec.dispatcher == spec.target,
            "another device takes work only as a task for its assistant"
        );
        if spec.kind == WorkKind::Task {
            ensure!(
                spec.dispatcher != spec.target,
                "an agent task goes to another device"
            );
            ensure!(
                self.settings_snapshot().manager.is_some(),
                "this device has no assistant to take the task"
            );
        }
        Ok(())
    }

    /// Records the dispatch. A Codex work is then started by the Agent that registered it; an
    /// agent task is queued for this device's assistant. A retry of the same `work_id` returns the
    /// existing record and `false`, so it never starts a second turn.
    pub(super) async fn accept_dispatch(
        self: &Arc<Self>,
        network: &str,
        spec: WorkSpec,
    ) -> Result<(WorkRecord, bool)> {
        if self
            .store
            .lock()
            .await
            .work_record(&spec.work_id)?
            .is_none()
        {
            self.authorize_dispatch(network, &spec).await?;
        }
        let (record, created) =
            self.store
                .lock()
                .await
                .accept_work(&self.identity, network, spec, now_ms())?;
        tracing::info!(
            event = "work.dispatched",
            conversation_id = record.conversation,
            work_id = record.work_id,
            kind = ?record.kind,
            created,
            "work dispatched to this device"
        );
        self.publish_changed(network, &record.conversation, &record.work_id);
        if created && record.kind == WorkKind::Task {
            self.wake_scheduler()?;
        }
        Ok((record, created))
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
            Ok((record, _)) => Ok(record),
            Err(_) => {
                self.sync_peer(network, origin).await?;
                self.accept_dispatch(network, spec)
                    .await
                    .map(|(record, _)| record)
            }
        };
        Ok(match record {
            Ok(record) => {
                json!({"work_id":record.work_id,"state":record.state.name()})
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
        let dispatch = self.store.lock().await.save_dispatch(&Dispatch {
            spec,
            state: DispatchState::Pending,
        })?;
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
        let spec = &dispatch.spec;
        match &state {
            DispatchState::Rejected { reason } => tracing::warn!(
                event = "work.dispatch_rejected",
                conversation_id = spec.conversation,
                work_id = spec.work_id,
                target = spec.target,
                reason,
                "another device refused the work"
            ),
            _ => tracing::info!(
                event = "work.dispatched",
                conversation_id = spec.conversation,
                work_id = spec.work_id,
                target = spec.target,
                "another device accepted the work"
            ),
        }
        let dispatch = Dispatch { state, ..dispatch };
        self.store.lock().await.save_dispatch(&dispatch)?;
        self.publish_changed(network, &dispatch.spec.conversation, &dispatch.spec.work_id);
        Ok(dispatch)
    }

    /// The Agent's report of its own send: the thread before `turn/start`, then the turn, a
    /// refusal, or an unknown result that the Node settles by reading the thread.
    pub(super) async fn bind_work(
        self: &Arc<Self>,
        network: &str,
        scope: &str,
        args: &Value,
    ) -> Result<Value> {
        let work_id = field(args, "work_id")?;
        let thread_id = args["thread_id"].as_str();
        if let Some(turn_id) = args["turn_id"].as_str() {
            tracing::info!(
                event = "work.turn_started",
                conversation_id = scope,
                work_id,
                turn_id,
                "Codex turn started"
            );
        } else if let Some(reason) = args["failed"].as_str() {
            tracing::warn!(
                event = "work.start_failed",
                conversation_id = scope,
                work_id,
                reason,
                "Codex did not start the work"
            );
        } else if let Some(reason) = args["unknown"].as_str() {
            tracing::warn!(
                event = "work.start_unknown",
                conversation_id = scope,
                work_id,
                reason,
                "unknown whether Codex started the work"
            );
        } else if thread_id.is_some() {
            tracing::info!(
                event = "work.thread_bound",
                conversation_id = scope,
                work_id,
                thread_id,
                "Codex thread bound to the work"
            );
        }
        let record = self
            .store
            .lock()
            .await
            .work_record(work_id)?
            .context("work not found on this device")?;
        ensure!(
            record.conversation == scope && record.kind == WorkKind::Codex,
            "work belongs to another conversation"
        );
        let mut record = record;
        if let Some(thread) = args["thread_id"].as_str() {
            vibeface_protocol::validate_id(thread).map_err(anyhow::Error::msg)?;
            if matches!(record.state, WorkState::Accepted) {
                ensure!(
                    now_ms() < record.send_by_ms,
                    "the send window for this work is over; it will not start"
                );
                record = self
                    .signal_work(network, work_id, Signal::ThreadCreated(thread.into()))
                    .await?;
            }
            ensure!(
                matches!(&record.state, WorkState::Starting { thread: Some(known) } if known == thread),
                "this work is already bound to another thread or past starting"
            );
        }
        if let Some(turn) = args["turn_id"].as_str() {
            let signal = Signal::TurnStarted {
                turn_id: turn.into(),
                epoch: self.codex_epoch(),
            };
            record = self.signal_work(network, work_id, signal).await?;
            // The turn may have ended before it was bound.
            if let Some(handle) = record.state.handle().cloned()
                && let Ok(probe @ Probe::Terminal { .. }) = self
                    .codex
                    .probe(
                        &handle.thread_id,
                        Some(&handle.turn_id),
                        work_id,
                        handle.epoch,
                    )
                    .await
            {
                record = self
                    .signal_work(network, work_id, Signal::Probed(probe))
                    .await?;
            }
        } else if let Some(reason) = args["failed"].as_str() {
            record = self
                .signal_work(
                    network,
                    work_id,
                    Signal::StartRejected(reason.chars().take(2_000).collect()),
                )
                .await?;
        } else if args["unknown"].is_string() {
            if let Err(error) = self.probe_work(network, work_id, true).await {
                tracing::debug!(%error, work_id, "unknown start waits for the next probe");
            }
            record = self
                .store
                .lock()
                .await
                .work_record(work_id)?
                .context("work not found on this device")?;
        }
        Ok(json!({"work_id":work_id,"state":record.state}))
    }

    /// Reads the thread to settle a work's state; it never starts or resends anything. Before the
    /// Agent's window is over only a found turn is applied, so a late send is never written off.
    pub(super) async fn probe_work(
        self: &Arc<Self>,
        network: &str,
        work_id: &str,
        now: bool,
    ) -> Result<()> {
        let record = self
            .store
            .lock()
            .await
            .work_record(work_id)?
            .context("work not found")?;
        if record.kind != WorkKind::Codex {
            return Ok(());
        }
        let over = record.send_window_over(now_ms());
        // A bound turn is read any time; only an unsent work waits for the Agent's window.
        let unsent = matches!(
            record.state,
            WorkState::Accepted | WorkState::Starting { .. }
        );
        if !now && !over && unsent {
            return Ok(());
        }
        let epoch = self.codex_epoch();
        let codex = &self.codex;
        let probe = match &record.state {
            WorkState::Accepted | WorkState::Starting { thread: None } => {
                if over {
                    self.signal_work(
                        network,
                        work_id,
                        Signal::StartRejected("协调 Agent 没有在时限内开始这项工作".into()),
                    )
                    .await?;
                }
                return Ok(());
            }
            WorkState::Starting {
                thread: Some(thread),
            } => codex.probe(thread, None, work_id, epoch).await,
            state => match state.handle() {
                Some(handle) => {
                    codex
                        .probe(&handle.thread_id, Some(&handle.turn_id), work_id, epoch)
                        .await
                }
                None => return Ok(()),
            },
        };
        match probe {
            Ok(Probe::NotStarted | Probe::Missing)
                if !over && matches!(record.state, WorkState::Starting { .. }) =>
            {
                Ok(())
            }
            Ok(probe) => {
                self.signal_work(network, work_id, Signal::Probed(probe))
                    .await?;
                Ok(())
            }
            Err(error) => bail!("App Server probe failed: {error}"),
        }
    }

    /// Settles works the Agent registered but did not finish starting within its window.
    pub(super) async fn reconcile_works(self: &Arc<Self>, network: &str) -> Result<()> {
        let records = self.store.lock().await.work_records()?;
        for record in records {
            if matches!(
                record.state,
                WorkState::Accepted | WorkState::Starting { .. }
            ) && record.kind == WorkKind::Codex
                && record.send_window_over(now_ms())
                && let Err(error) = self.probe_work(network, &record.work_id, false).await
            {
                tracing::debug!(%error, "work probe waits for the App Server");
            }
        }
        Ok(())
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
            .work_record(work_id)?
            .context("work not found")?;
        match &record.state {
            WorkState::Running { handle } => {
                self.codex
                    .interrupt(&handle.thread_id, &handle.turn_id)
                    .await
                    .map_err(|error| anyhow::anyhow!("interrupt was not confirmed: {error}"))?;
                self.signal_work(network, work_id, Signal::InterruptRequested)
                    .await?;
            }
            // An agent task stops with its coordination turn.
            WorkState::Accepted if record.kind == WorkKind::Task => {
                let job = self.store.lock().await.request_manager_cancel(
                    &self.identity,
                    network,
                    &record.conversation,
                    &task_trigger(work_id),
                )?;
                if let Some(crate::scheduling::JobState::Cancelling { attempt }) =
                    job.map(|job| job.state)
                {
                    self.fence_manager_attempt(&attempt.id).await?;
                }
                self.publish_changed(network, &record.conversation, work_id);
            }
            _ => {}
        }
        // Stopping a work stops what it delegated.
        let children = {
            let store = self.store.lock().await;
            let mut children = Vec::new();
            for work in store.works(network, &record.conversation)? {
                if work.parent.as_deref() == Some(work_id)
                    && store
                        .work_record(&work.work_id)?
                        .is_some_and(|child| live(&child))
                {
                    children.push(work.work_id);
                }
            }
            children
        };
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
                self.store.lock().await.save_stop_request(&request)?;
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
        let dispatches = self.store.lock().await.dispatches()?;
        for dispatch in dispatches {
            if dispatch.state == DispatchState::Pending {
                self.send_dispatch(network, dispatch.spec).await?;
            }
        }
        let requests = self.store.lock().await.stop_requests()?;
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
                    .forget_stop_request(&request.work_id)?;
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
                    .forget_stop_request(&request.work_id)?;
            } else if now_ms() >= request.deadline_ms {
                request.timed_out = true;
                self.store.lock().await.save_stop_request(&request)?;
            } else {
                continue;
            }
            self.publish_changed(network, &request.conversation, &request.work_id);
        }
        Ok(())
    }

    /// Maps App Server notifications and requests onto the work that owns their thread and turn.
    pub(super) async fn route_codex_event(
        self: &Arc<Self>,
        network: &str,
        event: Value,
    ) -> Result<()> {
        let method = event["method"].as_str().unwrap_or_default();
        let records = self.store.lock().await.work_records()?;
        match method {
            "bridge.codex_connected" => {
                let epoch = self.store.lock().await.next_codex_epoch()?;
                self.codex_epoch
                    .store(epoch, std::sync::atomic::Ordering::SeqCst);
                for record in records.into_iter().filter(live) {
                    if let Err(error) = self.probe_work(network, &record.work_id, false).await {
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
                    self.signal_work(network, &record.work_id, Signal::Disconnected)
                        .await?;
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
        let records = self.store.lock().await.work_records()?;
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
        let work_id = record.work_id.clone();
        self.publish_progress(
            network,
            json!({"type":"native_progress","conversation_id":record.conversation,"work_id":work_id,
                "target_node":self.identity.device_id()?,"sequence":self.progress_sequence.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1,
                "progress":observation::native_progress(&event)}),
        );
        let item = &params["item"];
        let signal = match method {
            "item/completed" if item["type"] == "agentMessage" && item["phase"] == "commentary" => {
                Signal::Progress {
                    summary: item["text"].as_str().unwrap_or_default().into(),
                    at_ms: now_ms(),
                }
            }
            "item/completed"
                if item["type"] == "agentMessage"
                    && (item["phase"].is_null() || item["phase"] == "final_answer") =>
            {
                Signal::Text(item["text"].as_str().unwrap_or_default().into())
            }
            "turn/plan/updated" => Signal::Progress {
                summary: plan_summary(params),
                at_ms: now_ms(),
            },
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
            _ => return Ok(()),
        };
        self.signal_work(network, &work_id, signal).await?;
        Ok(())
    }

    pub(super) fn start_codex(self: &Arc<Self>, mut events: mpsc::UnboundedReceiver<Value>) {
        self.codex.start();
        let node = self.clone();
        tokio::spawn(async move {
            while let Some(event) = events.recv().await {
                match event["method"].as_str() {
                    Some("bridge.codex_request") => {
                        node.on_codex_request(&event["params"]).await;
                        continue;
                    }
                    Some("bridge.codex_disconnected") => node.drop_codex_requests(),
                    _ => {}
                }
                node.triage_app_server(&event);
                let network = node.store.lock().await.network().ok().flatten();
                if let Some(network) = network
                    && let Err(error) = node.route_codex_event(&network, event).await
                {
                    tracing::warn!(%error, "could not apply an App Server event");
                }
            }
        });
    }
}
