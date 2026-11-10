use super::{DesktopHost, NodeIdentity, NodeStatus, model};
use anyhow::{Context, Result, bail, ensure};
use serde::Deserialize;
use serde_json::Value;
use std::{fmt, net::SocketAddr};
use vibeface_control::{ControlRequest, LocalHttpCredential};
use zeroize::Zeroizing;

/// One-time network initialization authority, separate from the local HTTP launcher.
/// Keep this value on the background identity flow; it cannot be serialized into UI state.
pub struct BootstrapInvitation {
    pub origin: String,
    pub node_id: String,
    pub capability: Zeroizing<String>,
}

impl fmt::Debug for BootstrapInvitation {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("BootstrapInvitation")
            .field("origin", &self.origin)
            .field("node_id", &self.node_id)
            .field("capability", &"[redacted]")
            .finish()
    }
}

#[derive(Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum LocalSetupReply {
    Uninitialized {
        invitation: InvitationReply,
    },
    Initialized {
        entry: InitializedEntry,
        #[serde(rename = "removed")]
        _removed: bool,
        #[serde(default, rename = "local_launch")]
        _local_launch: Option<serde::de::IgnoredAny>,
    },
    OwnerRequired {
        initialized: bool,
    },
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct InvitationReply {
    url: String,
    device_id: String,
    capability: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct InitializedEntry {
    address: String,
    device_id: String,
}

fn verify_entry(
    origin: &str,
    node_id: &str,
    expected: &NodeIdentity,
    launcher: &LocalHttpCredential,
) -> Result<()> {
    model::device_id(node_id)?;
    ensure!(
        node_id == expected.device_id && node_id == launcher.node_id,
        "本机设置返回了另一个节点身份。"
    );
    let url = reqwest::Url::parse(origin)
        .map_err(|_| anyhow::anyhow!("本机设置返回了无效来源。"))?;
    launcher.require_origin(&url)?;
    ensure!(
        origin == launcher.origin,
        "本机设置来源与所选节点不一致。"
    );
    Ok(())
}

fn parse_invitation(
    value: Value,
    expected: &NodeIdentity,
    launcher: &LocalHttpCredential,
) -> Result<BootstrapInvitation> {
    let setup: LocalSetupReply = serde_json::from_value(value)
        .map_err(|_| anyhow::anyhow!("本机设置返回了无效结果。"))?;
    match setup {
        LocalSetupReply::Uninitialized { invitation } => {
            let capability = Zeroizing::new(invitation.capability);
            verify_entry(&invitation.url, &invitation.device_id, expected, launcher)?;
            ensure!(
                capability.len() == 64
                    && capability
                        .bytes()
                        .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)),
                "节点初始化凭据无效。"
            );
            ensure!(
                capability.as_str() != launcher.capability.as_str(),
                "节点返回了启动凭据，不能用于创建个人网络。"
            );
            Ok(BootstrapInvitation {
                origin: invitation.url,
                node_id: invitation.device_id,
                capability,
            })
        }
        LocalSetupReply::Initialized { entry, .. } => {
            verify_entry(&entry.address, &entry.device_id, expected, launcher)?;
            bail!("所选节点已属于个人网络，请通过邀请加入或连接已有身份。")
        }
        LocalSetupReply::OwnerRequired { initialized } => {
            if initialized {
                bail!("所选节点已属于个人网络，不能重新创建。")
            }
            bail!("未取得本机所有者授权，不能创建个人网络。")
        }
    }
}

impl DesktopHost {
    /// Reads the selected Node's one-time bootstrap invitation through Core's account-verified
    /// local setup endpoint. This operation never initializes or starts a Node.
    pub async fn bootstrap_invitation(
        &self,
        expected: &NodeIdentity,
    ) -> Result<BootstrapInvitation> {
        ensure!(
            self.selected.as_ref() == Some(expected),
            "请先选择并核对这台本机节点。"
        );
        let inspected = self.verify(expected, Some(NodeStatus::Running)).await?;
        ensure!(inspected.web_enabled, "所选节点未开启本机设置入口。");
        let launcher = LocalHttpCredential::read(&expected.data_dir)?;
        ensure!(
            launcher.node_id == expected.device_id,
            "本机设置启动器身份与所选节点不一致。"
        );
        let listen: SocketAddr = inspected
            .local_listen
            .parse()
            .context("本机节点监听地址无效。")?;
        ensure!(listen.ip().is_loopback(), "本机设置入口必须位于回环地址。");
        let origin = reqwest::Url::parse(&format!("http://{listen}"))?;
        launcher.require_origin(&origin)?;
        let value = self
            .request(&ControlRequest::LocalSetup {
                data_dir: expected.data_dir.clone(),
            })
            .await?;
        let invitation = parse_invitation(value, expected, &launcher)?;
        // Recheck after the subprocess/network round trip before handing authority to the client.
        let current_inspect = self.verify(expected, Some(NodeStatus::Running)).await?;
        ensure!(
            current_inspect.web_enabled && current_inspect.local_listen == inspected.local_listen,
            "本机设置监听地址已变化，请重新核对所选节点。"
        );
        let current = LocalHttpCredential::read(&expected.data_dir)?;
        ensure!(
            current.node_id == launcher.node_id && current.origin == launcher.origin,
            "本机设置入口已变化，请重新核对所选节点。"
        );
        Ok(invitation)
    }
}

#[cfg(test)]
#[path = "bootstrap_tests.rs"]
mod tests;
