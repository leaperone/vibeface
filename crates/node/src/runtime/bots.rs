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

    pub(super) async fn control_memory(&self, access: &Access, input: &Value) -> Result<Value> {
        ensure!(access.human, "only the user controls memory");
        let scope = id(input, "conversation_id")?;
        self.require_scope(access, &scope).await?;
        let command = crate::store::bot_settings::MemoryControlCommand {
            memory_id: id(input, "memory_id")?,
            base: serde_json::from_value(input.get("base").context("base is required")?.clone())?,
            after: serde_json::from_value(
                input.get("after").context("after is required")?.clone(),
            )?,
            attestation: attestation(input)?,
            operation_id: field(input, "operation_id")?.into(),
        };
        let result = self.store.lock().await.control_memory(
            &self.identity,
            &access.network_id,
            &scope,
            command,
            now_ms(),
        )?;
        if result.state == MemoryControlStatus::Applied {
            self.publish_changed(&access.network_id, &scope, Changed::Reload);
        }
        Ok(json!(result))
    }

    pub(super) async fn memory_state(&self, access: &Access, input: &Value) -> Result<Value> {
        ensure!(access.human, "only the user reads memory state");
        let scope = id(input, "conversation_id")?;
        self.require_scope(access, &scope).await?;
        let memory_id = id(input, "memory_id")?;
        let operation_id = input
            .get("operation_id")
            .map(|value| value.as_str().context("operation_id must be a string"))
            .transpose()?;
        let store = self.store.lock().await;
        store
            .bot_for_conversation(&access.network_id, &scope)?
            .context("no such bot")?;
        Ok(json!(store.memory_state(
            &access.network_id,
            &scope,
            &memory_id,
            operation_id
        )?))
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
    /// the pending check), its queued turns are cancelled when claimed, and the works it
    /// dispatched to this device are interrupted. Every device stops its own works when the
    /// deletion reaches it.
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

    async fn stop_bot_turns(self: &Arc<Self>, network: &str, scope: &str) -> Result<()> {
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
        let works: Vec<String> = {
            let store = self.store.lock().await;
            let mut works = Vec::new();
            for work in store.works(network, scope)? {
                if store
                    .work_record(&work.work_id)?
                    .is_some_and(|record| super::work::live(&record))
                {
                    works.push(work.work_id);
                }
            }
            works
        };
        for work_id in works {
            if let Err(error) = self.interrupt_work(network, &work_id).await {
                tracing::warn!(event = "bot.work_stop_failed", conversation_id = scope, work_id, error = %format!("{error:#}"), "a deleted bot's work could not be stopped");
            }
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

    /// The user picks a bot's face, or `null` for the one its ID gives, over the face's `version`
    /// the user saw. When the face is already `avatar` nothing is written, so a retry after the
    /// write landed reads it back. Otherwise the write holds only at that version: any later
    /// change gives a new one, so a replay cannot undo it even when the face went back.
    pub(super) async fn set_bot_avatar(&self, access: &Access, input: &Value) -> Result<Value> {
        ensure!(access.human, "only the user picks a bot's face here");
        let scope = id(input, "conversation_id")?;
        if let Some(allowed) = &access.conversation_id {
            ensure!(allowed == &scope, "outside this client's conversation");
        }
        let field = |key: &str| input.get(key).context("version and avatar are required");
        let version: Option<String> = serde_json::from_value(field("version")?.clone())
            .context("version is a string or null")?;
        let avatar: Option<Avatar> = serde_json::from_value(field("avatar")?.clone())
            .context("a face is an outline, eyes and hue, or null")?;
        let membership = self.membership_state().await?;
        ensure!(
            membership.genesis.network_id == access.network_id,
            "wrong personal network"
        );
        let mut store = self.store.lock().await;
        let bot = store
            .bot_for_conversation(&access.network_id, &scope)?
            .context("no such bot")?;
        if bot.avatar == avatar {
            return Ok(json!({"bot":bot}));
        }
        ensure!(
            bot.avatar_version == version,
            "the bot's face changed since you saw it; nothing was written"
        );
        let event = store.append_event(
            &self.identity,
            &access.network_id,
            &scope,
            EventBody::BotAvatarSet {
                version,
                avatar,
                attestation: Some(attestation(input)?),
            },
            vec![],
            now_ms(),
        )?;
        let bot = store.bot_for_conversation(&access.network_id, &scope)?;
        drop(store);
        tracing::info!(
            event = "bot.avatar",
            conversation_id = scope,
            "bot face set"
        );
        self.publish_changed(&access.network_id, &scope, Changed::Event(&event.hash));
        Ok(json!({"bot":bot}))
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Eyes, Hue, Outline};
    use crate::runtime::triage::tests::{bot, node, start_reply};

    #[tokio::test]
    async fn the_panel_writes_a_face_only_at_the_version_it_saw() -> Result<()> {
        let (node, admin, dir) = node().await?;
        bot(&node, &admin, "c", "Ops", &[]).await?;
        bot(&node, &admin, "d", "Docs", &[]).await?;
        let network = node.membership_state().await?.genesis.network_id;
        let face = |outline, eyes, hue| -> Result<Avatar> {
            Ok(Avatar {
                outline,
                eyes,
                hue: Hue::try_from(hue).map_err(anyhow::Error::msg)?,
            })
        };
        let (cloud, bean) = (
            face(Outline::Cloud, Eyes::Wide, 285)?,
            face(Outline::Bean, Eyes::Dot, 155)?,
        );
        let client = |scope: Option<&str>| Access {
            network_id: network.clone(),
            human: true,
            conversation_id: scope.map(Into::into),
            manager: None,
        };
        let request =
            |scope: &str, version: Option<String>, avatar: Option<Avatar>| -> Result<Value> {
                let signed = admin.attest(
                    &network,
                    scope,
                    EventBody::BotAvatarSet {
                        version: version.clone(),
                        avatar,
                        attestation: None,
                    },
                )?;
                Ok(
                    json!({"method":"set_bot_avatar","conversation_id":scope,"version":version,
                "avatar":avatar,"attestation":signed.attestation()}),
                )
            };
        let events = |node: &Arc<Runtime>| {
            let (node, network) = (node.clone(), network.clone());
            async move {
                let local = node.identity.device_id()?;
                anyhow::Ok(
                    node.store
                        .lock()
                        .await
                        .events_from(&network, &local, 1, 1000)?
                        .len(),
                )
            }
        };
        let shown = |node: &Arc<Runtime>| {
            let (node, network) = (node.clone(), network.clone());
            async move {
                let bot = node
                    .store
                    .lock()
                    .await
                    .bot_for_conversation(&network, "c")?
                    .context("bot")?;
                anyhow::Ok((bot.avatar, bot.avatar_version))
            }
        };
        let reader = Access {
            human: false,
            conversation_id: Some("d".into()),
            ..client(None)
        };
        let listed = node.tool(reader.clone(), "list_bots", json!({})).await?;
        let derived = Avatar::derived("c");
        let c = listed["bots"]
            .as_array()
            .unwrap()
            .iter()
            .find(|bot| bot["bot_id"] == "c")
            .unwrap();
        assert_eq!(
            c["avatar"],
            json!({"outline":derived.outline,"eyes":derived.eyes,"hue":derived.hue,"picked":false})
        );

        let pick = request("c", None, Some(cloud))?;
        let agent = Access {
            human: false,
            ..client(None)
        };
        assert!(
            node.rpc(agent, pick.clone()).await.is_err(),
            "agents never pick a face"
        );
        let elsewhere = node.rpc(client(Some("d")), pick.clone()).await.unwrap_err();
        assert!(elsewhere.to_string().contains("outside"), "{elsewhere}");
        let mut missing = pick.clone();
        missing.as_object_mut().unwrap().remove("version");
        let missing = node.rpc(client(None), missing).await.unwrap_err();
        assert!(
            missing
                .to_string()
                .contains("version and avatar are required"),
            "{missing}"
        );
        let mut forged = pick.clone();
        forged["version"] = json!("0123456789abcdef");
        assert!(
            node.rpc(client(None), forged).await.is_err(),
            "the signature covers the version"
        );
        let in_bots = node
            .rpc(client(None), request("bots", None, Some(cloud))?)
            .await;
        assert!(in_bots.is_err(), "the bots scope has no face");
        assert_eq!(
            shown(&node).await?,
            (None, None),
            "no version before any change"
        );

        let written = node.rpc(client(Some("c")), pick.clone()).await?;
        let (_, picked) = shown(&node).await?;
        let picked = picked.context("a written face has a version")?;
        assert_eq!(picked.len(), 16);
        assert_eq!(written["bot"]["avatar"], json!(cloud));
        assert_eq!(
            written["bot"]["avatar_version"],
            json!(picked),
            "the reply names the new version"
        );

        let count = events(&node).await?;
        let again = node.rpc(client(None), pick.clone()).await?;
        assert_eq!(again["bot"]["avatar_version"], json!(picked));
        assert_eq!(
            events(&node).await?,
            count,
            "a retry after the write landed writes nothing"
        );

        node.rpc(client(None), request("c", Some(picked.clone()), None)?)
            .await?;
        let (face, back) = shown(&node).await?;
        assert_eq!(face, None, "back to the face the id gives");
        let back = back.context("going back is a change too")?;
        let count = events(&node).await?;
        let stale = node.rpc(client(None), pick.clone()).await.unwrap_err();
        assert!(
            stale.to_string().contains("changed since you saw it"),
            "{stale}"
        );
        assert_eq!(
            (shown(&node).await?, events(&node).await?),
            ((None, Some(back.clone())), count),
            "the face went back to what the request saw, yet its version moved on"
        );

        node.rpc(client(None), request("c", Some(back), Some(bean))?)
            .await?;
        let stale = node.rpc(client(None), pick).await.unwrap_err();
        assert!(
            stale.to_string().contains("changed since you saw it"),
            "{stale}"
        );
        assert_eq!(
            shown(&node).await?.0,
            Some(bean),
            "a stale replay cannot undo a later pick"
        );

        let read = node
            .tool(reader.clone(), "read_bot", json!({"bot":"Ops"}))
            .await?;
        assert_eq!(
            read["avatar"],
            json!({"outline":"bean","eyes":"dot","hue":155,"picked":true})
        );
        drop(node);
        std::fs::remove_dir_all(dir)?;
        Ok(())
    }

    #[tokio::test]
    async fn created_and_looked_up_bots_show_their_face() -> Result<()> {
        let (node, admin, dir) = node().await?;
        bot(&node, &admin, "c", "Ops", &[]).await?;
        let network = node.membership_state().await?.genesis.network_id;
        let (trigger, attempt) = start_reply(&node, &admin, "m1").await?;
        let idle = Access {
            network_id: network,
            human: false,
            conversation_id: Some("c".into()),
            manager: None,
        };
        node.register_manager_attempt(
            "token".into(),
            idle.clone(),
            &attempt,
            &trigger,
            crate::providers::Provider::Codex,
        )
        .await?;
        let live = Access {
            manager: Some(crate::runtime::ManagerAccess {
                attempt_id: attempt,
                trigger,
            }),
            ..idle
        };
        let created = node
            .tool(
                live.clone(),
                "create_bot",
                json!({"call_ref":"new","name":"Docs"}),
            )
            .await?;
        let bot_id = created["bot_id"].as_str().context("bot_id")?;
        let derived = Avatar::derived(bot_id);
        let shown =
            json!({"outline":derived.outline,"eyes":derived.eyes,"hue":derived.hue,"picked":false});
        assert_eq!(created["avatar"], shown);
        assert!(created.get("avatar_version").is_none());
        let looked_up = node
            .tool(live, "call_lookup", json!({"call_ref":"new"}))
            .await?;
        assert_eq!(looked_up["avatar"], shown);
        assert!(looked_up.get("avatar_version").is_none());
        drop(node);
        std::fs::remove_dir_all(dir)?;
        Ok(())
    }
}
