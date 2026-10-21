//! The Node's resident Cursor SDK bridge. Every Cursor turn shares one process, so the
//! process only carries what is the same for every turn: the Node-wide shell environment and
//! one callback token. A tool callback names its agent, and the agent picks the attempt.
use super::*;
use crate::manager::ManagerKind;
use vibeface_bridge::agent::{SdkBridge, ToolCallbackConfig};

/// Process groups of the running bridges, with the program each one runs.
pub(super) const PIDFILE: &str = "cursor-sdk-bridge.pid";

impl Runtime {
    /// What every coordination Agent shell gets, whatever the turn.
    pub(crate) fn agent_env(&self) -> BTreeMap<String, String> {
        BTreeMap::from([
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

    /// Retires the current bridge after its running turns, then starts one for the current
    /// settings when Cursor is the assistant and its key is set, so the next reply starts warm.
    /// A Cursor that is off only retires.
    pub(super) fn refresh_cursor(&self) {
        let Some(bridge) = self.cursor.get().cloned() else {
            return;
        };
        let launch = self
            .assistant()
            .filter(|manager| manager.provider == ManagerKind::CursorSdk)
            .and_then(|manager| {
                let (key, env) =
                    crate::providers::cursor_launch(&self.provider_env(manager.provider));
                Some((manager, key?, env))
            });
        tokio::spawn(async move {
            bridge.retire().await;
            if let Some((manager, api_key, env)) = launch
                && let Err(error) = bridge
                    .prewarm(&manager.cursor_sdk_bin, &manager.workspace, &api_key, &env)
                    .await
            {
                tracing::warn!(%error, "Cursor SDK bridge did not start ahead of a reply");
            }
        });
    }

    /// Access for a tool call. A Cursor callback's bearer is the bridge-wide token, so the
    /// agent it names selects the attempt; the attempt's own token then decides, which keeps
    /// fencing and sealing unchanged.
    pub(crate) async fn tool_access(&self, bearer: &str, agent_id: &str) -> Option<Access> {
        let token = if bearer == self.cursor_callback_token {
            self.cursor_agents.lock().await.get(agent_id)?.clone()
        } else {
            bearer.to_owned()
        };
        self.authorize_bearer(&token).await
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
    async fn cursor_callbacks_act_as_the_attempt_that_owns_the_agent() -> Result<()> {
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
        node.cursor_agents.lock().await.extend([
            ("agent-a".to_owned(), "attempt-a".to_owned()),
            ("agent-b".to_owned(), "attempt-b".to_owned()),
        ]);
        let cursor = node.cursor_callback_token.clone();
        let conversation =
            |access: Option<Access>| access.and_then(|access| access.conversation_id);

        assert_eq!(
            conversation(node.tool_access(&cursor, "agent-a").await).as_deref(),
            Some("conversation-a")
        );
        assert_eq!(
            conversation(node.tool_access(&cursor, "agent-b").await).as_deref(),
            Some("conversation-b")
        );
        assert!(node.tool_access(&cursor, "unknown-agent").await.is_none());
        assert!(node.tool_access("attempt-a", "cli").await.is_some());
        assert!(
            node.tool_access("attempt-a", "agent-b")
                .await
                .is_some_and(|access| access.conversation_id.as_deref() == Some("conversation-a"))
        );

        node.revoke_tool_token("attempt-a").await;
        assert!(node.tool_access(&cursor, "agent-a").await.is_none());
        assert!(node.tool_access(&cursor, "agent-b").await.is_some());
        drop(node);
        std::fs::remove_dir_all(dir)?;
        Ok(())
    }
}
