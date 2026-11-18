use super::*;
use crate::host::NodeIdentity;

impl Backend {
    pub(in crate::views::backend) async fn reconnect_remembered_local(
        &mut self,
        identity: &NodeIdentity,
        passphrase: &str,
    ) -> Result<bool> {
        let directory = self.client_directory()?;
        if !ClientKey::exists(&directory) {
            return Ok(false);
        }
        let key = Arc::new(ClientKey::open(&directory, passphrase)?);
        let Some(network) = key.network_id().map(str::to_owned) else {
            return Ok(false);
        };
        let (entry, grant) = match self.local_grant(identity, &key).await {
            Ok(local) => local,
            Err(error) => {
                return match remembered_endpoint_for_node(&key, identity) {
                    Some(endpoint) => self.connect(key, endpoint).await.map(|_| true),
                    None => Err(error),
                };
            }
        };
        ensure!(
            grant.network_id() == network,
            "此 Node 属于另一个个人网络。"
        );
        // Establish and verify membership before changing the current profile or UI state.
        let client = Client::open_local(
            key.clone(),
            grant,
            self.prepare_outbox(&network, key.device_id())?,
        )
        .await?;
        if !client.can_write() {
            client.close();
            return Ok(false);
        }
        self.prepare_connection(&key, &network, "正在读取本机 Node。")
            .await?;
        self.use_local_client(client, key, identity.clone(), entry)
            .await?;
        Ok(true)
    }

    async fn local_grant(
        &self,
        identity: &NodeIdentity,
        key: &ClientKey,
    ) -> Result<(Entry, vibeface_client::LocalGrant)> {
        let credential = self.host.local_credential(identity).await?;
        let entry = Entry {
            origin: credential.origin,
            node_id: credential.node_id,
        };
        let expected = entry.clone();
        let data_dir = identity.data_dir.clone();
        let target = tokio::task::spawn_blocking(move || {
            vibeface_client::LocalTarget::read(&data_dir, &expected)
        })
        .await??;
        let grant = target.grant(key.device_id()).await?;
        Ok((entry, grant))
    }

    async fn use_local_client(
        &mut self,
        client: Client,
        key: Arc<ClientKey>,
        identity: NodeIdentity,
        entry: Entry,
    ) -> Result<()> {
        let enrolled = client.can_write();
        self.state.network = Some(client.network_id().into());
        self.state.client_id = Some(key.device_id().into());
        self.reconnect = Some(ReconnectTarget::Local(identity.clone()));
        self.active_entry = Some(entry);
        self.key = Some(key);
        self.pushes = Some(client.subscribe());
        self.client = Some(client);
        if enrolled {
            self.identity(IdentityAction::Directory).await?;
        }
        self.refresh().await?;
        self.host.remember_selection(&identity)?;
        self.remember_profile_hints()?;
        Ok(())
    }

    pub(super) async fn save_connection(
        &mut self,
        editor: Editor,
        values: BTreeMap<String, String>,
    ) -> Result<()> {
        #[cfg(test)]
        Self::test_phase("save-connection-enter");
        let get = |key: &str| values.get(key).cloned().unwrap_or_default();
        match editor {
            Editor::ConnectLocal => {
                if let Some(expected) = values.get("_local_profile_id") {
                    ensure!(
                        self.current_profile_id()? == *expected,
                        "个人网络档案已切换，请重新连接。"
                    );
                }
                let identity = self
                    .host
                    .snapshot()
                    .selected
                    .context("请先选择运行中的本机 Node。")?;
                if let Some(expected) = values.get("_local_node_id") {
                    ensure!(
                        &identity.device_id == expected
                            && values
                                .get("_local_data_dir")
                                .is_some_and(|path| identity.data_dir == std::path::Path::new(path)),
                        "本机 Node 已切换，请重新打开连接表单。"
                    );
                }
                let directory = self.prepare_client_storage()?;
                let passphrase = get("passphrase");
                let key = Arc::new(if ClientKey::exists(&directory) {
                    ClientKey::open(&directory, &passphrase)?
                } else {
                    ClientKey::create(&directory, &passphrase)?
                });
                let (entry, grant) = match self.local_grant(&identity, &key).await {
                    Ok(local) => local,
                    Err(error) => {
                        return match remembered_endpoint_for_node(&key, &identity) {
                            Some(endpoint) => self.connect(key, endpoint).await,
                            None => Err(error),
                        };
                    }
                };
                let network = grant.network_id().to_owned();
                let previous = self
                    .prepare_connection(&key, &network, "正在验证本机 Node 和档案权限。")
                    .await?;
                let client = if let Some(previous) = previous {
                    previous.reconnect(Endpoint::Local(grant)).await?
                } else {
                    Client::open_local(
                        key.clone(),
                        grant,
                        self.prepare_outbox(&network, key.device_id())?,
                    )
                    .await?
                };
                self.use_local_client(client, key, identity, entry).await?;
            }
            Editor::Connect => {
                let directory = self.prepare_client_storage()?;
                let passphrase = get("passphrase");
                let key = if ClientKey::exists(&directory) {
                    ClientKey::open(&directory, &passphrase)?
                } else {
                    ClientKey::create(&directory, &passphrase)?
                };
                let remembered = key.remembered_endpoint().ok();
                let hint = self
                    .client_profiles
                    .as_ref()
                    .and_then(|registry| registry.selected().hints.entry.as_ref());
                let endpoint = connection_endpoint(
                    &get("transport"),
                    &get("origin"),
                    &get("node_id"),
                    &get("addresses"),
                    remembered.as_ref(),
                    hint,
                )?;
                self.connect(Arc::new(key), endpoint).await?;
            }
            _ => unreachable!("editor routed to the wrong connection handler"),
        }
        Ok(())
    }
}

fn remembered_endpoint_for_node(key: &ClientKey, identity: &NodeIdentity) -> Option<Endpoint> {
    key.remembered_endpoint()
        .ok()
        .filter(|endpoint| endpoint.node_id() == identity.device_id)
}
