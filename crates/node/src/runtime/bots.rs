use super::*;

impl Runtime {
    pub(super) async fn create_bot(
        self: &Arc<Self>,
        access: &Access,
        input: &Value,
    ) -> Result<Value> {
        access.require_body(input)?;
        ensure!(access.account_actor(), "only an Account creates Bots");
        self.require_role(&access.network_id, NodeRole::Gateway)
            .await?;
        let bot_id = id(input, "bot_id")?;
        let scope = id(input, "conversation_id")?;
        if let Some(allowed) = &access.conversation_id {
            ensure!(allowed == &scope, "outside this client's conversation");
        }
        let (_, definition) = crate::store::bots::bot_definition_from_body(input)?;
        let EventBody::BotCreated { project, .. } = &definition else {
            unreachable!()
        };
        let existing = self
            .store
            .lock()
            .await
            .bots(&access.network_id)?
            .iter()
            .any(|bot| bot.bot_id == bot_id);
        if !existing && let Some(project) = project {
            self.validate_bot_project(&access.network_id, project)
                .await?;
        }
        let bot = {
            let mut store = self.store.lock().await;
            let _verification = access.verification.enter();
            store.create_bot(
                &self.identity,
                &access.authorization,
                &access.freshness,
                &access.transport,
                &access.body,
                now_ms(),
            )?
        };
        self.publish_changed(&access.network_id, &scope, &bot.event_hash);
        Ok(json!({"bot":bot}))
    }

    pub(super) async fn bots(&self, access: &Access) -> Result<Value> {
        let store = self.store.lock().await;
        let _verification = access.verification.enter();
        let invocation = &access.authorization.signed_invocation().payload;
        let facts = vibeface_secure::authorization::RequestFacts {
            resource: invocation.resource.clone(),
            action: invocation.action,
            audience: invocation.audience.clone(),
            operation_id: invocation.operation_id.clone(),
            body_hash: invocation.body_hash.clone(),
            context: invocation.context.clone(),
        };
        access.authorization.check_current_in(
            &store.connection,
            store.authorization_state(),
            &facts,
            now_ms() as u64,
        )?;
        let allowed = store.authorized_conversation_ids(&access.authorization)?;
        let bots = store
            .bots(&access.network_id)?
            .into_iter()
            .filter(|bot| {
                allowed.contains(&bot.conversation_id)
                    && access
                        .conversation_id
                        .as_ref()
                        .is_none_or(|scope| scope == &bot.conversation_id)
            })
            .collect::<Vec<_>>();
        Ok(json!({"bots": bots}))
    }

    async fn validate_bot_project(&self, network: &str, project: &BotProject) -> Result<()> {
        let store = self.store.lock().await;
        let target =
            store.authorize_node(network, &project.target_node, Some(NodeRole::Executor))?;
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
        if project.target_node == self.identity.node_id()? {
            ensure!(
                self.config.roles.contains(&NodeRole::Executor),
                "project Node execution capability is disabled"
            );
            self.allowed_path(&project.cwd)?;
        } else {
            let space = SpaceId::parse(network)?;
            let host = ServiceId::from_key(&PublicKey::parse(&target.public_key)?);
            let state = store.authorization_state();
            let mut resources = store
                .connection
                .prepare("SELECT resource_id FROM resource_records WHERE space_id=?")?;
            let resources = resources.query_map([space.as_str()], |row| row.get::<_, String>(0))?;
            let mut registered = false;
            for resource in resources {
                let resource = ResourceId::parse(&resource?)?;
                if state.resource(&space, &resource).is_some_and(|record| {
                    record.payload.kind
                        == vibeface_secure::authorization::ResourceKind::ExecutionService
                        && record.payload.host == host
                }) {
                    registered = true;
                    break;
                }
            }
            ensure!(
                registered,
                "Bot project Node has no registered execution service in this space"
            );
        }
        Ok(())
    }
}
