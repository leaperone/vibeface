use super::*;

impl Backend {
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
                let directory = self.prepare_client_storage()?;
                let passphrase = get("passphrase");
                let key = Arc::new(if ClientKey::exists(&directory) {
                    ClientKey::open(&directory, &passphrase)?
                } else {
                    ClientKey::create(&directory, &passphrase)?
                });
                let identity = self
                    .host
                    .snapshot()
                    .selected
                    .context("请先明确选择运行中的本机Node")?;
                let credential = self.host.local_credential(&identity).await?;
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
                let network = grant.network_id().to_owned();
                let previous = self
                    .prepare_connection(
                        &key,
                        &network,
                        "正在验证本机会话，只读访问不授予成员权限。",
                    )
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
                self.state.network = Some(network);
                self.state.client_id = Some(key.device_id().into());
                self.reconnect = Some(ReconnectTarget::Local(identity));
                self.active_entry = Some(entry);
                self.key = Some(key);
                self.pushes = Some(client.subscribe());
                self.client = Some(client);
                self.refresh().await?;
                self.remember_profile_hints()?;
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
