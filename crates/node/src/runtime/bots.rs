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
        self.publish_changed(&access.network_id, &scope, &bot.event_hash);
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
        Ok(json!({"bots":bots,"primary":store.primary_assistant(&access.network_id)?}))
    }

    /// The user's choice of the assistant that judges what comes in without an owner.
    pub(super) async fn set_primary_assistant(
        &self,
        access: &Access,
        input: &Value,
    ) -> Result<Value> {
        ensure!(access.human, "only the user picks the primary assistant");
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
            "no such assistant"
        );
        let event = self
            .append(
                &access.network_id,
                ASSISTANTS_SCOPE,
                EventBody::PrimaryAssistantSet {
                    conversation_id: scope,
                    attestation: Some(attestation(input)?),
                },
            )
            .await?;
        tracing::info!(event = "assistant.primary", "primary assistant set");
        self.publish_changed(&access.network_id, ASSISTANTS_SCOPE, &event.hash);
        Ok(json!({"primary":self.store.lock().await.primary_assistant(&access.network_id)?}))
    }

    async fn validate_bot_project(&self, network: &str, project: &BotProject) -> Result<()> {
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
                self.settings_snapshot().manager.is_some(),
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
