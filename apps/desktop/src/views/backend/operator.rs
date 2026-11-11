use super::*;
use crate::host::{NodeOperatorChange, NodeOperatorRead};
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PendingOperator {
    node_id: String,
    operation_id: String,
    kind: OperatorKind,
    fields: Vec<String>,
    has_secret_changes: bool,
}
#[derive(Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum OperatorKind {
    Node,
    Provider,
    Triage,
    Relay,
    Webhook,
    TokenCreate,
    TokenRevoke,
}
impl PendingOperator {
    fn validate(&self) -> Result<()> {
        ensure!(
            self.node_id.len() == 64
                && self
                    .node_id
                    .bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)),
            "本机设置记录Node标识无效"
        );
        vibeface_protocol::validate_id(&self.operation_id).map_err(anyhow::Error::msg)?;
        ensure!(
            self.fields.len() <= 32
                && self
                    .fields
                    .iter()
                    .all(|field| field.len() <= 64 && !field.chars().any(char::is_control)),
            "本机设置记录字段无效"
        );
        Ok(())
    }
}
impl OperatorKind {
    fn reviewable(self) -> bool {
        matches!(
            self,
            Self::Node | Self::Provider | Self::Triage | Self::Relay
        )
    }
    fn label(self) -> &'static str {
        match self {
            Self::Node => "Node配置",
            Self::Provider => "Provider配置",
            Self::Triage => "triage配置",
            Self::Relay => "relay配置",
            Self::Webhook => "Webhook测试",
            Self::TokenCreate => "授权码创建",
            Self::TokenRevoke => "授权码撤销",
        }
    }
}
impl Backend {
    pub(super) fn operator_identity(&self) -> Result<NodeIdentity> {
        let identity = self
            .host
            .snapshot()
            .selected
            .context("请先明确选择由当前用户管理的运行中Node")?;
        if let Some(expected) = &self.form_owner {
            ensure!(
                expected == &identity.device_id,
                "设置表单的原本机Node已经切换，请重新读取"
            );
        }
        Ok(identity)
    }
    pub(super) async fn read_node(&mut self, query: ReadRequest) -> Result<Value> {
        if self.form_owner.is_none()
            && let Some(client) = self.client.as_ref()
        {
            return client.read(query).await;
        }
        let identity = self.operator_identity()?;
        let query = match query {
            ReadRequest::NodeSettings => NodeOperatorRead::NodeSettings,
            ReadRequest::TriageSettings => NodeOperatorRead::TriageSettings,
            ReadRequest::RelaySettings => NodeOperatorRead::RelaySettings,
            ReadRequest::ProviderModels { provider } => {
                NodeOperatorRead::ProviderModels { provider }
            }
            ReadRequest::ProviderProbe { provider } => NodeOperatorRead::ProviderProbe { provider },
            ReadRequest::Logs => NodeOperatorRead::Logs {
                filter: Default::default(),
            },
            ReadRequest::LogsFiltered(filter) => NodeOperatorRead::Logs { filter },
            ReadRequest::Diagnostics => NodeOperatorRead::Diagnostics {
                since_ms: None,
                until_ms: None,
            },
            _ => anyhow::bail!("此请求不属于本机Node设置"),
        };
        self.host.operator_read(&identity, query).await
    }
    pub(super) async fn read_owner_node(&mut self, query: ReadRequest) -> Result<Value> {
        let previous = self.form_owner.clone();
        self.form_owner = Some(
            self.host
                .snapshot()
                .selected
                .context("请先选择本机Node")?
                .device_id,
        );
        let result = self.read_node(query).await;
        self.form_owner = previous;
        result
    }
    pub(super) async fn operator_submit(&mut self, action: Action) -> Result<()> {
        let identity = self.operator_identity()?;
        let path = self.client_directory()?.join("operator-unknown.json");
        ensure!(
            !tokio::fs::try_exists(&path).await?,
            "原本机设置操作结果未知，请先读取配置并明确核对原状态"
        );
        let (kind, fields, has_secret_changes) = match &action {
            Action::NodeSettings(_) => (OperatorKind::Node, vec!["roots", "manager"], false),
            Action::ProviderSettings(settings) => (
                OperatorKind::Provider,
                vec![
                    "provider",
                    "roles",
                    "model",
                    "program",
                    "app_server",
                    "environment",
                ],
                !settings.env.is_empty(),
            ),
            Action::Triage(settings) => (
                OperatorKind::Triage,
                vec!["outlet", "projects", "webhook", "session_scan"],
                !matches!(settings.webhook_secret, vibeface_client::SecretChange::Keep),
            ),
            Action::Relay { .. } => (
                OperatorKind::Relay,
                vec!["volunteer", "listen", "url", "limits"],
                false,
            ),
            Action::TestWebhook => (OperatorKind::Webhook, vec!["test_delivery"], false),
            Action::CreateDecisionToken { .. } => {
                (OperatorKind::TokenCreate, vec!["token_creation"], true)
            }
            Action::RevokeDecisionToken { .. } => {
                (OperatorKind::TokenRevoke, vec!["token_revocation"], true)
            }
            _ => anyhow::bail!("此操作不属于本机设置"),
        };
        let change = NodeOperatorChange::try_from(action)?;
        let pending = PendingOperator {
            node_id: identity.device_id.clone(),
            operation_id: self
                .form_id
                .clone()
                .unwrap_or_else(|| uuid::Uuid::new_v4().to_string()),
            kind,
            fields: fields.into_iter().map(str::to_owned).collect(),
            has_secret_changes,
        };
        self.state.operator_fields = pending.fields.clone();
        self.state.operator_secret_changes = pending.has_secret_changes;
        pending.validate()?;
        let save_path = path.clone();
        tokio::task::spawn_blocking(move || {
            vibeface_bridge::platform::privatefs::create_dir(
                save_path.parent().context("待核对记录缺少父目录")?,
            )?;
            vibeface_bridge::platform::privatefs::write_atomic(
                &save_path,
                &serde_json::to_vec(&pending)?,
            )
        })
        .await??;
        self.state.operator_unknown = Some(identity.device_id.clone());
        self.state.operator_unknown_kind = Some(kind.label().into());
        self.state.operator_can_close = kind.reviewable();
        self.publish();
        match self.host.operator_change(&identity, change).await {
            Ok(mut value) => {
                if let Some(token) = value
                    .as_object_mut()
                    .and_then(|object| object.remove("token"))
                    .and_then(|value| value.as_str().map(str::to_owned))
                {
                    let _ = self
                        .emit(Event::CopySecret(zeroize::Zeroizing::new(token)))
                        .await;
                }
                tokio::fs::remove_file(path).await?;
                self.state.operator_unknown = None;
                if let Some(id) = &self.form_id {
                    let _ = self.emit(Event::FormSaved(id.clone())).await;
                }
                self.publish();
                let _ = self
                    .emit(Event::Notice(
                        "本机设置操作已返回，读取配置可核对当前状态。".into(),
                    ))
                    .await;
                Ok(())
            }
            Err(error) => {
                if !error.is::<crate::host::OperatorOutcomeUnknown>() {
                    tokio::fs::remove_file(path).await?;
                    self.state.operator_unknown = None;
                    self.publish();
                }
                Err(error)
            }
        }
    }
    pub(super) async fn acknowledge_operator(&mut self, node_id: &str) -> Result<()> {
        let identity = self.operator_identity()?;
        ensure!(identity.device_id == node_id, "待核对操作不属于所选Node");
        self.host
            .operator_read(&identity, NodeOperatorRead::NodeSettings)
            .await?;
        let path = self.client_directory()?.join("operator-unknown.json");
        let bytes = vibeface_bridge::platform::privatefs::read_private_bounded(
            &path,
            4096,
            "本机设置待核对记录",
        )?;
        let pending: PendingOperator = serde_json::from_slice(&bytes)?;
        pending.validate()?;
        ensure!(pending.node_id == node_id, "待核对操作不属于所选Node");
        ensure!(
            pending.kind.reviewable(),
            "授权码与Webhook关键操作不能用普通设置提醒关闭；原结果仍未知，须查询实际状态或专用回执"
        );
        let archive = self
            .client_directory()?
            .join("operator-reviewed")
            .join(format!("{}.json", pending.operation_id));
        tokio::task::spawn_blocking(move||{vibeface_bridge::platform::privatefs::create_dir(archive.parent().context("核对记录缺少父目录")?)?;vibeface_bridge::platform::privatefs::write_atomic(&archive,&serde_json::to_vec(&json!({"original":pending,"server_outcome":"unknown","closed_by_user":true,"reviewed_at_ms":vibeface_protocol::now_ms()}))?)}).await??;
        tokio::fs::remove_file(path).await?;
        self.state.operator_unknown = None;
        self.publish();
        let _ = self
            .emit(Event::Notice(
                "已关闭本机设置待核对提醒。原请求不会重发；新的操作需要重新明确发起。".into(),
            ))
            .await;
        Ok(())
    }
    pub(super) async fn load_operator_pending(&mut self) -> Result<()> {
        let path = self.client_directory()?.join("operator-unknown.json");
        if tokio::fs::try_exists(&path).await? {
            let pending: PendingOperator = serde_json::from_slice(
                &vibeface_bridge::platform::privatefs::read_private_bounded(
                    &path,
                    4096,
                    "本机设置待核对记录",
                )?,
            )?;
            pending.validate()?;
            self.state.operator_fields = pending.fields.clone();
            self.state.operator_secret_changes = pending.has_secret_changes;
            self.state.operator_unknown_kind = Some(pending.kind.label().into());
            self.state.operator_can_close = pending.kind.reviewable();
            self.state.operator_unknown = Some(pending.node_id);
            self.publish();
        }
        Ok(())
    }
}
