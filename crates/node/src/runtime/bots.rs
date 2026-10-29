use super::*;

impl Runtime {
    pub(super) async fn create_bot(
        self: &Arc<Self>,
        access: &Access,
        input: &Value,
    ) -> Result<Value> {
        ensure!(access.human, "only the user creates Bots");
        let bot_id = id(input, "bot_id")?;
        let scope = id(input, "conversation_id")?;
        if let Some(allowed) = &access.conversation_id {
            ensure!(allowed == &scope, "outside this client's conversation");
        }
        let name = field(input, "name")?;
        let instructions = input
            .get("instructions")
            .map(|value| value.as_str().context("instructions must be a string"))
            .transpose()?
            .unwrap_or("");
        let project: Option<BotProject> = input
            .get("project")
            .filter(|value| !value.is_null())
            .map(|value| serde_json::from_value(value.clone()))
            .transpose()?;
        let existing = self
            .store
            .lock()
            .await
            .bots(&access.network_id)?
            .iter()
            .any(|bot| bot.bot_id == bot_id);
        if !existing && let Some(project) = &project {
            self.validate_bot_project(&access.network_id, project)
                .await?;
        }
        let bot = self.store.lock().await.create_bot(
            &self.identity,
            &access.network_id,
            &BotCreateSpec {
                conversation_id: scope.clone(),
                bot_id,
                name: name.to_owned(),
                instructions: instructions.to_owned(),
                project,
                attestation: attestation(input)?,
                owner: first_owner(input)?,
            },
            now_ms(),
        )?;
        self.publish_changed(&access.network_id, &scope, Changed::Event(&bot.event_hash));
        Ok(json!({"bot":bot}))
    }

    pub(super) async fn bots(&self, access: &Access) -> Result<Value> {
        ensure!(
            access.human,
            "Bot definitions are unavailable to Agent tokens"
        );
        let state = self.membership_state().await?;
        ensure!(
            state.genesis.network_id == access.network_id,
            "wrong personal network"
        );
        let store = self.store.lock().await;
        store.authorize_node(&access.network_id, &self.identity.device_id()?)?;
        let bots = store
            .bots(&access.network_id)?
            .into_iter()
            .filter(|bot| {
                access
                    .conversation_id
                    .as_ref()
                    .is_none_or(|scope| scope == &bot.conversation_id)
            })
            .collect::<Vec<_>>();
        let mut last = store.last_messages(&access.network_id)?;
        let mut busy = store.busy_conversations()?;
        let bots: Vec<_> = bots
            .into_iter()
            .map(|bot| {
                let last_message = last.remove(bot.conversation_id.as_str());
                let busy = busy.remove(bot.conversation_id.as_str());
                let mut value = json!(bot);
                value["last_message"] = json!(last_message);
                value["busy"] = json!(busy);
                value
            })
            .collect();
        Ok(json!({"bots":bots,"primary":store.primary_bot(&access.network_id)?}))
    }

    /// The user pauses, resumes or deletes a schedule; the scheduler reads the projection, so a
    /// paused one is not queued and a queued run is cancelled when claimed.
    pub(super) async fn control_schedule(&self, access: &Access, input: &Value) -> Result<Value> {
        ensure!(access.human, "only the user pauses or deletes a schedule");
        let scope = id(input, "conversation_id")?;
        let schedule_id = id(input, "schedule_id")?;
        let action: ScheduleAction = serde_json::from_value(input["action"].clone())
            .context("action is pause, resume or delete")?;
        let written = self.store.lock().await.control_schedule(
            &self.identity,
            &access.network_id,
            &scope,
            &schedule_id,
            action,
            attestation(input)?,
            now_ms(),
        )?;
        if let Some(hash) = &written {
            self.publish_changed(&access.network_id, &scope, Changed::Event(hash));
            self.schedule_wakeup.notify_one();
        }
        Ok(json!({"written":written.is_some()}))
    }

    /// The user deletes a bot from the web. Deleting one already gone writes nothing.
    pub(super) async fn delete_bot(
        self: &Arc<Self>,
        access: &Access,
        input: &Value,
    ) -> Result<Value> {
        ensure!(access.human, "only the user deletes a bot here");
        let scope = id(input, "conversation_id")?;
        if let Some(allowed) = &access.conversation_id {
            ensure!(allowed == &scope, "outside this client's conversation");
        }
        let network = &access.network_id;
        let written = self.store.lock().await.delete_bot(
            &self.identity,
            network,
            &scope,
            attestation(input)?,
            now_ms(),
        )?;
        if let Some(event) = &written {
            self.bot_deleted(network, &scope, &event.hash);
        }
        Ok(
            json!({"written":written.is_some(),"primary":self.store.lock().await.primary_bot(network)?}),
        )
    }

    /// Stops what a deleted bot was doing here, apart from the caller, which may be another
    /// bot's tool call: its running turn ends as a stopped reply (an unknown outcome goes to
    /// the pending check) and its queued turns are cancelled when claimed.
    pub(crate) fn bot_deleted(self: &Arc<Self>, network: &str, scope: &str, hash: &str) {
        tracing::info!(
            event = "bot.deleted",
            conversation_id = scope,
            "bot deleted"
        );
        self.publish_changed(network, scope, Changed::Event(hash));
        let (node, network, scope) = (self.clone(), network.to_owned(), scope.to_owned());
        tokio::spawn(async move {
            if let Err(error) = node.stop_bot_turns(&network, &scope).await {
                tracing::warn!(event = "bot.stop_failed", conversation_id = scope, error = %format!("{error:#}"), "a deleted bot's turn could not be stopped");
            }
            node.schedule_wakeup.notify_one();
        });
    }

    async fn stop_bot_turns(&self, network: &str, scope: &str) -> Result<()> {
        let running: Vec<String> = self
            .store
            .lock()
            .await
            .scheduler_jobs(scope)?
            .into_iter()
            .filter(|job| {
                matches!(
                    job.state,
                    crate::scheduling::JobState::Claimed { .. }
                        | crate::scheduling::JobState::Running { .. }
                )
            })
            .map(|job| job.trigger)
            .collect();
        for trigger in running {
            let job = self.store.lock().await.request_manager_cancel(
                &self.identity,
                network,
                scope,
                &trigger,
            )?;
            if let Some(crate::scheduling::JobState::Cancelling { attempt }) =
                job.map(|job| job.state)
            {
                self.fence_manager_attempt(&attempt.id).await?;
            }
            self.publish_changed(network, scope, Changed::Job(&trigger));
        }
        Ok(())
    }

    /// The user's choice of the bot that judges what comes in without an owner.
    pub(super) async fn set_primary_bot(&self, access: &Access, input: &Value) -> Result<Value> {
        ensure!(access.human, "only the user picks the primary bot");
        let scope = id(input, "conversation_id")?;
        if let Some(allowed) = &access.conversation_id {
            ensure!(allowed == &scope, "outside this client's conversation");
        }
        ensure!(
            self.store
                .lock()
                .await
                .bot_for_conversation(&access.network_id, &scope)?
                .is_some(),
            "no such bot"
        );
        let event = self
            .append(
                &access.network_id,
                BOTS_SCOPE,
                EventBody::PrimaryBotSet {
                    conversation_id: scope,
                    attestation: Some(attestation(input)?),
                },
            )
            .await?;
        tracing::info!(event = "bot.primary", "primary bot set");
        self.publish_changed(&access.network_id, BOTS_SCOPE, Changed::Event(&event.hash));
        Ok(json!({"primary":self.store.lock().await.primary_bot(&access.network_id)?}))
    }

    /// The user moves a bot's project folder, or clears it; asking for the current one writes nothing.
    pub(super) async fn set_bot_project(&self, access: &Access, input: &Value) -> Result<Value> {
        ensure!(access.human, "only the user moves a bot's project");
        let scope = id(input, "conversation_id")?;
        if let Some(allowed) = &access.conversation_id {
            ensure!(allowed == &scope, "outside this client's conversation");
        }
        let project: Option<BotProject> = input
            .get("project")
            .filter(|value| !value.is_null())
            .map(|value| serde_json::from_value(value.clone()))
            .transpose()?;
        let bot = self
            .store
            .lock()
            .await
            .bot_for_conversation(&access.network_id, &scope)?
            .context("no such bot")?;
        if bot.project == project {
            return Ok(json!({"bot":bot}));
        }
        if let Some(project) = &project {
            self.validate_bot_project(&access.network_id, project)
                .await?;
        }
        let event = self
            .append(
                &access.network_id,
                &scope,
                EventBody::BotProjectSet {
                    project,
                    attestation: Some(attestation(input)?),
                },
            )
            .await?;
        tracing::info!(
            event = "bot.project",
            conversation_id = scope,
            "bot project set"
        );
        self.publish_changed(&access.network_id, &scope, Changed::Event(&event.hash));
        Ok(json!({"bot":self.store.lock().await.bot_for_conversation(&access.network_id, &scope)?}))
    }

    pub(super) async fn validate_bot_project(
        &self,
        network: &str,
        project: &BotProject,
    ) -> Result<()> {
        self.store
            .lock()
            .await
            .authorize_node(network, &project.target_node)?;
        let cwd = Path::new(&project.cwd);
        ensure!(
            cwd.is_absolute()
                && !cwd.components().any(|part| {
                    matches!(
                        part,
                        std::path::Component::ParentDir | std::path::Component::CurDir
                    )
                }),
            "Bot project must use an absolute directory without parent components"
        );
        if project.target_node == self.identity.device_id()? {
            ensure!(
                self.manager().is_some(),
                "project Node coordination Agent is unavailable"
            );
            self.allowed_path(&project.cwd)?;
        } else {
            let capabilities = self
                .peer_request(
                    network,
                    &project.target_node,
                    json!({"type":"capabilities"}),
                )
                .await?;
            ensure!(
                capabilities["manager"].is_string(),
                "project Node has no coordination Agent"
            );
            ensure!(
                capabilities["roots"].as_array().is_some_and(|roots| {
                    roots
                        .iter()
                        .filter_map(Value::as_str)
                        .any(|root| cwd.starts_with(root))
                }),
                "Bot project is outside the execution Node's configured roots"
            );
        }
        Ok(())
    }
}
