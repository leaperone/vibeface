use super::*;

impl Runtime {
    pub(super) async fn create_bot(
        self: &Arc<Self>,
        access: &Access,
        input: &Value,
    ) -> Result<Value> {
        ensure!(access.human, "only the user creates Bots");
        self.require_role(&access.cluster_id, NodeRole::Gateway)
            .await?;
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
            .bots(&access.cluster_id)?
            .iter()
            .any(|bot| bot.bot_id == bot_id);
        if !existing && let Some(project) = &project {
            self.validate_bot_project(&access.cluster_id, project)
                .await?;
        }
        let bot = self.store.lock().await.create_bot(
            &self.identity,
            &access.cluster_id,
            &BotCreateSpec {
                conversation_id: scope.clone(),
                bot_id,
                name: name.to_owned(),
                instructions: instructions.to_owned(),
                project,
            },
            now_ms(),
        )?;
        self.publish_changed(&access.cluster_id, &scope, &bot.event_hash);
        Ok(json!({"bot":bot}))
    }

    pub(super) async fn bots(&self, access: &Access) -> Result<Value> {
        ensure!(
            access.human,
            "Bot definitions are unavailable to Agent tokens"
        );
        let state = self.membership_state().await?;
        ensure!(
            state.genesis.cluster_id == access.cluster_id,
            "wrong cluster"
        );
        let store = self.store.lock().await;
        store.authorize_node(&access.cluster_id, &self.identity.peer_id()?, None)?;
        let bots = store
            .bots(&access.cluster_id)?
            .into_iter()
            .filter(|bot| {
                access
                    .conversation_id
                    .as_ref()
                    .is_none_or(|scope| scope == &bot.conversation_id)
            })
            .collect::<Vec<_>>();
        Ok(json!({"bots":bots}))
    }

    async fn validate_bot_project(&self, cluster: &str, project: &BotProject) -> Result<()> {
        self.store.lock().await.authorize_node(
            cluster,
            &project.target_node,
            Some(NodeRole::Scheduler),
        )?;
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
        if project.target_node == self.identity.peer_id()? {
            ensure!(
                self.config.roles.contains(&NodeRole::Scheduler)
                    && self.settings_snapshot().manager.is_some(),
                "project Node coordination Agent is unavailable"
            );
            self.allowed_path(&project.cwd)?;
        } else {
            let capabilities = self
                .peer_request(
                    cluster,
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
