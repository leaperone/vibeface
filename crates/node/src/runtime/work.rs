//! Drives the work state machine from App Server results and events. Every change goes through
//! `Store::apply_work_signal`, so the journal, the DAG and the UI agree.
use super::*;
use crate::scheduling::task_trigger;
use crate::work::{
    Dispatch, DispatchState, Handle, Probe, Signal, StopRequest, WorkRecord, WorkSpec, WorkState,
};
use vibeface_bridge::{
    codex::{ExecEvent, StartReport},
    failure::{ExecFailure, FailureKind},
};

/// An interrupt the App Server took is checked against the turn once this passes without its end.
const INTERRUPT_TIMEOUT: Duration = Duration::from_secs(30);
/// A stop not confirmed by then shows as still trying; it is retried until its work ends.
const STOP_DELIVERY_TIMEOUT: Duration = Duration::from_secs(30);
/// Reconciliation reads the turn of each running or unverified work this often, so an end the
/// event stream missed is still recorded.
const LIVE_PROBE_MS: i64 = 60_000;
/// The user waits this long for the first stop attempt; a stop is idempotent, so retries are safe.
const STOP_ATTEMPT_TIMEOUT: Duration = Duration::from_secs(5);
/// Retries of a held stop start this far apart and double up to `STOP_RETRY_MAX_MS`.
const STOP_RETRY_MS: i64 = 3_000;
const STOP_RETRY_MAX_MS: i64 = 60_000;

fn live(record: &WorkRecord) -> bool {
    !matches!(
        record.state,
        WorkState::Finished { .. } | WorkState::Unverified { probe: false, .. }
    )
}

impl Runtime {
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
            let thread_id = record.state.thread();
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
            .filter_map(|record| record.state.thread().map(str::to_owned))
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
                // A refused or unknown dispatch of this Node has no result in the DAG; only this
                // Node, which holds it, can follow it up.
                let ended = store
                    .works(network, &spec.conversation)?
                    .iter()
                    .any(|work| work.work_id == previous && work.outcome.is_some())
                    || spec.dispatcher == self.identity.device_id()?
                        && store.dispatch(previous)?.is_some_and(|dispatch| {
                            dispatch.spec.conversation == spec.conversation
                                && dispatch.state.ended()
                        });
                ensure!(
                    ended,
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
            Err(error) => json!({"rejected":error.to_string(),
                "failure":error.downcast_ref::<ExecFailure>().map(|failure| failure.kind)}),
        })
    }

    /// A dispatch to another Node waits in this Node's journal until the target answers; an
    /// unknown answer is retried with the same `work_id`, which the target accepts at most once.
    /// `notify` is set for a retry outside the dispatching turn: a refusal then wakes the bot.
    pub(super) async fn send_dispatch(
        self: &Arc<Self>,
        network: &str,
        spec: WorkSpec,
        notify: bool,
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
                failure: serde_json::from_value(reply["failure"].clone()).unwrap_or_default(),
            },
            Err(error) => {
                tracing::debug!(%error, "dispatch waits for its target");
                return Ok(dispatch);
            }
        };
        let spec = &dispatch.spec;
        match &state {
            DispatchState::Rejected { reason, .. } => tracing::warn!(
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
        let (dispatch, receipt) = self.store.lock().await.answer_dispatch(
            network,
            &self.identity.device_id()?,
            &spec.work_id,
            state,
            notify,
        )?;
        self.publish_changed(
            network,
            &dispatch.spec.conversation,
            Changed::Work(&dispatch.spec.work_id),
        );
        if receipt.is_some() {
            self.wake_scheduler()?;
        }
        Ok(dispatch)
    }

    /// The Agent's report of its own send: the thread before `turn/start`, then the turn, a
    /// refusal, or an unknown result that the Node settles by reading the thread.
    pub(super) async fn bind_work(
        self: &Arc<Self>,
        network: &str,
        scope: &str,
        work_id: &str,
        report: StartReport,
    ) -> Result<Value> {
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
        match report {
            StartReport::Thread { thread_id } => {
                tracing::info!(
                    event = "work.thread_bound",
                    conversation_id = scope,
                    work_id,
                    thread_id,
                    "Codex thread bound to the work"
                );
                vibeface_protocol::validate_id(&thread_id).map_err(anyhow::Error::msg)?;
                if matches!(record.state, WorkState::Accepted) {
                    ensure!(
                        now_ms() < record.send_by_ms,
                        "the send window for this work is over; it will not start"
                    );
                    record = self
                        .signal_work(network, work_id, Signal::ThreadCreated(thread_id.clone()))
                        .await?;
                }
                ensure!(
                    matches!(&record.state, WorkState::Starting { thread: Some(known) } if *known == thread_id),
                    "this work is already bound to another thread or past starting"
                );
            }
            StartReport::Turn { turn_id } => {
                tracing::info!(
                    event = "work.turn_started",
                    conversation_id = scope,
                    work_id,
                    turn_id,
                    "Codex turn started"
                );
                let signal = Signal::TurnStarted {
                    turn_id: turn_id.clone(),
                    epoch: self.codex.epoch(),
                };
                record = self.signal_work(network, work_id, signal).await?;
                if record
                    .state
                    .handle()
                    .is_none_or(|handle| handle.turn_id != turn_id)
                {
                    tracing::warn!(
                        event = "work.turn_untracked",
                        conversation_id = scope,
                        work_id,
                        turn_id,
                        state = record.state.name(),
                        "a turn started after its work had ended; the Node does not follow it"
                    );
                }
                // The turn may have ended before it was bound.
                if let Some(handle) = record.state.handle().cloned()
                    && let Ok(probe @ Probe::Terminal { .. }) = self
                        .codex
                        .probe(&handle.thread_id, Some(&handle.turn_id), work_id)
                        .await
                {
                    record = self
                        .signal_work(network, work_id, Signal::Probed(probe))
                        .await?;
                }
            }
            StartReport::Rejected(mut failure) => {
                tracing::warn!(
                    event = "work.start_failed",
                    conversation_id = scope,
                    work_id,
                    kind = ?failure.kind,
                    reason = failure.detail,
                    "Codex did not start the work"
                );
                failure.detail = failure.detail.chars().take(2_000).collect();
                record = self
                    .signal_work(network, work_id, Signal::StartRejected(failure))
                    .await?;
            }
            StartReport::Unknown { reason } => {
                tracing::warn!(
                    event = "work.start_unknown",
                    conversation_id = scope,
                    work_id,
                    reason,
                    "unknown whether Codex started the work"
                );
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
        let codex = &self.codex;
        let probe = match &record.state {
            WorkState::Accepted | WorkState::Starting { thread: None } => {
                if over {
                    self.signal_work(
                        network,
                        work_id,
                        Signal::StartRejected(ExecFailure::new(
                            FailureKind::TimedOut,
                            "the coordination Agent did not start this work in time",
                        )),
                    )
                    .await?;
                }
                return Ok(());
            }
            state => match (state.handle(), state.thread()) {
                (Some(handle), _) => {
                    codex
                        .probe(&handle.thread_id, Some(&handle.turn_id), work_id)
                        .await
                }
                // A send not seen in time is looked for by its work ID.
                (None, Some(thread)) => codex.probe(thread, None, work_id).await,
                (None, None) => return Ok(()),
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

    /// Settles works the Agent registered but did not finish starting within its window, reads
    /// the turns of running and unverified works about once a minute, and carries out the stops
    /// this device holds for its own works. A probe only reads; it never starts or resends.
    pub(super) async fn reconcile_works(self: &Arc<Self>, network: &str) -> Result<()> {
        use std::sync::atomic::Ordering::Relaxed;
        let at = now_ms();
        let last = self.live_probe_at.load(Relaxed);
        let read_live = at - last >= LIVE_PROBE_MS
            && self
                .live_probe_at
                .compare_exchange(last, at, Relaxed, Relaxed)
                .is_ok();
        let records = self.store.lock().await.work_records()?;
        for record in records {
            if record.kind != WorkKind::Codex {
                continue;
            }
            let due = match record.state {
                WorkState::Accepted | WorkState::Starting { .. } => record.send_window_over(at),
                WorkState::Running { .. } | WorkState::Unverified { probe: true, .. } => read_live,
                _ => false,
            };
            if due && let Err(error) = self.probe_work(network, &record.work_id, false).await {
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
        let probe = self
            .codex
            .probe(&handle.thread_id, Some(&handle.turn_id), &work_id);
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
            | WorkState::Unverified { probe: true, .. }
                if record.kind == WorkKind::Codex =>
            {
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
                    WorkState::Unverified {
                        handle: Some(_), ..
                    } => retrying = true,
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
                self.send_dispatch(network, dispatch.spec, true).await?;
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

    /// A new connection gets the next epoch and has every live work read again; a lost one
    /// leaves the running works waiting for that read.
    async fn on_connection(self: &Arc<Self>, connected: bool) -> Result<()> {
        if connected {
            let epoch = self.store.lock().await.next_codex_epoch();
            self.codex
                .connected(*epoch.as_ref().unwrap_or(&self.codex.epoch()));
            epoch?;
        } else {
            self.drop_codex_requests();
        }
        let Some(network) = self.store.lock().await.network()? else {
            return Ok(());
        };
        let records = self.store.lock().await.work_records()?;
        for record in records.into_iter().filter(live) {
            if connected {
                if let Err(error) = self.probe_work(&network, &record.work_id, false).await {
                    tracing::debug!(%error, "work probe after reconnect failed");
                }
            } else if record.state.handle().is_some() {
                self.signal_work(&network, &record.work_id, Signal::Disconnected)
                    .await?;
            }
        }
        Ok(())
    }

    /// Applies an App Server notification to the works whose turn it is about.
    async fn route_turn_event(self: &Arc<Self>, network: &str, event: Value) -> Result<()> {
        let turn = codex_server::turn_event(&event);
        let records = self.store.lock().await.work_records()?;
        // A turn on the thread of a send not seen in time may be that send; reading the thread
        // tells, and settles the work if so.
        if turn.boundary {
            let unseen: Vec<_> = records
                .iter()
                .filter(|record| {
                    matches!(
                        &record.state,
                        WorkState::Unverified {
                            handle: None,
                            probe: true,
                            ..
                        }
                    ) && record.state.thread() == turn.thread
                })
                .map(|record| record.work_id.clone())
                .collect();
            for work_id in unseen {
                if let Err(error) = Box::pin(self.probe_work(network, &work_id, true)).await {
                    tracing::debug!(%error, work_id, "an unseen send waits for the next probe");
                }
            }
        }
        let works: Vec<_> = records
            .into_iter()
            .filter(|record| {
                record.state.handle().is_some_and(|h| {
                    Some(h.thread_id.as_str()) == turn.thread
                        && Some(h.turn_id.as_str()) == turn.turn
                })
            })
            .collect();
        let target_node = self.identity.device_id()?;
        // A send steered into a running turn shares it with the work that started it.
        for record in works {
            self.publish_progress(
                network,
                json!({"type":"native_progress","conversation_id":record.conversation,"work_id":record.work_id,
                    "target_node":target_node,"sequence":self.progress_sequence.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1,
                    "progress":observation::native_progress(&event)}),
            );
            if let Some(signal) = turn.signal.clone() {
                self.signal_work(network, &record.work_id, signal).await?;
            }
        }
        Ok(())
    }

    pub(super) fn start_codex(self: &Arc<Self>, mut events: mpsc::UnboundedReceiver<ExecEvent>) {
        let node = self.clone();
        tokio::spawn(async move {
            // Known before the first connect, so requests on these threads come back to the Node
            // before the client reports its threads observed.
            node.observe_live_threads().await;
            node.codex.start();
            while let Some(event) = events.recv().await {
                let applied = match event {
                    ExecEvent::Request(request) => {
                        node.on_codex_request(&request).await;
                        Ok(())
                    }
                    ExecEvent::Observed => node.close_lost_questions().await,
                    ExecEvent::Connected => node.on_connection(true).await,
                    ExecEvent::Disconnected => node.on_connection(false).await,
                    ExecEvent::Message(message) => {
                        node.triage_app_server(&message);
                        let network = node.store.lock().await.network();
                        match network {
                            Ok(Some(network)) => node.route_turn_event(&network, message).await,
                            other => other.map(|_| ()),
                        }
                    }
                };
                if let Err(error) = applied {
                    tracing::warn!(%error, "could not apply an App Server event");
                }
            }
        });
    }
}
