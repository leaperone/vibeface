//! Drives the work state machine from what the providers report and read back. Provider signals
//! go through `Store::apply_work_signal`, so the journal, the DAG and the UI agree.
use super::agent_tools::ExecutionProviderRequest;
use super::*;
use crate::scheduling::task_trigger;
use crate::store::admission::{AdmissionRejected, authorize_peer_node_from};
use crate::work::{
    Dispatch, DispatchState, Probe, RequestedWorkKind, Signal, StopRequest, WorkAdmission,
    WorkRecord, WorkRequest, WorkSpec, WorkState,
};
use vibeface_bridge::{
    codex::{ExecEvent, StartReport},
    failure::{ExecFailure, FailureKind},
};

/// An interrupt the App Server took is checked against the turn once this passes without its end.
const INTERRUPT_TIMEOUT: Duration = Duration::from_secs(30);
/// A stop not confirmed by then shows as still trying; it is retried until its work ends.
pub(super) const STOP_DELIVERY_TIMEOUT: Duration = Duration::from_secs(30);
/// Reconciliation reads the turn of each running or unverified work this often, so an end the
/// event stream missed is still recorded.
const LIVE_PROBE_MS: i64 = 60_000;
/// The user waits this long for the first stop attempt; a stop is idempotent, so retries are safe.
const STOP_ATTEMPT_TIMEOUT: Duration = Duration::from_secs(5);
/// Retries of a held stop start this far apart and double up to `STOP_RETRY_MAX_MS`.
const STOP_RETRY_MS: i64 = 3_000;
const STOP_RETRY_MAX_MS: i64 = 60_000;

pub(super) fn live(record: &WorkRecord) -> bool {
    !matches!(
        record.state,
        WorkState::Finished { .. } | WorkState::Unverified { probe: false, .. }
    )
}

/// Whether the work's turn runs on the App Server, whose connection and events the Node follows.
fn on_app_server(record: &WorkRecord) -> bool {
    matches!(record.kind, WorkKind::Agent(provider) if provider.on_app_server())
}

struct WorkDispatchSettings {
    identity: LocalIdentity,
    roots: Vec<PathBuf>,
    manager: Option<config::ManagerSettings>,
    providers: crate::providers::Providers,
}

impl Runtime {
    pub(super) async fn signal_work(
        self: &Arc<Self>,
        network: &str,
        work_id: &str,
        signal: Signal,
    ) -> Result<WorkRecord> {
        let quiet = matches!(signal, Signal::Text(_) | Signal::Progress { .. });
        let identity = self.identity.clone();
        let call_network = network.to_owned();
        let call_work = work_id.to_owned();
        let (before, (record, receipt)) = self
            .store
            .call("work.signal", move |store| {
                let before = if quiet {
                    None
                } else {
                    store
                        .work_record(&call_work)?
                        .map(|record| record.state.name())
                };
                let applied = store.apply_work_signal(
                    &identity,
                    &call_network,
                    &call_work,
                    signal,
                    now_ms(),
                )?;
                Ok((before, applied))
            })
            .await?;
        // A probe that finds the turn where it was changes nothing; only a new state is logged.
        if !quiet && before != Some(record.state.name()) {
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
        if !quiet && record.state.finished() && on_app_server(&record) {
            self.codex.release_work(work_id);
        }
        if !quiet && record.state.finished() && record.kind == WorkKind::Agent(Provider::Opencode) {
            self.opencode.release_work(work_id);
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
            && matches!(record.state, WorkState::Running { .. })
            && self.waiting_stop(work_id).await?
            && self.stop_attempt_due(work_id)
        {
            match Box::pin(self.interrupt_turn(network, &record)).await {
                Ok(stopped) => return Ok(stopped),
                Err(error) => {
                    tracing::warn!(event = "work.stop_pending", %error, work_id, "stop waits for the next reconcile")
                }
            }
        }
        // Only a state change can change which threads stay subscribed.
        if !quiet {
            self.observe_live_threads().await;
        }
        Ok(record)
    }

    /// Threads of live work stay subscribed across App Server reconnects. Threads on the user's
    /// own Codex are only probed: the Node keeps no connection there.
    async fn observe_live_threads(&self) {
        let threads = self
            .store
            .call("work.observe_threads", |store| store.open_work_records())
            .await
            .unwrap_or_default()
            .into_iter()
            .filter(|record| live(record) && on_app_server(record))
            .filter_map(|record| record.state.thread().map(str::to_owned))
            .filter(|thread| self.codex.user_thread(thread).is_none())
            .collect();
        let _ = self.codex.observe(threads);
    }

    /// Every dispatch, local or from another Node, passes the same checks: the trigger chain
    /// carries the user's signed message or their permission to continue, the directory is in
    /// this Node's roots, and an agent task has an Agent to run it.
    fn authorize_dispatch(
        store: &Store,
        network: &str,
        spec: &WorkRequest,
        kind: WorkKind,
        settings: &WorkDispatchSettings,
    ) -> Result<()> {
        if spec.thread.is_none() || !spec.cwd.is_empty() {
            let path = std::fs::canonicalize(&spec.cwd).context("execution path does not exist")?;
            let roots = settings
                .roots
                .iter()
                .map(std::fs::canonicalize)
                .collect::<Vec<_>>();
            let allowed = roots
                .iter()
                .filter_map(|root| root.as_ref().ok())
                .any(|root| path.starts_with(root));
            if !allowed {
                for root in roots {
                    root.context("configured execution root cannot be resolved")?;
                }
            }
            ensure!(
                allowed,
                AdmissionRejected::new(
                    "execution path is outside configured roots; ask the user to allow its folder with propose_provider_config roots"
                )
            );
            ensure!(path.is_dir(), "execution path must be a directory");
        }
        let conversation = store
            .conversation(network, &spec.conversation)?
            .context("conversation not found")?;
        let from_owner = conversation.owner.as_deref() == Some(spec.dispatcher.as_str());
        match spec.trigger.split_once(':') {
            Some(("message", message_id)) => {
                ensure!(
                    from_owner,
                    AdmissionRejected::new(
                        "only the conversation's scheduling device dispatches its work"
                    )
                );
                let message = store
                    .message(network, &spec.conversation, message_id)?
                    .context("the user's message has not reached this device")?;
                ensure!(
                    message.role == Role::User,
                    AdmissionRejected::new("work must follow the user's own message")
                );
            }
            Some(("work", previous)) => {
                ensure!(
                    from_owner,
                    AdmissionRejected::new(
                        "only the conversation's scheduling device dispatches its work"
                    )
                );
                // A refused or unknown dispatch of this Node has no result in the DAG; only this
                // Node, which holds it, can follow it up.
                let ended = store
                    .works(network, &spec.conversation)?
                    .iter()
                    .any(|work| work.work_id == previous && work.outcome.is_some())
                    || spec.dispatcher == settings.identity.device_id()?
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
                    AdmissionRejected::new(
                        "only the conversation's scheduling device dispatches its work"
                    )
                );
                let (schedule_id, _) = crate::scheduling::parse_schedule_trigger(&spec.trigger)
                    .context(AdmissionRejected::new("invalid work trigger"))?;
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
                    AdmissionRejected::new(
                        "only the conversation's scheduling device dispatches its work"
                    )
                );
                let message = store
                    .message(network, &spec.conversation, message_id)?
                    .context("the bot's message has not reached this device")?;
                ensure!(
                    message.role == Role::Bot,
                    AdmissionRejected::new("work must follow a message in this conversation")
                );
            }
            Some(("task", task)) => {
                let task = store
                    .work_record(task)?
                    .context("agent task not found on this device")?;
                ensure!(
                    spec.dispatcher == spec.target
                        && matches!(kind, WorkKind::Agent(_))
                        && task.kind == WorkKind::Task
                        && task.conversation == spec.conversation
                        && spec.parent.as_deref() == Some(task.work_id.as_str())
                        && !task.state.finished(),
                    AdmissionRejected::new("an agent task only starts work on its own device")
                );
            }
            Some(("assignment", assignment_id)) => {
                ensure!(
                    crate::store::workspace::assignment_work_allowed_from(
                        &store.connection,
                        network,
                        assignment_id,
                        &spec.dispatcher,
                        &spec.conversation
                    )?,
                    AdmissionRejected::new(
                        "this assignment does not authorize work in this conversation"
                    )
                );
            }
            _ => bail!(AdmissionRejected::new("invalid work trigger")),
        }
        if let Some(original) = &spec.continues {
            let original = store
                .work_record(original)?
                .context("the work to continue is not on this device")?;
            ensure!(
                original.conversation == spec.conversation,
                AdmissionRejected::new("a continuation keeps the original conversation")
            );
            match original.kind {
                _ if original.kind == kind => {}
                WorkKind::Agent(ran) => bail!(
                    "work {} ran on {}; continue it with agent {} or leave agent out",
                    original.work_id,
                    ran.label(),
                    ran.name()
                ),
                WorkKind::Task => bail!(AdmissionRejected::new(
                    "a continuation keeps the original kind"
                )),
            }
        }
        ensure!(
            kind == WorkKind::Task || spec.dispatcher == spec.target,
            AdmissionRejected::new("another device takes work only as a task for its bot")
        );
        if kind == WorkKind::Task {
            ensure!(
                spec.dispatcher != spec.target,
                AdmissionRejected::new("an agent task goes to another device")
            );
            if settings.manager.is_none() {
                return Err(ExecFailure::new(
                    FailureKind::NoBot,
                    "this device has no bot to take the task",
                )
                .into());
            }
        }
        Ok(())
    }

    /// Records the dispatch. Its provider then starts it (see `Executor::start_or_resume`); an agent task
    /// is queued for this device's bot. A retry of the same `work_id` returns the existing
    /// record and `false`, so it never starts a second turn.
    pub(super) async fn accept_dispatch(
        self: &Arc<Self>,
        network: &str,
        mut request: WorkRequest,
        args: &Value,
    ) -> Result<WorkAdmission> {
        request.model = request
            .model
            .as_deref()
            .map(str::trim)
            .filter(|model| !model.is_empty())
            .map(str::to_owned);
        ensure!(
            request.target == self.identity.device_id()?,
            AdmissionRejected::new("work targets another Node")
        );
        ensure!(
            request.work_id
                == WorkSpec::id(&request.conversation, &request.trigger, &request.call_ref),
            AdmissionRejected::new("work ID does not match its call")
        );
        let _settings = self.manager_controls.lock().await;
        let settings = WorkDispatchSettings {
            identity: self.identity.clone(),
            roots: self.settings_snapshot().roots,
            manager: self.manager(),
            providers: self.providers().switches,
        };
        let network = network.to_owned();
        let call_network = network.clone();
        let args = args.clone();
        let admitted = self
            .store
            .call("work.accept_dispatch", move |store| {
                let network = call_network.as_str();
                if let Some(existing) = store.accepted_work(network, &request)? {
                    Ok(existing)
                } else {
                    if let Some(envelope) = store.peer_dispatch_envelope(network, &request)? {
                        authorize_peer_node_from(&store.connection, network, &request.dispatcher)?;
                        ensure!(
                            store.peer_command_dependencies(network, &envelope)?,
                            "dispatch dependencies are still missing"
                        );
                    }
                    ensure!(
                        !store.peer_cancelled(network, &request.conversation, &request.work_id)?,
                        AdmissionRejected::new("work was cancelled before dispatch admission")
                    );
                    let first = (|| {
                        let proposed =
                            Self::resolve_work_model(store, network, &request, &settings)?;
                        let kind = match request.kind {
                            RequestedWorkKind::Agent(_) => {
                                proposed.provider.check_work(&args)?;
                                WorkKind::Agent(proposed.provider)
                            }
                            RequestedWorkKind::Task => WorkKind::Task,
                        };
                        Self::authorize_dispatch(store, network, &request, kind, &settings)?;
                        store.accept_work(
                            &settings.identity,
                            network,
                            request.clone(),
                            proposed,
                            now_ms(),
                        )
                    })();
                    match first {
                        Ok(admitted) => Ok(admitted),
                        Err(error) => match store.accepted_work(network, &request)? {
                            Some(existing) => Ok(existing),
                            None => Err(error),
                        },
                    }
                }
            })
            .await?;
        let record = &admitted.record;
        tracing::info!(
            event = "work.dispatched",
            conversation_id = record.conversation,
            work_id = record.work_id,
            kind = ?record.kind,
            created = admitted.created,
            "work dispatched to this device"
        );
        self.publish_changed(
            &network,
            &record.conversation,
            Changed::Work(&record.work_id),
        );
        if admitted.created && record.kind == WorkKind::Task {
            self.wake_scheduler()?;
        }
        Ok(admitted)
    }

    fn resolve_work_model(
        store: &Store,
        network: &str,
        request: &WorkRequest,
        settings: &WorkDispatchSettings,
    ) -> Result<ResolvedModel> {
        let (provider, model) = match request.kind {
            RequestedWorkKind::Agent(agent) => {
                let provider = Self::execution_provider_from(
                    store,
                    network,
                    &settings.identity.device_id()?,
                    &settings.providers.execution,
                    settings.manager.as_ref().map(|manager| manager.provider),
                    ExecutionProviderRequest {
                        agent,
                        thread: request.thread.as_deref(),
                        continues: request.continues.as_deref(),
                    },
                )?;
                if let Some(thread) = &request.thread {
                    match Self::ran_on_from(
                        store,
                        network,
                        &settings.identity.device_id()?,
                        thread,
                    )? {
                        Some(ran) if ran != provider => bail!(
                            "this device's {} works ran in this session; set agent to {} or leave agent out",
                            ran.label(),
                            ran.name()
                        ),
                        None if !provider.work().any_thread => bail!(
                            "{} takes work only in sessions this device's works ran in; leave agent out for a Codex thread, and dispatch without thread for a session the user opened",
                            provider.label()
                        ),
                        _ => {}
                    }
                    ensure!(
                        store.native_thread_conversation(thread)?.is_none(),
                        "this thread is a bot's own reply thread; message that bot with message_bot instead"
                    );
                }
                let model = settings
                    .providers
                    .resolve_execution_model(provider, request.model.as_deref());
                (provider, model)
            }
            RequestedWorkKind::Task => {
                ensure!(
                    request.model.is_none(),
                    AdmissionRejected::new("a remote task uses the target Bot's model; omit model")
                );
                let manager = settings.manager.as_ref().ok_or_else(|| {
                    ExecFailure::new(
                        FailureKind::NoBot,
                        "this device has no bot to take the task",
                    )
                })?;
                let bot = store.bot_for_conversation(network, &request.conversation)?;
                let model = crate::scheduler::bot_model(manager, bot.as_ref())
                    .unwrap_or_else(|| manager.model.clone());
                (manager.provider, manager.provider.model(model.trim()))
            }
        };
        crate::model::validate_model(&model)?;
        Ok(ResolvedModel {
            provider,
            model: (!model.is_empty()).then_some(model),
        })
    }

    #[cfg(test)]
    pub(super) async fn accept_remote_dispatch(
        self: &Arc<Self>,
        network: &str,
        origin: &str,
        input: &Value,
    ) -> Result<Value> {
        let spec: WorkRequest = serde_json::from_value(input["spec"].clone())?;
        ensure!(
            spec.kind == RequestedWorkKind::Task,
            "another device takes work only as a task for its bot"
        );
        ensure!(
            spec.dispatcher == origin && spec.target == self.identity.device_id()?,
            "a dispatch names its sender and this device"
        );
        ensure!(
            spec.work_id == WorkSpec::id(&spec.conversation, &spec.trigger, &spec.call_ref),
            "work ID does not match its call"
        );
        let record = self
            .accept_dispatch(network, spec.clone(), &Value::Null)
            .await
            .map(|admitted| admitted.record);
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
        spec: WorkRequest,
        notify: bool,
    ) -> Result<Dispatch> {
        let dispatch = self
            .store
            .call("work.save_dispatch", move |store| {
                store.save_dispatch(&Dispatch {
                    spec,
                    state: DispatchState::Pending,
                })
            })
            .await?;
        if dispatch.state != DispatchState::Pending {
            return Ok(dispatch);
        }
        match self.offer_dispatch(network, &dispatch, notify).await? {
            Ok(answered) => Ok(answered),
            Err(error) => {
                tracing::debug!(%error, "dispatch waits for its target");
                Ok(dispatch)
            }
        }
    }

    /// Asks the target to take a pending dispatch and records its answer; the inner error is the
    /// request's when the target gave none, and the dispatch stays pending.
    async fn offer_dispatch(
        self: &Arc<Self>,
        network: &str,
        dispatch: &Dispatch,
        notify: bool,
    ) -> Result<Result<Dispatch>> {
        let lane = format!("dispatch:{network}:{}", dispatch.spec.work_id);
        let _peer_delivery = self.peer_delivery_locks.lock(&lane).await;
        let call_network = network.to_owned();
        let spec = dispatch.spec.clone();
        let command = self
            .store
            .call("work.dispatch_command", move |store| {
                store.peer_dispatch_command(&call_network, &spec)
            })
            .await?;
        let command_id = command.envelope.command_id.clone();
        let request = json!({"type":"dispatch_work","command":command.envelope});
        let reply = match self
            .peer_request(network, &command.target_node, request)
            .await
        {
            Ok(reply) => reply,
            Err(error) => return Ok(Err(error)),
        };
        ensure!(
            reply["admission"]["command_id"] == command_id,
            "dispatch answer belongs to another command"
        );
        let admission = reply["admission"]["state"]
            .as_str()
            .context("dispatch answer had no admission state")?;
        if matches!(admission, "saved" | "waiting_dependencies") {
            return Ok(Ok(dispatch.clone()));
        }
        let state = if admission == "owner_accepted"
            && reply["work_id"] == dispatch.spec.work_id.as_str()
        {
            DispatchState::Accepted
        } else {
            ensure!(admission == "rejected", "invalid dispatch admission answer");
            DispatchState::Rejected {
                reason: reply["rejected"]
                    .as_str()
                    .unwrap_or("the device refused the work")
                    .into(),
                failure: serde_json::from_value(reply["failure"].clone()).unwrap_or_default(),
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
        let call_network = network.to_owned();
        let local = self.identity.device_id()?;
        let call_work = spec.work_id.clone();
        let (dispatch, receipt) = self
            .store
            .call("work.answer_dispatch", move |store| {
                store.answer_dispatch(&call_network, &local, &call_work, state, notify)
            })
            .await?;
        self.publish_changed(
            network,
            &dispatch.spec.conversation,
            Changed::Work(&dispatch.spec.work_id),
        );
        if receipt.is_some() {
            self.wake_scheduler()?;
        }
        Ok(Ok(dispatch))
    }

    /// The Agent's report of its own send to the App Server: the thread before `turn/start`, then
    /// the turn, a refusal, or an unknown result that the Node settles by reading the thread.
    pub(super) async fn bind_work(
        self: &Arc<Self>,
        network: &str,
        scope: &str,
        work_id: &str,
        report: StartReport,
    ) -> Result<Value> {
        let record = {
            let call_work = work_id.to_owned();
            self.store
                .call("work.record", move |store| store.work_record(&call_work))
                .await
        }?
        .context("work not found on this device")?;
        ensure!(
            record.conversation == scope && on_app_server(&record),
            "work belongs to another conversation"
        );
        let execution = {
            let call_network = network.to_owned();
            let call_work = work_id.to_owned();
            self.store
                .call("work.native_execution", move |store| {
                    store.work_native_execution(&call_network, &call_work)
                })
                .await
        }?;
        if let Some(execution) = execution {
            let matches = match &report {
                StartReport::Thread { thread_id } => thread_id == &execution.thread_id,
                StartReport::Turn { turn_id } => {
                    turn_id == &execution.turn_id
                        && record.state.thread() == Some(execution.thread_id.as_str())
                }
                _ => true,
            };
            if !matches {
                self.signal_work(
                    network,
                    work_id,
                    Signal::SubmissionUnverified(
                        "native send report differs from its reserved turn".into(),
                    ),
                )
                .await?;
                bail!("native send report differs from its reserved turn; delivery is unknown");
            }
        }
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
                if record.state.handle().is_some()
                    && let Ok(probe @ Probe::Terminal { .. }) =
                        self.work_executor(&record)?.probe(&record).await
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
                record = {
                    let call_work = work_id.to_owned();
                    self.store
                        .call("work.record", move |store| store.work_record(&call_work))
                        .await
                }?
                .context("work not found on this device")?;
            }
        }
        Ok(json!({"work_id":work_id,"state":record.state}))
    }

    /// Reads the turn back to settle a work's state; it never starts or resends anything. Before
    /// the send window is over only a found turn is applied, so a late send is never written off.
    pub(super) async fn probe_work(
        self: &Arc<Self>,
        network: &str,
        work_id: &str,
        now: bool,
    ) -> Result<()> {
        let record = {
            let call_work = work_id.to_owned();
            self.store
                .call("work.record", move |store| store.work_record(&call_work))
                .await
        }?
        .context("work not found")?;
        if record.kind == WorkKind::Task {
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
        let probe = match &record.state {
            WorkState::Accepted | WorkState::Starting { thread: None } => {
                if over {
                    self.signal_work(
                        network,
                        work_id,
                        Signal::StartRejected(ExecFailure::new(
                            FailureKind::TimedOut,
                            "this work was not started in time",
                        )),
                    )
                    .await?;
                }
                return Ok(());
            }
            state if state.thread().is_none() => return Ok(()),
            _ => self.work_executor(&record)?.probe(&record).await,
        };
        let execution = {
            let call_network = network.to_owned();
            let call_work = work_id.to_owned();
            self.store
                .call("work.native_execution", move |store| {
                    store.work_native_execution(&call_network, &call_work)
                })
                .await
        }?;
        if let Some(execution) = execution {
            let turn = match &probe {
                Ok(Probe::Running { turn_id, .. } | Probe::Terminal { turn_id, .. }) => {
                    Some(turn_id)
                }
                _ => None,
            };
            if turn.is_some_and(|turn| turn != &execution.turn_id) {
                self.signal_work(
                    network,
                    work_id,
                    Signal::SubmissionUnverified(
                        "native readback differs from its reserved turn".into(),
                    ),
                )
                .await?;
                return Ok(());
            }
        }
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
            Err(error) => Err(error),
        }
    }

    /// Settles works the Agent registered but did not finish starting within its window, reads
    /// the turns of running and unverified works about once a minute, and carries out the stops
    /// this device holds for its own works. A probe reads provider state and never starts or resends work.
    pub(super) async fn reconcile_works(self: &Arc<Self>, network: &str) -> Result<()> {
        use std::sync::atomic::Ordering::Relaxed;
        let at = now_ms();
        let last = self.live_probe_at.load(Relaxed);
        let read_live = at - last >= LIVE_PROBE_MS
            && self
                .live_probe_at
                .compare_exchange(last, at, Relaxed, Relaxed)
                .is_ok();
        let records = self
            .store
            .call("work.reconcile_records", move |store| {
                store.reconcile_work_records(at, read_live)
            })
            .await?;
        for record in records {
            if let Err(error) = self.probe_work(network, &record.work_id, false).await {
                tracing::debug!(%error, "work probe waits for its provider");
            }
        }
        let local = self.identity.device_id()?;
        let stops = self
            .store
            .call("work.local_stops", move |store| {
                store.stop_requests_for_target(&local)
            })
            .await?;
        for request in stops {
            let work_id = request.work_id.clone();
            if let Err(error) = self.reconcile_stop(network, request).await {
                tracing::debug!(%error, work_id, "stop waits for its provider");
            }
        }
        Ok(())
    }

    /// Whether the user stopped this device's work and the work has not ended yet.
    async fn waiting_stop(&self, work_id: &str) -> Result<bool> {
        let local = self.identity.device_id()?;
        let work_id = work_id.to_owned();
        self.store
            .call("work.waiting_stop", move |store| {
                Ok(store
                    .stop_request(&work_id)?
                    .is_some_and(|request| request.target == local))
            })
            .await
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
        let _input = self
            .work_input_lock(&request.work_id)
            .await
            .write_owned()
            .await;
        let request = request.clone();
        self.store
            .call("work.hold_stop", move |store| {
                store.hold_stop_request(&request)
            })
            .await
    }

    async fn drop_stop(&self, work_id: &str) -> Result<()> {
        let call_work = work_id.to_owned();
        self.store
            .call("work.forget_stop", move |store| {
                store.forget_stop_request(&call_work)
            })
            .await?;
        self.stop_retries
            .lock()
            .expect("stop retries")
            .remove(work_id);
        Ok(())
    }

    /// Asks the provider to stop a held stop's turn. Only its answer moves the work to
    /// interrupting and starts the deadline; a failed or unknown result leaves the stop to the
    /// next reconcile, which reads the turn before asking again.
    async fn interrupt_turn(
        self: &Arc<Self>,
        network: &str,
        record: &WorkRecord,
    ) -> Result<WorkRecord> {
        let work_id = record.work_id.as_str();
        self.work_executor(record)?.interrupt(record).await?;
        self.stop_retries
            .lock()
            .expect("stop retries")
            .remove(work_id);
        let call_work = work_id.to_owned();
        self.store
            .call("work.interrupt_deadline", move |store| {
                let work_id = call_work.as_str();
                // A turn that outlived an interrupt the provider took is still being stopped.
                let again = store
                    .work_record(work_id)?
                    .is_some_and(|record| matches!(record.state, WorkState::Interrupting { .. }));
                if let Some(mut request) = store.stop_request(work_id)? {
                    request.deadline_ms = now_ms() + INTERRUPT_TIMEOUT.as_millis() as i64;
                    request.timed_out = again;
                    store.save_stop_request(&request)?;
                }
                Ok(())
            })
            .await?;
        self.signal_work(network, work_id, Signal::InterruptRequested)
            .await
    }

    /// Carries out a stop this device holds for its own work until the work ends. Before asking
    /// again it reads the turn, so a turn that has ended is settled and never interrupted. An
    /// interrupt the provider took that has not ended the turn by the deadline is sent again;
    /// one whose turn cannot be read then leaves the work unverified.
    async fn reconcile_stop(
        self: &Arc<Self>,
        network: &str,
        mut request: StopRequest,
    ) -> Result<()> {
        let work_id = request.work_id.clone();
        let record = {
            let call_work = work_id.to_owned();
            self.store
                .call("work.record", move |store| store.work_record(&call_work))
                .await
        }?;
        let Some(record) = record.filter(live) else {
            self.drop_stop(&work_id).await?;
            self.publish_changed(network, &request.conversation, Changed::Work(&work_id));
            return Ok(());
        };
        // pi may still wait for get_state before it has a turn to interrupt.
        if record.state.handle().is_none() {
            if record.kind == WorkKind::Agent(Provider::Pi) && self.stop_attempt_due(&work_id) {
                let _input = self.work_input_lock(&work_id).await.write_owned().await;
                tokio::time::timeout(
                    STOP_ATTEMPT_TIMEOUT,
                    self.work_executor(&record)?.interrupt(&record),
                )
                .await
                .context("pi preparation stop timed out")??;
            }
            return Ok(());
        }
        let at = now_ms();
        if !request.timed_out && at >= request.deadline_ms {
            request.timed_out = true;
            let held = request.clone();
            self.store
                .call("work.update_stop", move |store| {
                    store.save_stop_request(&held)
                })
                .await?;
            self.publish_changed(network, &request.conversation, Changed::Work(&work_id));
        }
        let interrupting = matches!(record.state, WorkState::Interrupting { .. });
        if (interrupting && at < request.deadline_ms) || !self.stop_attempt_due(&work_id) {
            return Ok(());
        }
        let probe = self.work_executor(&record)?.probe(&record);
        match tokio::time::timeout(STOP_ATTEMPT_TIMEOUT, probe).await {
            Ok(Ok(Probe::Running { turn_id, epoch })) => {
                if matches!(record.state, WorkState::Unverified { .. }) {
                    let running = Probe::Running { turn_id, epoch };
                    self.signal_work(network, &work_id, Signal::Probed(running))
                        .await?;
                }
                self.interrupt_turn(network, &record).await?;
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
        let record = {
            let call_work = work_id.to_owned();
            self.store
                .call("work.record", move |store| store.work_record(&call_work))
                .await
        }?
        .context("work not found")?;
        let mut waiting_for_start = false;
        let mut retrying = false;
        match &record.state {
            WorkState::Accepted if record.kind == WorkKind::Task => {
                let identity = self.identity.clone();
                let call_network = network.to_owned();
                let conversation = record.conversation.clone();
                let trigger = task_trigger(work_id);
                let job = self
                    .store
                    .call("work.cancel_task", move |store| {
                        store.request_manager_cancel(
                            &identity,
                            &call_network,
                            &conversation,
                            &trigger,
                        )
                    })
                    .await?;
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
                if record.kind != WorkKind::Task =>
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
                    WorkState::Running { .. } => {
                        retrying = !(held
                            && self.stop_attempt_due(work_id)
                            && Box::pin(self.interrupt_turn(network, &record))
                                .await
                                .inspect_err(|error| {
                                    tracing::warn!(event = "work.stop_pending", %error, work_id, "stop waits for the next reconcile")
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
        let call_network = network.to_owned();
        let conversation = record.conversation.clone();
        let call_work = work_id.to_owned();
        let children = self
            .store
            .call("work.children", move |store| {
                let network = call_network.as_str();
                let work_id = call_work.as_str();
                let mut children = Vec::new();
                for work in store.works(network, &conversation)? {
                    if work.parent.as_deref() == Some(work_id)
                        && store
                            .work_record(&work.work_id)?
                            .is_some_and(|child| live(&child))
                    {
                        children.push(work.work_id);
                    }
                }
                Ok(children)
            })
            .await?;
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
        let call_network = network.to_owned();
        let call_scope = scope.to_owned();
        let call_work = work_id.to_owned();
        let target = self
            .store
            .call("work.stop_target", move |store| {
                let work_target = store
                    .work(&call_network, &call_work)?
                    .filter(|work| work.conversation_id == call_scope)
                    .map(|work| work.target_node);
                work_target
                    .or(store
                        .dispatch(&call_work)?
                        .filter(|dispatch| {
                            dispatch.spec.conversation == call_scope
                                && dispatch.state == DispatchState::Pending
                        })
                        .map(|dispatch| dispatch.spec.target))
                    .context("work not found in this conversation")
            })
            .await?;
        if target == self.identity.device_id()? {
            return self.interrupt_work(network, work_id).await;
        }
        let at = now_ms();
        let request = StopRequest {
            work_id: work_id.into(),
            conversation: scope.into(),
            target,
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
                    if reply["admission"]["state"] == "owner_accepted" {
                        self.drop_stop(work_id).await?;
                    }
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
        let call_network = network.to_owned();
        let held = request.clone();
        let command = self
            .store
            .call("work.stop_command", move |store| {
                store.peer_stop_command(&call_network, &held)
            })
            .await?;
        let command_id = command.envelope.command_id.clone();
        let reply = self
            .peer_request(
                network,
                &command.target_node,
                json!({"type":"interrupt_work","command":command.envelope}),
            )
            .await?;
        ensure!(
            reply["admission"]["command_id"] == command_id && reply["work_id"] == request.work_id,
            "stop answer belongs to another command"
        );
        let state = reply["admission"]["state"]
            .as_str()
            .context("stop answer had no admission state")?;
        ensure!(
            matches!(state, "saved" | "waiting_dependencies" | "owner_accepted"),
            "stop was not accepted: {}",
            reply["rejected"].as_str().unwrap_or(state)
        );
        Ok(reply)
    }

    /// Retries waiting dispatches and the stops held for other devices' works.
    pub(super) async fn deliver_work_requests(self: &Arc<Self>, network: &str) -> Result<()> {
        let dispatches = self
            .store
            .call("work.pending_dispatches", |store| {
                store.pending_dispatches()
            })
            .await?;
        for dispatch in dispatches {
            let lane = format!("dispatch_worker:{network}:{}", dispatch.spec.work_id);
            let Some(guard) = self.peer_delivery_locks.try_lock(&lane) else {
                continue;
            };
            let (node, network) = (self.clone(), network.to_owned());
            tokio::spawn(async move {
                let _guard = guard;
                if let Err(error) = node
                    .offer_dispatch(&network, &dispatch, true)
                    .await
                    .and_then(|answer| answer.map(drop))
                {
                    tracing::debug!(%error, "dispatch waits for its target");
                }
            });
        }
        let local = self.identity.device_id()?;
        let stops = self
            .store
            .call("work.remote_stops", |store| store.stop_requests())
            .await?;
        for request in stops.into_iter().filter(|request| request.target != local) {
            let lane = format!("stop_worker:{network}:{}", request.work_id);
            let Some(guard) = self.peer_delivery_locks.try_lock(&lane) else {
                continue;
            };
            let (node, network) = (self.clone(), network.to_owned());
            tokio::spawn(async move {
                let _guard = guard;
                if let Err(error) = node.deliver_held_stop(&network, request).await {
                    tracing::debug!(%error, "stop waits for its target");
                }
            });
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
        let call_network = network.to_owned();
        let conversation = request.conversation.clone();
        let call_work = work_id.clone();
        let ended = self
            .store
            .call("work.stop_ended", move |store| {
                Ok(store.work(&call_network, &call_work)?.is_some_and(|work| {
                    work.conversation_id == conversation && work.outcome.is_some()
                }) || store
                    .dispatch(&call_work)?
                    .is_some_and(|dispatch| dispatch.state.ended()))
            })
            .await?;
        if !ended && !request.timed_out && now_ms() >= request.deadline_ms {
            request.timed_out = true;
            let held = request.clone();
            self.store
                .call("work.update_stop", move |store| {
                    store.save_stop_request(&held)
                })
                .await?;
            self.publish_changed(network, &request.conversation, Changed::Work(&work_id));
        }
        if !ended {
            if !self.stop_attempt_due(&work_id) {
                return Ok(());
            }
            let reply =
                tokio::time::timeout(STOP_ATTEMPT_TIMEOUT, self.deliver_stop(network, &request))
                    .await
                    .map_err(|_| {
                        crate::net::Unanswered::error("stop request timed out".into())
                    })??;
            if reply["admission"]["state"] != "owner_accepted" {
                return Ok(());
            }
        }
        self.drop_stop(&work_id).await?;
        self.publish_changed(network, &request.conversation, Changed::Work(&work_id));
        Ok(())
    }

    /// A new connection gets the next epoch and has every live work read again; a lost one
    /// leaves the running works waiting for that read.
    async fn on_connection(self: &Arc<Self>, connected: bool) -> Result<()> {
        if connected {
            let epoch = self
                .store
                .call("work.codex_epoch", |store| store.next_codex_epoch())
                .await;
            self.codex
                .connected(*epoch.as_ref().unwrap_or(&self.codex.epoch()));
            epoch?;
        } else {
            self.drop_codex_requests().await?;
        }
        let Some(network) = self
            .store
            .call("work.network", |store| store.network())
            .await?
        else {
            return Ok(());
        };
        let records = self
            .store
            .call("work.records", |store| store.open_work_records())
            .await?;
        for record in records
            .into_iter()
            .filter(|record| live(record) && on_app_server(record))
        {
            if connected {
                if let Err(error) = self.probe_work(&network, &record.work_id, false).await {
                    tracing::debug!(%error, "work probe after reconnect failed");
                }
            } else if record.state.handle().is_some()
                && record
                    .state
                    .thread()
                    .is_none_or(|thread| self.codex.user_thread(thread).is_none())
            {
                self.signal_work(&network, &record.work_id, Signal::Disconnected)
                    .await?;
            }
        }
        // The probes found which threads are on the user's own Codex.
        if connected {
            self.observe_live_threads().await;
        }
        Ok(())
    }

    /// Applies an App Server notification to the works whose turn it is about.
    async fn route_turn_event(self: &Arc<Self>, network: &str, event: Value) -> Result<()> {
        let turn = codex_server::turn_event(&event);
        let Some(thread) = turn.thread else {
            return Ok(());
        };
        let call_thread = thread.to_owned();
        let records = self
            .store
            .call("work.records_on_thread", move |store| {
                store.work_records_on_thread(&call_thread)
            })
            .await?;
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
                    ) && record.state.thread() == Some(thread)
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
                record
                    .state
                    .handle()
                    .is_some_and(|h| h.thread_id == thread && Some(h.turn_id.as_str()) == turn.turn)
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

    pub(super) fn start_codex(self: &Arc<Self>, mut events: vibeface_bridge::codex::EventReceiver) {
        let node = self.clone();
        if let Err(error) = self.background_tasks.spawn(async move {
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
                        let network = node.store.call("work.network", |store| store.network()).await;
                        match network {
                            Ok(Some(network)) => node.route_turn_event(&network, message).await,
                            other => other.map(|_| ()),
                        }
                    }
                };
                if let Err(error) = applied {
                    tracing::warn!(event = "app_server.event_failed", %error, "could not apply an App Server event");
                }
            }
        }) {
            tracing::debug!(%error, "App Server event worker skipped while the Node is shutting down");
        }
    }
}
