//! Owns a claimed scheduler attempt, including every provider round and its explicit cleanup.
use super::*;
use crate::{
    scheduling::{ClaimedJob, ManagerAttempt},
    store::scheduler::SchedulerBudgetReservation,
};
use anyhow::Context as _;
use tokio::sync::watch;
use vibeface_bridge::agent::AgentResult;

pub(super) struct TurnAttempt {
    node: Arc<Runtime>,
    network: String,
    conversation: String,
    scope: SessionScope,
    attempt: ManagerAttempt,
    job: SchedulerJob,
    claimed: ClaimedJob,
    environment: TurnEnvironment,
    bot_model: Option<String>,
    phase: TurnPhase,
    budget: Option<SchedulerBudgetReservation>,
    partial: Arc<Mutex<PartialText>>,
    progress_sequence: Arc<std::sync::atomic::AtomicU64>,
}

struct TurnEnvironment {
    permission_epoch: u64,
    local_settings: crate::runtime::NodeSettings,
    provider_local: crate::providers::Local,
}

struct ProviderRound {
    settings: crate::config::ManagerSettings,
    stable: StableInstructions,
    config: ManagerConfig,
    server: String,
    session_config: SessionConfig,
    token: String,
}

/// Local publication follows registration and budget admission. It is not Provider Started.
#[derive(Clone, Copy)]
enum Activation {
    Pending,
    Published,
}

struct ToolRegistration {
    cancel: watch::Receiver<bool>,
    access: ToolAccess,
}

enum ToolAccess {
    Open,
    Fenced,
}

struct RegisteredRound {
    round: ProviderRound,
    registration: ToolRegistration,
}

enum TurnPhase {
    Unregistered {
        round: ProviderRound,
        activation: Activation,
    },
    Registered {
        registered: RegisteredRound,
        activation: Activation,
    },
    /// Retained after ManagerError: no successful SDK return or completed backup close yet.
    Submitted(RegisteredRound),
    Returned {
        registered: RegisteredRound,
        result: AgentResult,
    },
    /// The old registration is closed and its launch config has been dropped.
    Switching { activation: Activation },
}

impl TurnPhase {
    fn round(&self) -> Result<&ProviderRound> {
        match self {
            Self::Unregistered { round, .. } => Ok(round),
            Self::Registered { registered, .. }
            | Self::Submitted(registered)
            | Self::Returned { registered, .. } => Ok(&registered.round),
            Self::Switching { .. } => bail!("no provider round while switching"),
        }
    }

    fn round_mut(&mut self) -> Result<&mut ProviderRound> {
        match self {
            Self::Unregistered { round, .. } => Ok(round),
            Self::Registered { registered, .. }
            | Self::Submitted(registered)
            | Self::Returned { registered, .. } => Ok(&mut registered.round),
            Self::Switching { .. } => bail!("no provider round while switching"),
        }
    }

    fn activation(&self) -> Activation {
        match self {
            Self::Unregistered { activation, .. }
            | Self::Registered { activation, .. }
            | Self::Switching { activation } => *activation,
            Self::Submitted(_) | Self::Returned { .. } => Activation::Published,
        }
    }

    fn locally_activated(&self) -> bool {
        matches!(self.activation(), Activation::Published)
    }

    fn unresolved(&self) -> bool {
        matches!(self, Self::Submitted(_))
    }

    fn registration_mut(&mut self) -> Option<&mut ToolRegistration> {
        match self {
            Self::Registered { registered, .. }
            | Self::Submitted(registered)
            | Self::Returned { registered, .. } => Some(&mut registered.registration),
            Self::Unregistered { .. } | Self::Switching { .. } => None,
        }
    }

    fn tools_fenced(&self) -> bool {
        match self {
            Self::Registered { registered, .. }
            | Self::Submitted(registered)
            | Self::Returned { registered, .. } => {
                matches!(registered.registration.access, ToolAccess::Fenced)
            }
            Self::Unregistered { .. } | Self::Switching { .. } => true,
        }
    }

    // Moving a phase is synchronous: no await, callback, or fallible work can strand resources
    // in the temporary Switching value. Unexpected transitions restore the original owner.
    fn register(&mut self, cancel: watch::Receiver<bool>) -> Result<()> {
        let activation = self.activation();
        match std::mem::replace(self, Self::Switching { activation }) {
            Self::Unregistered { round, activation } => {
                *self = Self::Registered {
                    registered: RegisteredRound {
                        round,
                        registration: ToolRegistration {
                            cancel,
                            access: ToolAccess::Open,
                        },
                    },
                    activation,
                };
                Ok(())
            }
            previous => {
                *self = previous;
                bail!("provider round already registered")
            }
        }
    }

    fn activate(&mut self) -> Result<()> {
        match self {
            Self::Registered { activation, .. } => {
                *activation = Activation::Published;
                Ok(())
            }
            _ => bail!("local activation requires a registered provider round"),
        }
    }

    fn submit(&mut self) -> Result<watch::Receiver<bool>> {
        let activation = self.activation();
        match std::mem::replace(self, Self::Switching { activation }) {
            Self::Registered {
                registered,
                activation: Activation::Published,
            } => {
                let cancel = registered.registration.cancel.clone();
                *self = Self::Submitted(registered);
                Ok(cancel)
            }
            previous => {
                *self = previous;
                bail!("native submission requires local activation")
            }
        }
    }

    fn returned(&mut self, result: AgentResult) -> Result<()> {
        let activation = self.activation();
        match std::mem::replace(self, Self::Switching { activation }) {
            Self::Submitted(registered) => {
                *self = Self::Returned { registered, result };
                Ok(())
            }
            previous => {
                *self = previous;
                bail!("SDK return requires a submitted provider round")
            }
        }
    }

    fn result(&self) -> Result<&AgentResult> {
        match self {
            Self::Returned { result, .. } => Ok(result),
            _ => bail!("provider round has no successful SDK result"),
        }
    }

    fn begin_switch(&mut self) {
        let activation = self.activation();
        *self = Self::Switching { activation };
    }
}

enum ProviderOutcome {
    Returned(AgentResult),
    Failed(anyhow::Error),
}

struct TurnInput {
    prompt: String,
    shown: Arc<Mutex<Shown>>,
    fresh_prompt: Option<String>,
    fresh_shown: Option<Shown>,
    reply_only: bool,
}

/// Constructed only after Store.finish_job returned its committed job.
pub(super) struct RecordedReply(Value);

impl TurnAttempt {
    pub(super) async fn claim(
        node: Arc<Runtime>,
        network: String,
        conversation: String,
        scope: SessionScope,
    ) -> Result<Option<Self>> {
        let PreparedTurn {
            permission_epoch,
            settings,
            local_settings,
            provider_local,
            bot_model,
            stable,
            config,
            server,
            session_config,
        } = prepare_turn(&node, &network, &conversation, &scope).await?;
        let provider = session_config.provider;
        if node.provider_configuration_pending(provider, &provider_local) {
            return Ok(None);
        }
        let Some(claimed) = ({
            let identity = node.identity.clone();
            let network = network.clone();
            let session = SessionKey {
                conversation_id: conversation.clone(),
                scope: scope.clone(),
            };
            let session_config = session_config.clone();
            let limits = node.config.scheduling;
            node.store
                .call("scheduler.claim", move |store| {
                    store.authorize_node(&network, &identity.device_id()?)?;
                    store.claim_next_in_session(
                        &identity,
                        &network,
                        &session,
                        boot_id(),
                        &session_config,
                        &limits,
                    )
                })
                .await?
        }) else {
            return Ok(None);
        };

        let job = claimed.job.clone();
        let trigger = &job.trigger;
        let attempt = job
            .state
            .attempt()
            .context("claimed scheduler attempt missing")?
            .clone();
        if let Some(work_id) = match crate::scheduling::Trigger::decode(trigger).ok() {
            Some(crate::scheduling::Trigger::Work(id) | crate::scheduling::Trigger::Task(id)) => {
                Some(id)
            }
            _ => None,
        } {
            tracing::Span::current().record("work_id", work_id);
        }
        tracing::info!(event = "agent.turn.started", trigger, attempt = attempt.id, kind = ?job.kind, provider = ?provider, "agent turn started");
        // A resumed thread's Shell keeps its token, effective only during registration.
        let token = if provider.instance_per_bot() {
            uuid::Uuid::new_v4().to_string()
        } else {
            claimed
                .previous_token
                .clone()
                .unwrap_or_else(|| uuid::Uuid::new_v4().to_string())
        };
        Ok(Some(Self {
            node,
            network,
            conversation,
            scope,
            attempt,
            job,
            claimed,
            environment: TurnEnvironment {
                permission_epoch,
                local_settings,
                provider_local,
            },
            bot_model,
            phase: TurnPhase::Unregistered {
                round: ProviderRound {
                    settings,
                    stable,
                    config,
                    server,
                    session_config,
                    token,
                },
                activation: Activation::Pending,
            },
            budget: None,
            partial: Arc::new(Mutex::new(PartialText::default())),
            progress_sequence: Arc::new(std::sync::atomic::AtomicU64::new(0)),
        }))
    }

    pub(super) async fn execute(&mut self) -> Result<RecordedReply> {
        self.record_initial_runtime().await?;
        #[cfg(test)]
        if self.job.kind == TriggerKind::AgentTask {
            let barrier = self
                .node
                .claim_to_start_barrier
                .lock()
                .expect("claim barrier")
                .take();
            if let Some((claimed, resume)) = barrier {
                claimed.notify_one();
                resume.notified().await;
            }
        }
        loop {
            let input = self.compile_context().await?;
            let confirmed_shown = input.shown.clone();
            match self.run_round(input).await? {
                ProviderOutcome::Returned(result) => {
                    self.phase.returned(result)?;
                    return self.record_success(&confirmed_shown).await;
                }
                ProviderOutcome::Failed(error) => {
                    let known =
                        record_failure(&self.node, &self.job, &error, TurnExecution::Unknown)
                            .await?;
                    let round = self.phase.round()?;
                    let provider = round.session_config.provider;
                    let backup = error
                        .downcast_ref::<ManagerError>()
                        .filter(|_| {
                            self.job.kind != TriggerKind::AgentTask
                                && !matches!(self.scope, SessionScope::Task { .. })
                        })
                        .and_then(|error| {
                            round.settings.backup.candidate_for_job(
                                &round.settings,
                                &known,
                                error,
                                !self.partial.lock().expect("partial text").text().is_empty(),
                            )
                        })
                        .filter(|backup| {
                            backup.provider == provider
                                || self
                                    .environment
                                    .provider_local
                                    .switches
                                    .execution
                                    .contains(&backup.provider)
                        })
                        .cloned();
                    let Some(backup) = backup else {
                        return Err(error);
                    };
                    let previous = known
                        .runtime
                        .as_ref()
                        .context("turn runtime missing")?
                        .clone();
                    let reason = error
                        .downcast_ref::<ManagerError>()
                        .context("backup requires structured failure")?
                        .failure
                        .kind;
                    self.switch_provider(backup, previous, reason).await?;
                }
            }
        }
    }

    async fn record_initial_runtime(&mut self) -> Result<()> {
        let round = self.phase.round()?;
        let provider = round.session_config.provider;
        let runtime = TurnRuntime {
            provider,
            model: (!round.config.model.is_empty()).then(|| round.config.model.clone()),
            reasoning_effort: round.config.reasoning_effort.clone(),
            generation: self
                .node
                .provider_generation(provider, &self.environment.provider_local)?,
            session_generation: self.claimed.session_generation,
            delivery_id: self.attempt.id.clone(),
            stage: ProviderStage::Validate,
            execution: TurnExecution::NotStarted,
            attempted_at_ms: crate::clock::now_ms(),
            switched_from: None,
            switch_reason: None,
        };
        let (conversation, trigger, attempt_id) = (
            self.conversation.clone(),
            self.job.trigger.clone(),
            self.attempt.id.clone(),
        );
        self.job = self
            .node
            .store
            .call("scheduler.record_turn_runtime", move |store| {
                store.record_turn_runtime(&conversation, &trigger, &attempt_id, &runtime)
            })
            .await?;
        Ok(())
    }

    async fn compile_context(&self) -> Result<TurnInput> {
        let (node, network, conversation, job, attempt, claimed) = (
            &self.node,
            &self.network,
            &self.conversation,
            &self.job,
            &self.attempt,
            &self.claimed,
        );
        let trigger = &job.trigger;
        let round = self.phase.round()?;
        let (session_config, stable, config) =
            (&round.session_config, &round.stable, &round.config);
        let local_settings = &self.environment.local_settings;
        let provider = session_config.provider;
        let thread = provider.prompt_mode() == crate::providers::PromptMode::Thread;
        let local = node.identity.device_id()?;
        // A resumed thread the backend no longer has is replaced by a new one, which needs it all.
        let wants_fresh = thread && claimed.previous_native_id.is_some();
        let (mut history, mut fresh_history) = {
            let network = network.clone();
            let conversation = conversation.clone();
            let job = job.clone();
            let covers = claimed.covers.clone();
            node.store
                .call("scheduler.turn_history", move |store| {
                    let resumed = covers.as_ref().map(|covers| (covers, local.as_str()));
                    Ok((
                        store.turn_history(&network, &conversation, &job, resumed)?,
                        wants_fresh
                            .then(|| store.turn_history(&network, &conversation, &job, None))
                            .transpose()?,
                    ))
                })
                .await?
        };
        let attachment_events = {
            let network = (network).to_owned();
            let message_id = (job.reply_to).to_owned();
            node.store
                .call("scheduler.input_attachment_events", move |store| {
                    store.input_attachment_events(&network, &message_id)
                })
                .await
        }?;
        let pending = node
            .fetch_turn_attachments(
                network,
                history
                    .shown_events()
                    .chain(
                        fresh_history
                            .iter()
                            .flat_map(|history| history.shown_events()),
                    )
                    .chain(attachment_events.iter()),
            )
            .await;
        history.pending_blobs = pending.clone();
        if let Some(fresh) = &mut fresh_history {
            fresh.pending_blobs = pending;
        }
        let (works, dispatches, mut bot, memories, schedule, pending_checks) = {
            let network = network.clone();
            let conversation = conversation.clone();
            let job = job.clone();
            let trigger = trigger.clone();
            node.store.call("scheduler.context_snapshot", move |store| {
        let works = store.works(&network, &conversation)?.into_iter().filter_map(|work| match store.scheduler_work_in_scope(&network,&job,&work.work_id) {
            Ok(true) => Some(Ok(work)), Ok(false) => None, Err(error) => Some(Err(error)),
        }).collect::<Result<Vec<_>>>()?;
        let dispatches = store.undelivered_dispatches(&network, &conversation)?.into_iter().filter_map(|dispatch| match store.scheduler_work_in_scope(&network,&job,&dispatch.spec.work_id) {
            Ok(true) => Some(Ok(dispatch)), Ok(false) => None, Err(error) => Some(Err(error)),
        }).collect::<Result<Vec<_>>>()?;
        let mut bot = bot_value(store, &network, &conversation, crate::clock::now_ms())?;
        if let Some(sessions)=bot["sessions"].as_array_mut() {
            let mut filtered=Vec::new();
            for session in sessions.drain(..) {
                if store.scheduler_native_in_scope(&network,&job,session["session_id"].as_str().unwrap_or(""))? { filtered.push(session); }
            }
            *sessions=filtered;
        }
        if job.scope!=SessionScope::Main { bot["sent"]=json!([]); bot["sent_omitted"]=json!(0); }
        let memories: Vec<_> = store.memory_records(&network, &conversation)?
            .into_iter().filter(|record| record.essential)
            .map(|record| MemoryContextEntry { memory_id: record.entry.memory_id,
                text: record.entry.text, epoch: record.epoch, revision: record.revision })
            .collect();
        let schedule = match job.kind {
            TriggerKind::Schedule => Some(
                store
                    .schedule(&network, crate::scheduling::parse_schedule_trigger(&trigger).context("invalid schedule trigger")?.0)?
                    .context("the schedule was deleted")?,
            ),
            _ => None,
        };
        let pending_checks: Vec<_> = store.pending_checks()?.into_iter()
        .filter(|check| check.conversation_id == conversation)
        .filter(|check| store.scheduler_job(&conversation,&check.trigger).ok().flatten().is_some_and(|source| source.scope == job.scope))
        .map(|check| json!({"check_id":check.check_id,"trigger":check.trigger,
            "detail":if crate::model::looks_secret(&check.detail) { "[redacted]" } else { prefix(&check.detail, 1_000) },
            "created_at_ms":check.created_at_ms}))
        .collect();
        Ok((works, dispatches, bot, memories, schedule, pending_checks))
    }).await?
        };
        bot["configured_bot"] = stable.bot.clone();
        bot["skills"] = json!(stable.skills);
        bot["skills_omitted"] = json!(0);
        fit_bot(&mut bot, INDEX_BUDGET);
        let ((prompt, shown_now), fresh) = {
            let uses_handoff =
                node.compact_or_handoff(provider)? == crate::providers::CompactionMode::Handoff;
            let handoff = claimed.invalidation.as_ref().filter(|value| uses_handoff && value.source_provider.is_some()).map(|invalidation| {
        agent_context::ContextHandoff {
            schema_version: agent_context::SCHEMA_VERSION,
            source_provider: invalidation.source_provider.expect("existing session provider"),
            target_provider: invalidation.target_provider,
            basis: invalidation.source_basis.clone(),
            source_fingerprint: invalidation.old_stable_fingerprint.clone().unwrap_or_default(),
            generation: invalidation.generation,
            handoff_revision: invalidation.handoff_revision.clone(),
            recent_context: "The new provider session rebuilds from the recorded DAG basis. Use context to read older events.".into(),
            open_work: works.iter().filter(|work| work.outcome.is_none()).map(|work| work.work_id.clone()).collect(),
            decisions: vec![],
            reason: invalidation.reason.clone(),
        }
    });
            let unseen = Shown {
                session_generation: claimed.shown.session_generation,
                ..Shown::default()
            };
            let adapter_prefix = match stable.provider.stable_prompt {
                crate::providers::StablePromptPlacement::UserTurnPrefix => {
                    manager::bot_instructions(
                        config.tool_cli.as_deref(),
                        &config.custom_tools,
                        &stable.rendered,
                    )
                    .len()
                        + 2
                }
                _ => 0,
            };
            let stable_budget = if thread {
                adapter_prefix
            } else {
                stable.rendered.len() + 2
            };
            let dynamic_limit = PROMPT_LIMIT.saturating_sub(stable_budget);
            ensure!(
                dynamic_limit > 0,
                "compiled stable instructions exceed the provider input limit"
            );
            let render = |history: &TurnHistory, seen: Option<&Shown>| -> Result<_> {
                let (dynamic, mut shown) = compile_dynamic_with_limit(
                    history,
                    job,
                    &works,
                    &dispatches,
                    bot.clone(),
                    schedule.as_ref(),
                    &local_settings.roots,
                    seen,
                    dynamic_limit,
                    DynamicExtras {
                        handoff: handoff.as_ref(),
                        pending_checks: &pending_checks,
                        session_generation: claimed.shown.session_generation,
                        rules_in_stable: true,
                        memories: Some(&memories),
                    },
                )?;
                shown.dynamic = Some(dynamic.fingerprint.clone());
                let (sources, sources_omitted) = context_sources(&dynamic, history)?;
                shown.context = Some(crate::scheduling::ContextProjection {
                    network_id: network.clone(),
                    scope: job.scope.clone(),
                    attempt_id: attempt.id.clone(),
                    provider,
                    stable_fingerprint: stable.fingerprint.digest.clone(),
                    dynamic_fingerprint: dynamic.fingerprint.clone(),
                    session_generation: shown.session_generation,
                    coverage: dynamic.coverage.clone(),
                    stable_bytes: stable.rendered.len(),
                    dynamic_bytes: dynamic.rendered.len(),
                    invalidation_reason: claimed
                        .invalidation
                        .as_ref()
                        .map(|value| value.reason.clone()),
                    stage: crate::scheduling::ContextStage::Compiled,
                    session_status: None,
                    sources,
                    sources_omitted,
                    inputs: vec![],
                    inputs_omitted: 0,
                });
                let compiled = CompiledTurn::new(stable.clone(), dynamic, provider.prompt_mode());
                let prompt = compiled.prompt();
                ensure!(
                    prompt.len() + adapter_prefix <= PROMPT_LIMIT,
                    "compiled prompt exceeds the provider input limit"
                );
                Ok((prompt, shown))
            };
            let (prompt, shown) = render(&history, thread.then_some(&claimed.shown))?;
            let fresh = fresh_history
                .as_ref()
                .map(|history| render(history, Some(&unseen)))
                .transpose()?;
            ((prompt, shown), fresh)
        };
        {
            let conversation = (conversation).to_owned();
            let trigger = (trigger).to_owned();
            let attempt_id = (attempt.id).to_owned();
            let context = (shown_now
                .context
                .as_ref()
                .context("compiled context projection missing")?)
            .to_owned();
            node.store
                .call("scheduler.record_compiled_context", move |store| {
                    store.record_compiled_context(&conversation, &trigger, &attempt_id, &context)
                })
                .await
        }?;
        let (fresh_prompt, fresh_shown) = fresh.unzip();
        Ok(TurnInput {
            prompt,
            shown: Arc::new(Mutex::new(shown_now)),
            fresh_prompt,
            fresh_shown,
            reply_only: history
                .task_context
                .as_ref()
                .is_some_and(|context| context["task"]["reply_only"] == true),
        })
    }

    async fn run_round(&mut self, input: TurnInput) -> Result<ProviderOutcome> {
        let TurnInput {
            prompt,
            shown,
            fresh_prompt,
            fresh_shown,
            reply_only,
        } = input;
        let provider = self.phase.round()?.session_config.provider;
        {
            let (node, network, conversation, job, attempt) = (
                &self.node,
                &self.network,
                &self.conversation,
                &self.job,
                &self.attempt,
            );
            let round = self.phase.round_mut()?;
            let (config, token, server) = (&mut round.config, &round.token, &round.server);
            let local_url = node
                .tool_url
                .get()
                .context("local tool listener is not ready")?
                .clone();
            // The backend may still be starting with the Node.
            node.manager_backend_ready(provider).await;
            config.custom_tools = provider.tools_for(job.tool_kind());
            if reply_only {
                config.custom_tools.retain(|name, _| {
                    crate::tools::Tool::parse(name)
                        .is_some_and(|tool| tool.effect() == crate::tools::Effect::Read)
                });
            }
            config.tool_token = Some(token.clone());
            config.process_started = Some(process_recorder(node, &attempt.id));
            config.turn_binding = Some(turn_recorder(
                node,
                network,
                job,
                server,
                shown.clone(),
                fresh_prompt,
            ));
            config.tool_env.extend([
                ("VIBEFACE_NODE_URL".into(), local_url),
                ("VIBEFACE_NODE_CONVERSATION".into(), conversation.clone()),
                ("VIBEFACE_NODE_NETWORK".into(), network.clone()),
            ]);
        }
        let (persist_node, persist_conversation, persist_trigger, persist_attempt, persist_token) = (
            self.node.clone(),
            self.conversation.clone(),
            self.job.trigger.clone(),
            self.attempt.id.clone(),
            self.phase.round()?.token.clone(),
        );
        let resumed_thread = self.claimed.previous_native_id.clone();
        let (progress_node, progress_network, progress_job, progress_attempt, progress_source) = (
            self.node.clone(),
            self.network.clone(),
            self.job.clone(),
            self.attempt.id.clone(),
            self.node.identity.device_id()?,
        );
        let progress_text = self.partial.clone();
        let progress_sequence = self.progress_sequence.clone();
        // Readiness and SDK failures may qualify for backup. Local admission errors leave
        // through the outer Result and go straight to final failure recording.
        let readiness = self
            .node
            .provider_readiness(provider, &self.phase.round()?.config.model)
            .await;
        if let Err(error) = ensure_readiness(&readiness) {
            return Ok(ProviderOutcome::Failed(error));
        }
        let cancel = self
            .node
            .register_manager_attempt(
                self.phase.round()?.token.clone(),
                &self.network,
                &self.conversation,
                &self.attempt.id,
                &self.job.trigger,
                provider,
                self.environment.permission_epoch,
            )
            .await?;
        self.phase.register(cancel)?;
        self.reserve_and_activate().await?;
        ensure!(
            self.node
                .local_permission_epoch
                .load(std::sync::atomic::Ordering::Acquire)
                == self.environment.permission_epoch,
            "management permissions changed before native submission"
        );
        // This read lease must drop before any success seal, failure fence, or backup close.
        let _native_lease = self
            .node
            .manager_input_lease(&self.attempt.id, provider)
            .await?;
        let (conversation, trigger, attempt_id) = (
            self.conversation.clone(),
            self.job.trigger.clone(),
            self.attempt.id.clone(),
        );
        self.node
            .store
            .call("scheduler.update_turn_runtime", move |store| {
                store.update_turn_runtime(
                    &conversation,
                    &trigger,
                    &attempt_id,
                    ProviderStage::TurnStart,
                    TurnExecution::Unknown,
                )
            })
            .await?;
        let cancel = self.phase.submit()?;
        let thread = provider.prompt_mode() == crate::providers::PromptMode::Thread;
        let result = manager::run_manager(
            &self.phase.round()?.config,
            &prompt,
            self.claimed.previous_native_id.as_deref(),
            move |id| async move {
                if resumed_thread.is_some_and(|resumed| resumed != id)
                    && let Some(fresh) = fresh_shown
                {
                    *shown.lock().expect("shown") = fresh;
                }
                {
                    let conversation = (persist_conversation).to_owned();
                    let trigger = (persist_trigger).to_owned();
                    let attempt_id = (persist_attempt).to_owned();
                    let native_id = (id).to_owned();
                    let token = (persist_token).to_owned();
                    // Cursor confirms delivered context only after a successful turn.
                    let sent = !thread && provider != crate::providers::Provider::Cursor;
                    persist_node
                        .store
                        .call("scheduler.save_scheduler_native_id", move |store| {
                            store.save_scheduler_native_id(
                                &conversation,
                                &trigger,
                                &attempt_id,
                                &native_id,
                                &token,
                                sent,
                            )
                        })
                        .await
                }
            },
            cancel,
            move |progress| {
                progress_text.lock().expect("partial text").apply(&progress);
                let sequence =
                    progress_sequence.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
                let compacted = progress["type"] == "context_compacted";
                let node = progress_node.clone();
                let network = progress_network.clone();
                let job = progress_job.clone();
                let attempt = progress_attempt.clone();
                let source = progress_source.clone();
                async move {
                    if compacted {
                        {
                            let conversation = (job.conversation_id).to_owned();
                            let trigger = (job.trigger).to_owned();
                            let attempt_id = (attempt).to_owned();
                            node.store
                                .call("scheduler.record_context_compaction", move |store| {
                                    store.record_context_compaction(
                                        &conversation,
                                        &trigger,
                                        &attempt_id,
                                    )
                                })
                                .await
                        }?;
                    }
                    node.publish_progress(
                        &network,
                        progress_event(&job, &attempt, &source, sequence, progress),
                    );
                    Ok(())
                }
            },
        )
        .await;
        Ok(match result {
            Ok(result) => ProviderOutcome::Returned(result),
            Err(error) => ProviderOutcome::Failed(error),
        })
    }

    async fn reserve_and_activate(&mut self) -> Result<()> {
        if self.budget.is_none()
            && matches!(
                self.job.kind,
                TriggerKind::WorkResult
                    | TriggerKind::AgentTask
                    | TriggerKind::WorkspaceTask
                    | TriggerKind::Schedule
                    | TriggerKind::BotMessage
            )
        {
            let (network, job, limit) = (
                self.network.clone(),
                self.job.clone(),
                self.node.config.daily_decision_limit,
            );
            self.budget = Some(
                self.node
                    .store
                    .call("scheduler.reserve_scheduler_budget", move |store| {
                        store.reserve_scheduler_budget(
                            &network,
                            &job,
                            1,
                            limit,
                            crate::clock::now_ms(),
                        )
                    })
                    .await?,
            );
        }
        if !self.phase.locally_activated() && self.job.kind != TriggerKind::AgentTask {
            let (conversation, trigger, attempt_id) = (
                self.conversation.clone(),
                self.job.trigger.clone(),
                self.attempt.id.clone(),
            );
            self.node
                .store
                .call("scheduler.mark_started", move |store| {
                    store.mark_started(&conversation, &trigger, &attempt_id)
                })
                .await?;
        }
        self.node.publish_changed(
            &self.network,
            &self.conversation,
            Changed::Job(&self.job.trigger),
        );
        self.phase.activate()
    }

    async fn seal_tools(&mut self) -> Result<()> {
        self.node.seal_manager_tools(&self.attempt.id).await?;
        if let Some(registration) = self.phase.registration_mut() {
            registration.access = ToolAccess::Fenced;
        }
        Ok(())
    }

    async fn fence_tools(&mut self) -> bool {
        if !self.phase.tools_fenced()
            && self
                .node
                .fence_manager_attempt(&self.attempt.id)
                .await
                .is_ok()
            && let Some(registration) = self.phase.registration_mut()
        {
            registration.access = ToolAccess::Fenced;
        }
        self.phase.tools_fenced()
    }

    async fn switch_provider(
        &mut self,
        backup: BackupModel,
        previous: TurnRuntime,
        reason: FailureKind,
    ) -> Result<()> {
        self.seal_tools().await?;
        self.node.finish_manager_attempt(&self.attempt.id).await;
        let mut selected = self.phase.round()?.settings.clone();
        self.phase.begin_switch(); // Drops the old launch config before preparing another provider.
        selected.provider = backup.provider;
        selected.model = backup.model;
        selected.reasoning_effort = backup.reasoning_effort;
        selected.backup.automatic = false;
        let next = prepare_turn_from(
            &self.node,
            &self.network,
            &SessionKey {
                conversation_id: self.conversation.clone(),
                scope: self.scope.clone(),
            },
            self.environment.local_settings.clone(),
            self.environment.provider_local.clone(),
            Some(selected),
            self.environment.permission_epoch,
        )
        .await
        .map_err(ManagerError::not_started)?;
        if self.node.provider_configuration_pending(
            next.session_config.provider,
            &self.environment.provider_local,
        ) {
            return Err(ManagerError::not_started(anyhow::anyhow!(
                "backup provider is starting"
            )));
        }
        let (conversation, trigger, attempt_id, config) = (
            self.conversation.clone(),
            self.job.trigger.clone(),
            self.attempt.id.clone(),
            next.session_config.clone(),
        );
        self.claimed = self
            .node
            .store
            .call("scheduler.switch_scheduler_session", move |store| {
                store.switch_scheduler_session(&conversation, &trigger, &attempt_id, &config)
            })
            .await?;
        let runtime = TurnRuntime {
            provider: next.session_config.provider,
            model: (!next.config.model.is_empty()).then(|| next.config.model.clone()),
            reasoning_effort: next.config.reasoning_effort.clone(),
            generation: self.node.provider_generation(
                next.session_config.provider,
                &self.environment.provider_local,
            )?,
            session_generation: self.claimed.session_generation,
            stage: ProviderStage::Validate,
            execution: TurnExecution::NotStarted,
            attempted_at_ms: crate::clock::now_ms(),
            switched_from: Some(crate::scheduling::RuntimeSelection {
                provider: previous.provider,
                model: previous.model,
                reasoning_effort: previous.reasoning_effort,
            }),
            switch_reason: Some(reason),
            ..previous
        };
        let (conversation, trigger, attempt_id) = (
            self.conversation.clone(),
            self.job.trigger.clone(),
            self.attempt.id.clone(),
        );
        self.job = self
            .node
            .store
            .call("scheduler.record_turn_runtime", move |store| {
                store.record_turn_runtime(&conversation, &trigger, &attempt_id, &runtime)
            })
            .await?;
        self.bot_model = next.bot_model;
        self.phase = TurnPhase::Unregistered {
            round: ProviderRound {
                settings: next.settings,
                stable: next.stable,
                config: next.config,
                server: next.server,
                session_config: next.session_config,
                token: uuid::Uuid::new_v4().to_string(),
            },
            activation: self.phase.activation(),
        };
        Ok(())
    }

    async fn record_success(
        &mut self,
        confirmed_shown: &Arc<Mutex<Shown>>,
    ) -> Result<RecordedReply> {
        let (conversation, trigger, attempt_id) = (
            self.conversation.clone(),
            self.job.trigger.clone(),
            self.attempt.id.clone(),
        );
        self.node
            .store
            .call("scheduler.update_turn_runtime", move |store| {
                store.update_turn_runtime(
                    &conversation,
                    &trigger,
                    &attempt_id,
                    ProviderStage::Record,
                    TurnExecution::Started,
                )
            })
            .await?;
        self.seal_tools().await?;
        if self.phase.round()?.session_config.provider == crate::providers::Provider::Cursor {
            let shown = confirmed_shown.lock().expect("shown").clone();
            confirm_cursor_seen(&self.node, &self.job, &shown, self.phase.result()?).await?;
        }
        let output = prefix(&self.phase.result()?.output, REPLY_LIMIT).to_owned();
        self.commit_reply(JobOutcome::Completed, output).await
    }

    async fn commit_reply(&mut self, outcome: JobOutcome, text: String) -> Result<RecordedReply> {
        let (identity, network, conversation, trigger, attempt_id) = (
            self.node.identity.clone(),
            self.network.clone(),
            self.conversation.clone(),
            self.job.trigger.clone(),
            self.attempt.id.clone(),
        );
        self.job = self
            .node
            .store
            .call("scheduler.finish_job", move |store| {
                store.finish_job(
                    &identity,
                    &network,
                    &conversation,
                    &trigger,
                    &attempt_id,
                    outcome,
                    &text,
                    crate::clock::now_ms(),
                )
            })
            .await?;
        Ok(RecordedReply(invocation_value(&self.job, &self.network)))
    }

    async fn refund_budget(&mut self) -> Result<()> {
        if let Some(reservation) = self.budget.take() {
            // A rejected refund or failed Store call consumes this live reservation too.
            self.node
                .store
                .call("scheduler.refund_scheduler_budget", move |store| {
                    store.refund_scheduler_budget(reservation, boot_id())
                })
                .await?;
        }
        Ok(())
    }

    async fn record_error(&mut self, error: anyhow::Error) -> Result<RecordedReply> {
        self.job = record_failure(
            &self.node,
            &self.job,
            &error,
            if self.phase.unresolved() {
                TurnExecution::Unknown
            } else {
                TurnExecution::NotStarted
            },
        )
        .await?;
        let cancellation = error
            .downcast_ref::<ManagerError>()
            .is_some_and(|error| error.cancelled());
        let failure = error.downcast_ref::<ManagerError>().map_or_else(
            || ExecFailure::new(FailureKind::Unknown, error.to_string()),
            |error| error.failure.clone(),
        );
        let uncertain = self.job.pending_check
            || self.job.runtime.as_ref().is_some_and(|runtime| {
                runtime.execution == TurnExecution::Unknown
                    || !self.phase.locally_activated()
                        && runtime.execution == TurnExecution::Started
            })
            || self.phase.unresolved()
                && error
                    .downcast_ref::<ManagerError>()
                    .is_none_or(|error| error.uncertain());
        let tools_fenced = self.fence_tools().await;
        let (network, conversation, trigger, local) = (
            self.network.clone(),
            self.conversation.clone(),
            self.job.trigger.clone(),
            self.node.identity.device_id()?,
        );
        let (current, moved) = self
            .node
            .store
            .call("scheduler.current_attempt", move |store| {
                let current = store
                    .scheduler_job(&conversation, &trigger)?
                    .context("scheduler job missing")?;
                let owner = store
                    .conversation(&network, &conversation)?
                    .and_then(|conversation| conversation.owner);
                let moved = current.kind != TriggerKind::AgentTask && owner != Some(local);
                Ok((current, moved))
            })
            .await?;
        let stopped = matches!(current.state, JobState::Cancelling { .. });
        let no_execution = self
            .node
            .store
            .call("scheduler.scheduler_attempt_not_started", move |store| {
                store.scheduler_attempt_not_started(&current)
            })
            .await?;
        // Stopping a reply ends only this turn; dispatched work keeps its own tracking.
        let terminal = if (cancellation || (stopped && no_execution)) && tools_fenced {
            JobOutcome::Cancelled(
                if moved {
                    "会话已移到另一台设备"
                } else {
                    "用户停止了回复"
                }
                .into(),
            )
        } else if uncertain || !tools_fenced {
            JobOutcome::Interrupted {
                failure,
                process_unconfirmed: true,
            }
        } else {
            JobOutcome::Failed(model_noted(failure, self.bot_model.as_deref()))
        };
        if tools_fenced && no_execution {
            self.refund_budget().await?;
        }
        let text = self.partial.lock().expect("partial text").text();
        self.commit_reply(terminal, prefix(&text, REPLY_LIMIT).to_owned())
            .await
    }

    pub(super) async fn finish(mut self, execution: Result<RecordedReply>) -> Result<Value> {
        let mut final_result = match execution {
            Ok(recorded) => Ok(recorded.0),
            Err(error) => self.record_error(error).await.map(|recorded| recorded.0),
        };
        // All claimed paths, including failed error recording, retain this final close.
        self.node.finish_manager_attempt(&self.attempt.id).await;
        if final_result.is_err()
            && !self.phase.unresolved()
            && let Err(error) = self.refund_budget().await
        {
            final_result = final_result.map_err(|failure| {
                failure.context(format!("scheduler budget refund failed: {error:#}"))
            });
        }
        let (node, network, conversation, job) =
            (&self.node, &self.network, &self.conversation, &self.job);
        let trigger = &job.trigger;
        if let Err(error) = {
            let identity = (node.identity).to_owned();
            let network = (network).to_owned();
            let job = (job).to_owned();
            node.store
                .call("scheduler.settle_workspace_assignment", move |store| {
                    store.settle_workspace_assignment(
                        &identity,
                        &network,
                        &job,
                        crate::clock::now_ms(),
                    )
                })
                .await
        } {
            tracing::warn!(event="workspace.assignment.reconcile_pending",%error,"assignment waits for lifecycle reconciliation");
        }
        node.publish_changed(network, crate::workspace::WORKSPACE_SCOPE, Changed::Reload);
        match (&final_result, &job.state) {
            (Err(error), _) => {
                tracing::error!(event = "agent.turn.finished", trigger, error = %format!("{error:#}"), "agent turn could not be recorded")
            }
            (Ok(_), JobState::Failed { failure, .. } | JobState::Interrupted { failure, .. }) => {
                tracing::warn!(
                    event = "agent.turn.finished",
                    trigger,
                    state = job.state.name(),
                    failure = ?failure.kind,
                    detail = failure.detail,
                    "agent turn ended without a reply"
                )
            }
            (Ok(_), state) => tracing::info!(
                event = "agent.turn.finished",
                trigger,
                state = state.name(),
                "agent turn finished"
            ),
        }
        node.publish_changed(network, conversation, Changed::Job(&job.trigger));

        final_result
    }
}

#[cfg(test)]
#[path = "turn_tests.rs"]
mod tests;
