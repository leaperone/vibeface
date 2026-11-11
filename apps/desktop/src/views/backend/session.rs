use super::*;

impl Backend {
    pub(super) async fn cancel_reconnect(&mut self) -> Result<()> {
        if let Some(task) = self.reconnect_task.as_mut() {
            if !self.reconnect_joined {
                task.abort();
                let _ = tokio::time::timeout(super::reads::READ_DRAIN_TIMEOUT, task)
                    .await
                    .context("重新连接任务尚未退出，原存储租约保持占用。")?;
                self.reconnect_joined = true;
            }
        }
        if let Some(client) = &self.reconnect_lease {
            tokio::time::timeout(
                super::reads::READ_DRAIN_TIMEOUT,
                client.wait_for_read_storage(),
            )
            .await
            .context("重新连接的本机缓存尚未写完，不能交接原身份存储。")?;
        }
        self.reconnect_task = None;
        self.reconnect_lease = None;
        self.reconnect_joined = false;
        while let Ok(recovered) = self.recovery.try_recv() {
            if let Ok(client) = recovered.result {
                client.close();
                self.reconnect_lease = Some(client);
                tokio::time::timeout(
                    super::reads::READ_DRAIN_TIMEOUT,
                    self.reconnect_lease
                        .as_ref()
                        .unwrap()
                        .wait_for_read_storage(),
                )
                .await
                .context("已返回连接的本机缓存尚未写完，不能交接原身份存储。")?;
                self.reconnect_lease = None;
            }
        }
        Ok(())
    }
    pub(super) async fn prepare_connection(
        &mut self,
        key: &ClientKey,
        network: &str,
        status: &str,
    ) -> Result<Option<Client>> {
        ensure!(
            key.network_id().is_none_or(|known| known == network),
            "目标设备属于另一个个人网络，原身份保持不变"
        );
        self.cancel_reconnect().await?;
        self.cancel_reads().await?;
        let previous = self.take_profile_client()?;
        let key_network = key.network_id().map(str::to_owned);
        let reusable = previous
            .filter(|previous| {
                previous.client.network_id() == network
                    && previous.client.client_id() == key.device_id()
                    && previous.key_network == key_network
            })
            .map(|previous| previous.client);
        self.active_record = ActiveRecord::None;
        self.key = None;
        self.active_entry = None;
        self.pushes = None;
        self.history_state = None;
        self.conversation = None;
        self.attachments.clear();
        self.reconnect = None;
        self.retry_attempt = 0;
        self.retry_at = tokio::time::Instant::now();
        self.state = Snapshot {
            generation: self.state.generation + 1,
            status: status.into(),
            ..Default::default()
        };
        self.publish();
        Ok(reusable)
    }
    pub(super) async fn connect(&mut self, key: Arc<ClientKey>, endpoint: Endpoint) -> Result<()> {
        #[cfg(test)]
        Self::test_phase("connect-enter");
        endpoint.validate()?;
        let entry = match &endpoint {
            Endpoint::Gateway(entry) | Endpoint::Preferred { gateway: entry, .. } => entry.clone(),
            Endpoint::Lan { node_id, .. } => Entry {
                origin: String::new(),
                node_id: node_id.clone(),
            },
            Endpoint::Local(_) => anyhow::bail!("本机会话须通过明确的本机Node入口建立"),
        };
        let network = key
            .network_id()
            .map(str::to_owned)
            .or_else(|| {
                self.client_profiles
                    .as_ref()
                    .and_then(|registry| registry.selected().hints.network.clone())
            })
            .context("此身份没有可验证缓存位置，请通过邀请加入或明确连接本机Node")?;
        let reusable = self
            .prepare_connection(&key, &network, "正在验证目标设备和成员目录。")
            .await?;
        let client = if let Some(client) = reusable {
            match client.reopen_offline(endpoint.clone()).await {
                Ok(client) => client,
                Err(error) => {
                    ensure!(
                        client.offline_block_reason().is_none(),
                        "旧身份只允许读取已验证的本机私有数据：{error}"
                    );
                    client.reconnect_or_offline(endpoint.clone()).await?
                }
            }
        } else {
            let outbox = self.prepare_outbox(&network, key.device_id())?;
            match Client::open_offline(key.clone(), endpoint.clone(), outbox.clone()).await {
                Ok(client) => client,
                Err(error) => {
                    ensure!(
                        key.network_id().is_some(),
                        "此身份尚未加入网络，且找不到匹配的签名缓存；没有连接网络或申请授权：{error}"
                    );
                    Client::open_or_offline(key.clone(), endpoint.clone(), outbox).await?
                }
            }
        };
        self.state.network = Some(client.network_id().into());
        self.state.client_id = Some(client.client_id().into());
        self.reconnect = Some(ReconnectTarget::Network(endpoint));
        self.active_entry = Some(entry);
        self.key = Some(key);
        self.pushes = Some(client.subscribe());
        self.client = Some(client);
        if !self.client()?.is_online() {
            self.load_projection().await?;
            return Ok(());
        }
        self.remember_profile_hints()?;
        self.identity(IdentityAction::Directory).await?;
        self.refresh().await?;
        self.flush_prepared().await
    }
    pub(super) async fn maintain(&mut self) -> Result<()> {
        if self.host.poll_owned()? {
            self.publish_host().await;
        }
        if self.state.online {
            let unreachable = self
                .state
                .nodes
                .iter()
                .any(|node| !node.online && !node.revoked);
            if self.last_record_read.elapsed() >= Duration::from_secs(300)
                || unreachable && self.last_record_read.elapsed() >= Duration::from_secs(15)
            {
                self.last_record_read = tokio::time::Instant::now();
                match self.active_record.clone() {
                    ActiveRecord::None => {}
                    ActiveRecord::Work {
                        conversation_id,
                        work_id,
                    } => self.read_work_detail(conversation_id, work_id).await?,
                    ActiveRecord::Channel(channel_id) => {
                        self.read_history(Some(channel_id), None).await?
                    }
                    ActiveRecord::Task(task_id) => self.read_history(None, Some(task_id)).await?,
                }
            }
            return Ok(());
        }
        // First-window reads now run off actor. Do not reconnect the healthy connection
        // while its current scoped refresh is still filling that window.
        self.cancel_stale_reads().await?;
        if self.refresh_in_flight() {
            return Ok(());
        }
        if self.state.private_block.is_some() {
            return Ok(());
        }
        if self.reconnect_task.is_some() || self.reconnect_lease.is_some() {
            return Ok(());
        }
        if tokio::time::Instant::now() < self.retry_at {
            return Ok(());
        }
        let Some(previous) = self.client.clone() else {
            return Ok(());
        };
        let target = self.reconnect.clone().context("没有可重新验证的接入入口")?;
        self.retry_attempt = self.retry_attempt.saturating_add(1);
        self.retry_at = tokio::time::Instant::now()
            + Duration::from_secs((1u64 << self.retry_attempt.min(5)).min(30));
        self.cancel_reads().await?;
        self.state.generation += 1;
        self.history_state = None;
        self.state.history_loading = None;
        self.state.writable = false;
        self.state.status = "正在重新验证连接，未知操作不会重发。".into();
        self.publish();
        let endpoint = match target {
            ReconnectTarget::Network(endpoint) => endpoint,
            ReconnectTarget::Local(identity) => {
                let expected = self.active_entry.clone().context("本机接入身份缺失")?;
                let data_dir = identity.data_dir;
                let target = tokio::task::spawn_blocking(move || {
                    vibeface_client::LocalTarget::read(&data_dir, &expected)
                })
                .await??;
                Endpoint::Local(target.grant(previous.client_id()).await?)
            }
        };
        let attempt = self.state.generation;
        let profile_id = self.current_profile_id()?;
        let recovered = self.recovered.clone();
        self.reconnect_lease = Some(previous.clone());
        self.reconnect_joined = false;
        self.reconnect_task = Some(tokio::spawn(async move {
            let result = previous.reconnect(endpoint).await;
            let _ = recovered
                .send(Reconnected {
                    attempt,
                    profile_id,
                    result,
                })
                .await;
        }));
        self.publish();
        Ok(())
    }

    pub(super) async fn identity(&mut self, action: IdentityAction) -> Result<()> {
        ensure!(
            !self.state.local_session,
            "本机会话没有成员管理权限，请先加入个人网络"
        );
        let key = self.key.as_ref().context("本机身份尚未解锁")?.clone();
        let client = self.client()?.clone();
        let mut session = client.management_session()?;
        match action {
            IdentityAction::Directory => {
                let directory = session.directory().await?;
                let members = serde_json::to_value(directory.view.members)?;
                self.state.members = serde_json::from_value(members)?;
                self.state.can_manage = self
                    .state
                    .members
                    .iter()
                    .any(|m| m.device_id == key.device_id() && m.manage);
                self.publish();
            }
            IdentityAction::Invite => {
                let link = session.invite().await?;
                let _ = self
                    .emit(Event::Detail(vec![(
                        "邀请链接，请只交给本人设备".into(),
                        link,
                    )]))
                    .await;
            }
            IdentityAction::Requests => {
                self.state.joins =
                    serde_json::from_value(serde_json::to_value(session.requests().await?)?)?;
                self.publish();
            }
            IdentityAction::Rename { device_id, name } => {
                session.grant(&device_id, Some(&name), None).await?;
            }
            IdentityAction::Grant { device_id, manage } => {
                session.grant(&device_id, None, Some(manage)).await?;
            }
            IdentityAction::Revoke(id) => {
                session.revoke(&id).await?;
            }
            IdentityAction::Recovery(approve) => {
                session.settle_recovery(approve).await?;
            }
            IdentityAction::ConfirmJoin(id) => {
                session.confirm_join(&id).await?;
            }
            IdentityAction::GrantJoin { join_id, manage } => {
                session.grant_join(&join_id, manage).await?;
            }
            IdentityAction::RejectJoin(id) => {
                session.reject_join(&id).await?;
            }
        }
        Ok(())
    }
    pub(super) fn publish_identity_plan(&mut self) {
        self.state.identity_plan = self
            .identity_plan
            .as_ref()
            .map(|(_, plan)| IdentityPlanView {
                backup_path: plan.backup_path().display().to_string(),
                sha256: plan.backup_sha256(),
                network_id: plan.network_id().into(),
                state: format!("{:?}", plan.state()),
            });
        self.publish();
    }
}
