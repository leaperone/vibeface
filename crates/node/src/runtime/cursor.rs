use super::*;
use crate::manager::CursorInstance;
use vibeface_bridge::agent::{SdkBridge, ToolCallbackConfig};

/// Process groups of the running bridges, with the program each one runs.
pub(super) const PIDFILE: &str = "cursor-sdk-bridge.pid";

impl Runtime {
    pub(crate) fn cursor_store_root(&self, owner: &CursorInstance) -> PathBuf {
        self.data_dir
            .join("cursor")
            .join("instances")
            .join(crate::crypto::digest(owner.key().as_bytes()))
    }

    /// What every coordination Agent shell gets, whatever the turn.
    pub(crate) fn agent_env(&self) -> BTreeMap<String, String> {
        BTreeMap::from([
            (
                "VIBEFACE_DATA_DIR".into(),
                self.data_dir.display().to_string(),
            ),
            (
                TRIAGE_URL_ENV.into(),
                format!("http://{}", self.config.local_listen),
            ),
            (
                "VIBEFACE_CODEX_SOCKET".into(),
                self.codex.socket().display().to_string(),
            ),
        ])
    }

    /// The tool listener is up, so the bridge can call back and turns may start.
    pub(crate) fn tools_ready(&self, url: String) -> Result<()> {
        let bridge = SdkBridge::new(
            self.agent_env(),
            ToolCallbackConfig {
                url: url.clone(),
                auth_token: self.cursor_callback_token.clone(),
            },
            Some(self.data_dir.join(PIDFILE)),
        )?;
        // The bridge goes first: a turn that sees the tool URL must find the bridge too.
        self.cursor
            .set(Arc::new(bridge))
            .map_err(|_| anyhow::anyhow!("Cursor SDK bridge is already initialized"))?;
        self.tool_url
            .set(url)
            .map_err(|_| anyhow::anyhow!("tool listener is already initialized"))?;
        self.refresh_cursor();
        Ok(())
    }

    pub(super) fn refresh_cursor(&self) {
        let Some(bridge) = self.cursor.get().cloned() else {
            return;
        };
        let launch = self
            .manager()
            .filter(|manager| manager.provider == Provider::Cursor)
            .and_then(|manager| {
                let (key, env) =
                    crate::providers::cursor_launch(&self.provider_env(manager.provider));
                Some((manager, key?, env))
            });
        let bin = self.providers().bin(Provider::Cursor);
        let owner = CursorInstance::Coordination;
        let owner_key = owner.key();
        let store_root = self.cursor_store_root(&owner);
        tokio::spawn(async move {
            bridge.retire_all().await;
            if let Some((manager, api_key, env)) = launch
                && let Err(error) = bridge
                    .prewarm(
                        &owner_key,
                        &bin,
                        &manager.workspace,
                        &store_root,
                        &api_key,
                        &env,
                    )
                    .await
            {
                tracing::warn!(event = "cursor.prewarm_failed", %error, "Cursor SDK bridge did not start ahead of a reply");
            }
        });
    }

    /// Access for a tool call. Cursor installs the active attempt bearer on its owner bridge;
    /// the existing authorization and fence checks then decide whether the call is still valid.
    pub(crate) async fn tool_access(&self, bearer: &str, _agent_id: &str) -> Option<Access> {
        self.authorize_bearer(bearer).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crypto::LocalIdentity;

    fn access(conversation: &str) -> Access {
        Access {
            network_id: "network".into(),
            human: false,
            conversation_id: Some(conversation.into()),
            manager: None,
        }
    }

    #[tokio::test]
    async fn cursor_callbacks_use_the_attempt_bearer() -> Result<()> {
        let dir = std::env::temp_dir().join(format!("vibeface-cursor-{}", uuid::Uuid::new_v4()));
        config::private_dir(&dir)?;
        let mut configuration = Config::local("127.0.0.1:0".parse()?, vec![], false);
        configuration.providers.codex_app_server.socket = Some(dir.join("unavailable.sock"));
        configuration.save(&dir)?;
        config::private_write(&dir.join("data-epoch.json"), &config::data_epoch_marker())?;
        config::private_write(
            &dir.join("identity.json"),
            &serde_json::to_vec(&LocalIdentity::generate())?,
        )?;
        let node = Runtime::open(&dir).await?;
        for (token, conversation) in [
            ("attempt-a", "conversation-a"),
            ("attempt-b", "conversation-b"),
        ] {
            node.agent_tokens
                .lock()
                .await
                .insert(token.into(), access(conversation));
        }
        let conversation =
            |access: Option<Access>| access.and_then(|access| access.conversation_id);

        assert_eq!(
            conversation(node.tool_access("attempt-a", "agent-a").await).as_deref(),
            Some("conversation-a")
        );
        assert_eq!(
            conversation(node.tool_access("attempt-b", "agent-b").await).as_deref(),
            Some("conversation-b")
        );
        assert!(node.tool_access("unknown", "agent-a").await.is_none());
        assert!(
            node.tool_access("attempt-a", "agent-b")
                .await
                .is_some_and(|access| access.conversation_id.as_deref() == Some("conversation-a"))
        );

        node.revoke_tool_token("attempt-a").await;
        assert!(node.tool_access("attempt-a", "agent-a").await.is_none());
        assert!(node.tool_access("attempt-b", "agent-b").await.is_some());
        drop(node);
        std::fs::remove_dir_all(dir)?;
        Ok(())
    }
}
