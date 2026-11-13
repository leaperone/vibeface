use super::*;

impl Backend {
    pub(super) async fn save_identity(
        &mut self,
        editor: Editor,
        values: BTreeMap<String, String>,
    ) -> Result<()> {
        #[cfg(test)]
        Self::test_phase("save-identity-enter");
        let get = |key: &str| values.get(key).cloned().unwrap_or_default();
        let entry = || Entry {
            origin: get("origin"),
            node_id: get("node_id"),
        };
        match editor {
            Editor::CreateProfile => {
                let registry = self
                    .client_profiles
                    .as_mut()
                    .context("个人网络档案尚未安全读取")?;
                let id = registry.create(&get("label"))?;
                self.publish_profiles()?;
                let _ = self
                    .emit(Event::Notice(format!(
                        "新档案已建立：{}。明确选择后再加入网络，原档案保留。",
                        id.as_str()
                    )))
                    .await;
            }
            Editor::Join => {
                let directory = self.prepare_client_storage()?;
                let passphrase = get("passphrase");
                let mut key = if ClientKey::exists(&directory) {
                    ClientKey::open(&directory, &passphrase)?
                } else {
                    ClientKey::create(&directory, &passphrase)?
                };
                ensure!(
                    key.network_id().is_none(),
                    "当前档案已加入个人网络。请创建并选择新档案后加入另一个网络，原身份会保留。"
                );
                let mut enrollment = if vibeface_client::Enrollment::pending(&key) {
                    vibeface_client::Enrollment::resume(&key).await?
                } else {
                    vibeface_client::Enrollment::start(
                        &key,
                        &get("origin"),
                        &get("ticket"),
                        &get("name"),
                    )
                    .await?
                };
                let deadline = tokio::time::Instant::now() + Duration::from_secs(600);
                loop {
                    ensure!(
                        tokio::time::Instant::now() < deadline,
                        "邀请已过期，请重新申请"
                    );
                    match enrollment.poll().await? {
                        vibeface_client::Progress::Waiting => {}
                        vibeface_client::Progress::Code(code) => {
                            let _ = self
                                .emit(Event::Notice(format!("请在管理设备核对短码：{code}")))
                                .await;
                        }
                        vibeface_client::Progress::Joined => break,
                    }
                    tokio::time::sleep(Duration::from_secs(1)).await;
                }
                let endpoint = enrollment.endpoint();
                let (network, _) = enrollment.finish()?;
                key.remember_endpoint(&network, endpoint.clone())?;
                self.connect(Arc::new(key), endpoint).await?;
            }
            Editor::CreateNetwork => {
                let directory = self.prepare_client_storage()?;
                let passphrase = get("passphrase");
                let key = if ClientKey::exists(&directory) {
                    ClientKey::open(&directory, &passphrase)?
                } else {
                    ClientKey::create(&directory, &passphrase)?
                };
                ensure!(
                    key.network_id().is_none() || vibeface_client::IdentityPlan::exists(&key),
                    "请创建并选择新的身份档案后创建或恢复网络，原身份会保留。"
                );
                let plan = if vibeface_client::IdentityPlan::exists(&key) {
                    vibeface_client::IdentityPlan::load(&key, &get("password"))?
                } else {
                    let selected = self
                        .host
                        .snapshot()
                        .selected
                        .context("请先选择运行中的本机Node")?;
                    let invitation = self.host.bootstrap_invitation(&selected).await?;
                    vibeface_client::prepare_create(
                        &key,
                        Entry {
                            origin: invitation.origin.clone(),
                            node_id: invitation.node_id.clone(),
                        },
                        invitation.capability.as_str(),
                        &get("name"),
                        Some(&get("node_name")),
                        &PathBuf::from(get("new_file")),
                        &get("password"),
                    )
                    .await?
                };
                self.identity_plan = Some((key, plan));
                self.publish_identity_plan();
            }
            Editor::Recovery => {
                let directory = self.prepare_client_storage()?;
                let passphrase = get("passphrase");
                let key = if ClientKey::exists(&directory) {
                    ClientKey::open(&directory, &passphrase)?
                } else {
                    ClientKey::create(&directory, &passphrase)?
                };
                ensure!(
                    key.network_id().is_none() || vibeface_client::IdentityPlan::exists(&key),
                    "请创建并选择新的身份档案后创建或恢复网络，原身份会保留。"
                );
                let plan = if vibeface_client::IdentityPlan::exists(&key) {
                    vibeface_client::IdentityPlan::load(&key, &get("password"))?
                } else {
                    vibeface_client::prepare_recovery(
                        &key,
                        entry(),
                        &PathBuf::from(get("old_file")),
                        &get("password"),
                        &PathBuf::from(get("new_file")),
                        vibeface_client::RecoveryOptions {
                            name: &get("name"),
                            capability: None,
                            members: None,
                        },
                    )
                    .await?
                };
                self.identity_plan = Some((key, plan));
                self.publish_identity_plan();
            }
            Editor::Member(device_id) => {
                self.identity(IdentityAction::Rename {
                    device_id,
                    name: get("name"),
                })
                .await?;
                self.identity(IdentityAction::Directory).await?;
            }
            _ => unreachable!("editor routed to the wrong identity handler"),
        }
        Ok(())
    }
}
