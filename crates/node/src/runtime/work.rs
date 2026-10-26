//! Drives the work state machine from App Server results and events. Every change goes through
//! `Store::apply_work_signal`, so the journal, the DAG and the UI agree.
use super::*;
use crate::scheduling::task_trigger;
use crate::work::{
    Dispatch, DispatchState, Handle, Probe, Signal, StopRequest, WorkRecord, WorkSpec, WorkState,
};

/// An interrupt the App Server took is checked against the turn once this passes without its end.
const INTERRUPT_TIMEOUT: Duration = Duration::from_secs(30);
/// A stop not confirmed by then shows as still trying; it is retried until its work ends.
const STOP_DELIVERY_TIMEOUT: Duration = Duration::from_secs(30);
/// The user waits this long for the first stop attempt; a stop is idempotent, so retries are safe.
const STOP_ATTEMPT_TIMEOUT: Duration = Duration::from_secs(5);
/// Retries of a held stop start this far apart and double up to `STOP_RETRY_MAX_MS`.
const STOP_RETRY_MS: i64 = 3_000;
const STOP_RETRY_MAX_MS: i64 = 60_000;

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
        if !quiet && !live(&record) {
            self.drop_stop(work_id).await?;
        }
        self.publish_changed(network, &record.conversation, Changed::Work(work_id));
        if receipt.is_some() {
            self.wake_scheduler()?;
        }
        // A held stop reaches a turn as soon as it is known to run, as when it binds.
        if !quiet
            && let WorkState::Running { handle } = &record.state
            && self.waiting_stop(work_id).await?
            && self.stop_attempt_due(work_id)
        {
            match Box::pin(self.interrupt_turn(network, work_id, handle)).await {
                Ok(stopped) => return Ok(stopped),
                Err(error) => tracing::warn!(%error, work_id, "stop waits for the next reconcile"),
            }
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
        // A send goes into a thread that already exists, often outside the roots; it picks no directory.
        if spec.thread.is_none() {
            self.allowed_path(&spec.cwd)?;
        }
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
            Some(("schedule", _)) => {
                ensure!(
                    from_owner,
                    "only the conversation's scheduling device dispatches its work"
                );
                let (schedule_id, _) = crate::scheduling::parse_schedule_trigger(&spec.trigger)
                    .context("invalid work trigger")?;
                ensure!(
                    store.schedule(network, schedule_id)?.is_some_and(|entry| {
                        entry.conversation_id == spec.conversation && entry.schedule.enabled
                    }),
                    "the schedule this work runs for is paused, deleted or has not reached this device"
                );
            }
            Some(("bot_message", message_id)) => {
                ensure!(
                    from_owner,
                    "only the conversation's scheduling device dispatches its work"
                );
                let message = store
                    .message(network, &spec.conversation, message_id)?
                    .context("the bot's message has not reached this device")?;
                ensure!(
                    message.role == Role::Bot,
                    "work must follow a message in this conversation"
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
            "another device takes work only as a task for its bot"
        );
        if spec.kind == WorkKind::Task {
            ensure!(
                spec.dispatcher != spec.target,
                "an agent task goes to another device"
            );
            ensure!(
                self.manager().is_some(),
                "this device has no bot to take the task"
            );
        }
        Ok(())
    }

    /// Records the dispatch. A Codex work is then started by the Agent that registered it; an
    /// agent task is queued for this device's bot. A retry of the same `work_id` returns the
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
        self.publish_changed(
            network,
            &record.conversation,
            Changed::Work(&record.work_id),
        );
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
        self.publish_changed(
            network,
            &dispatch.spec.conversation,
            Changed::Work(&dispatch.spec.work_id),
        );
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

    /// Settles works the Agent registered but did not finish starting within its window, and
    /// carries out the stops this device holds for its own works.
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
        let local = self.identity.device_id()?;
        let stops = self.store.lock().await.stop_requests()?;
        for request in stops.into_iter().filter(|request| request.target == local) {
            let work_id = request.work_id.clone();
            if let Err(error) = self.reconcile_stop(network, request).await {
                tracing::debug!(%error, work_id, "stop waits for the App Server");
            }
        }
        Ok(())
    }

    /// Whether the user stopped this device's work and the work has not ended yet.
    async fn waiting_stop(&self, work_id: &str) -> Result<bool> {
        let local = self.identity.device_id()?;
        Ok(self
            .store
            .lock()
            .await
            .stop_requests()?
            .iter()
            .any(|request| request.work_id == work_id && request.target == local))
    }

    /// Takes a turn at a held stop: true when one is due, and the next then waits 3 s, 6 s, …
    /// up to a minute. Taking the turn first keeps two attempts from running at once.
    fn stop_attempt_due(&self, work_id: &str) -> bool {
        let at = now_ms();
        let mut retries = self.stop_retries.lock().expect("stop retries");
        let (attempts, next) = retries.entry(work_id.to_owned()).or_insert((0, at));
        if at < *next {
            return false;
        }
        *next = at + (STOP_RETRY_MS << (*attempts).min(5)).min(STOP_RETRY_MAX_MS);
        *attempts += 1;
        true
    }

    /// Keeps the user's stop until its work ends. A stop asked again keeps its first request.
    async fn hold_stop(&self, request: &StopRequest) -> Result<bool> {
        let mut store = self.store.lock().await;
        if store
            .stop_requests()?
            .iter()
            .any(|held| held.work_id == request.work_id)
        {
            return Ok(false);
        }
        store.save_stop_request(request)?;
        Ok(true)
    }

    async fn drop_stop(&self, work_id: &str) -> Result<()> {
        self.store.lock().await.forget_stop_request(work_id)?;
        self.stop_retries
            .lock()
            .expect("stop retries")
            .remove(work_id);
        Ok(())
    }

    /// Sends `turn/interrupt` for a held stop. Only the App Server's answer moves the work to
    /// interrupting and starts the deadline; a failed or unknown result leaves the stop to the
    /// next reconcile, which reads the turn before asking again.
    async fn interrupt_turn(
        self: &Arc<Self>,
        network: &str,
        work_id: &str,
        handle: &Handle,
    ) -> Result<WorkRecord> {
        self.codex
            .interrupt(&handle.thread_id, &handle.turn_id)
            .await
            .map_err(|error| anyhow::anyhow!("interrupt was not confirmed: {error}"))?;
        self.stop_retries
            .lock()
            .expect("stop retries")
            .remove(work_id);
        {
            let mut store = self.store.lock().await;
            // A turn that outlived an interrupt the App Server took is still being stopped.
            let again = store
                .work_record(work_id)?
                .is_some_and(|record| matches!(record.state, WorkState::Interrupting { .. }));
            if let Some(mut request) = store
                .stop_requests()?
                .into_iter()
                .find(|request| request.work_id == work_id)
            {
                request.deadline_ms = now_ms() + INTERRUPT_TIMEOUT.as_millis() as i64;
                request.timed_out = again;
                store.save_stop_request(&request)?;
            }
        }
        self.signal_work(network, work_id, Signal::InterruptRequested)
            .await
    }

    /// Carries out a stop this device holds for its own work until the work ends. Before asking
    /// again it reads the turn, so a turn that has ended is settled and never interrupted. An
    /// interrupt the App Server took that has not ended the turn by the deadline is sent again;
    /// one whose turn cannot be read then leaves the work unverified.
    async fn reconcile_stop(
        self: &Arc<Self>,
        network: &str,
        mut request: StopRequest,
    ) -> Result<()> {
        let work_id = request.work_id.clone();
        let record = self.store.lock().await.work_record(&work_id)?;
        let Some(record) = record.filter(live) else {
            self.drop_stop(&work_id).await?;
            self.publish_changed(network, &request.conversation, Changed::Work(&work_id));
            return Ok(());
        };
        // An unbound work is interrupted once its turn binds.
        let Some(handle) = record.state.handle().cloned() else {
            return Ok(());
        };
        let at = now_ms();
        if !request.timed_out && at >= request.deadline_ms {
            request.timed_out = true;
            self.store.lock().await.save_stop_request(&request)?;
            self.publish_changed(network, &request.conversation, Changed::Work(&work_id));
        }
        let interrupting = matches!(record.state, WorkState::Interrupting { .. });
        if (interrupting && at < request.deadline_ms) || !self.stop_attempt_due(&work_id) {
            return Ok(());
        }
        let probe = self.codex.probe(
            &handle.thread_id,
            Some(&handle.turn_id),
            &work_id,
            self.codex_epoch(),
        );
        match tokio::time::timeout(STOP_ATTEMPT_TIMEOUT, probe).await {
            Ok(Ok(Probe::Running { turn_id, epoch })) => {
                if matches!(record.state, WorkState::Unverified { .. }) {
                    let running = Probe::Running { turn_id, epoch };
                    self.signal_work(network, &work_id, Signal::Probed(running))
                        .await?;
                }
                self.interrupt_turn(network, &work_id, &handle).await?;
            }
            Ok(Ok(probe)) => {
                self.signal_work(network, &work_id, Signal::Probed(probe))
                    .await?;
            }
            failed => {
                if interrupting {
                    self.signal_work(network, &work_id, Signal::InterruptTimedOut)
                        .await?;
                }
                bail!("the turn could not be read: {failed:?}");
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
        let mut waiting_for_start = false;
        let mut retrying = false;
        match &record.state {
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
                self.publish_changed(network, &record.conversation, Changed::Work(work_id));
            }
            // The stop is stored before anything is sent, so a failed interrupt or a restart
            // leaves it to the reconcile. A stop asked again sends nothing itself.
            WorkState::Accepted
            | WorkState::Starting { .. }
            | WorkState::Running { .. }
            | WorkState::Unverified {
                handle: Some(_),
                probe: true,
                ..
            } if record.kind == WorkKind::Codex => {
                let at = now_ms();
                let held = self
                    .hold_stop(&StopRequest {
                        work_id: work_id.into(),
                        conversation: record.conversation.clone(),
                        target: self.identity.device_id()?,
                        requested_at_ms: at,
                        deadline_ms: at + STOP_DELIVERY_TIMEOUT.as_millis() as i64,
                        timed_out: false,
                    })
                    .await?;
                match &record.state {
                    WorkState::Running { handle } => {
                        retrying = !(held
                            && self.stop_attempt_due(work_id)
                            && Box::pin(self.interrupt_turn(network, work_id, handle))
                                .await
                                .inspect_err(|error| {
                                    tracing::warn!(%error, work_id, "stop waits for the next reconcile")
                                })
                                .is_ok());
                    }
                    WorkState::Unverified { .. } => retrying = true,
                    _ => waiting_for_start = true,
                }
                self.publish_changed(network, &record.conversation, Changed::Work(work_id));
            }
            _ => {}
        }
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
        Ok(
            json!({"work_id":work_id,"interrupt_requested":true,"confirmed":false,"waiting_for_start":waiting_for_start,"retrying":retrying}),
        )
    }

    /// Stops a work wherever it runs. A stop for another device is stored first and delivered
    /// until that device takes it or the work ends.
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
        let at = now_ms();
        let request = StopRequest {
            work_id: work_id.into(),
            conversation: scope.into(),
            target: work.target_node,
            requested_at_ms: at,
            deadline_ms: at + STOP_DELIVERY_TIMEOUT.as_millis() as i64,
            timed_out: false,
        };
        self.hold_stop(&request).await?;
        if self.stop_attempt_due(work_id) {
            match tokio::time::timeout(STOP_ATTEMPT_TIMEOUT, self.deliver_stop(network, &request))
                .await
            {
                Ok(Ok(reply)) => {
                    self.drop_stop(work_id).await?;
                    self.publish_changed(network, scope, Changed::Work(work_id));
                    return Ok(reply);
                }
                failed => tracing::debug!(?failed, "stop request waits for its device"),
            }
        }
        self.publish_changed(network, scope, Changed::Work(work_id));
        Ok(
            json!({"work_id":work_id,"interrupt_requested":true,"confirmed":false,"waiting_for_device":true}),
        )
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

    /// Retries waiting dispatches and the stops held for other devices' works.
    pub(super) async fn deliver_work_requests(self: &Arc<Self>, network: &str) -> Result<()> {
        let dispatches = self.store.lock().await.dispatches()?;
        for dispatch in dispatches {
            if dispatch.state == DispatchState::Pending {
                self.send_dispatch(network, dispatch.spec).await?;
            }
        }
        let local = self.identity.device_id()?;
        let stops = self.store.lock().await.stop_requests()?;
        for request in stops.into_iter().filter(|request| request.target != local) {
            self.deliver_held_stop(network, request).await?;
        }
        Ok(())
    }

    /// Delivers a stop until its device takes it, which then holds it itself, or the work ends.
    /// A lost reply only means the device is asked again, where a stop is a no-op. Past its
    /// deadline the request shows as still trying; it is never given up.
    async fn deliver_held_stop(
        self: &Arc<Self>,
        network: &str,
        mut request: StopRequest,
    ) -> Result<()> {
        let work_id = request.work_id.clone();
        let ended = self
            .store
            .lock()
            .await
            .works(network, &request.conversation)?
            .iter()
            .any(|work| work.work_id == work_id && work.outcome.is_some());
        if !ended && !request.timed_out && now_ms() >= request.deadline_ms {
            request.timed_out = true;
            self.store.lock().await.save_stop_request(&request)?;
            self.publish_changed(network, &request.conversation, Changed::Work(&work_id));
        }
        if !ended && !self.stop_attempt_due(&work_id) {
            return Ok(());
        }
        let delivered = !ended
            && matches!(
                tokio::time::timeout(STOP_ATTEMPT_TIMEOUT, self.deliver_stop(network, &request))
                    .await,
                Ok(Ok(_))
            );
        if ended || delivered {
            self.drop_stop(&work_id).await?;
            self.publish_changed(network, &request.conversation, Changed::Work(&work_id));
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
        let works: Vec<_> = records
            .into_iter()
            .filter(|record| {
                record.state.handle().is_some_and(|h| {
                    Some(h.thread_id.as_str()) == thread && Some(h.turn_id.as_str()) == turn
                })
            })
            .collect();
        if works.is_empty() {
            return Ok(());
        }
        let item = &params["item"];
        let signal = match method {
            "item/completed" if item["type"] == "agentMessage" && item["phase"] == "commentary" => {
                Some(Signal::Progress {
                    summary: item["text"].as_str().unwrap_or_default().into(),
                    at_ms: now_ms(),
                })
            }
            "item/completed"
                if item["type"] == "agentMessage"
                    && (item["phase"].is_null() || item["phase"] == "final_answer") =>
            {
                Some(Signal::Text(
                    item["text"].as_str().unwrap_or_default().into(),
                ))
            }
            "turn/plan/updated" => Some(Signal::Progress {
                summary: plan_summary(params),
                at_ms: now_ms(),
            }),
            "turn/completed" => Some(Signal::TurnCompleted {
                turn_id: turn.unwrap_or_default().into(),
                status: params["turn"]["status"]
                    .as_str()
                    .unwrap_or("unknown")
                    .into(),
                text: String::new(),
                error: params["turn"]["error"]["message"]
                    .as_str()
                    .map(str::to_owned),
            }),
            _ => None,
        };
        let target_node = self.identity.device_id()?;
        // A send steered into a running turn shares it with the work that started it.
        for record in works {
            self.publish_progress(
                network,
                json!({"type":"native_progress","conversation_id":record.conversation,"work_id":record.work_id,
                    "target_node":target_node,"sequence":self.progress_sequence.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1,
                    "progress":observation::native_progress(&event)}),
            );
            if let Some(signal) = signal.clone() {
                self.signal_work(network, &record.work_id, signal).await?;
            }
        }
        Ok(())
    }

    pub(super) fn start_codex(self: &Arc<Self>, mut events: mpsc::UnboundedReceiver<Value>) {
        let node = self.clone();
        tokio::spawn(async move {
            // Known before the first connect, so requests on these threads come back to the Node
            // before the client reports its threads observed.
            node.observe_live_threads().await;
            node.codex.start();
            while let Some(event) = events.recv().await {
                match event["method"].as_str() {
                    Some("bridge.codex_request") => {
                        node.on_codex_request(&event["params"]).await;
                        continue;
                    }
                    Some("bridge.codex_observed") => {
                        if let Err(error) = node.close_lost_questions().await {
                            tracing::warn!(%error, "could not close questions about lost requests");
                        }
                        continue;
                    }
                    Some("bridge.codex_connected") => node.codex.connected(),
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
