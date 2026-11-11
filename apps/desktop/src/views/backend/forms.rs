//! Dispatch one form to its domain without combining every handler into one poll function.
mod bot;
mod connection;
mod delivery;
mod identity;
mod settings;
mod workspace;
use super::*;

impl Backend {
    pub(super) async fn save(
        &mut self,
        editor: Editor,
        values: BTreeMap<String, String>,
        conversation_id: Option<String>,
    ) -> Result<()> {
        #[cfg(test)]
        Self::test_phase("save-enter");
        match editor {
            editor @ (Editor::Connect | Editor::ConnectLocal) => {
                self.save_connection(editor, values).await
            }
            editor @ (Editor::CreateProfile
            | Editor::Join
            | Editor::CreateNetwork
            | Editor::Recovery
            | Editor::Member(_)) => self.save_identity(editor, values).await,
            editor @ (Editor::CreateBot
            | Editor::EditBot
            | Editor::BotModel(_)
            | Editor::Memory(_)
            | Editor::EditSchedule(_)
            | Editor::Schedule
            | Editor::Avatar) => self.save_bot(editor, values, conversation_id).await,
            editor @ (Editor::CreateTask
            | Editor::EditTask(..)
            | Editor::ChannelPost(_)
            | Editor::CreateChannel
            | Editor::EditChannel(..)
            | Editor::AssignTask(..)
            | Editor::QueuedEdit(_)
            | Editor::Answer(..)) => self.save_workspace(editor, values).await,
            editor @ (Editor::CloseSettings(_)
            | Editor::Logs
            | Editor::DecisionToken
            | Editor::NodeSettings
            | Editor::Triage
            | Editor::Provider
            | Editor::Relay) => self.save_settings(editor, values).await,
            Editor::Transfer => self.save_delivery(values).await,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn form_domains_reach_validation_on_the_native_actor_stack() -> Result<()> {
        let profile =
            std::env::temp_dir().join(format!("vibeface-form-boundaries-{}", uuid::Uuid::new_v4()));
        let host = crate::host::DesktopHost::open(profile.clone(), profile.join("runtime"), true)?;
        let password = "controlled boundary password";
        let key = ClientKey::create(&host.client_storage_dir(), password)?;
        drop(key);
        let (tx, mut events, _snapshots, stop, task) = Backend::spawn(
            host,
            &tokio::runtime::Handle::current(),
            "native-form-boundaries".into(),
        );
        let cases = [
            (
                Editor::Connect,
                BTreeMap::from([
                    ("passphrase".into(), password.into()),
                    ("transport".into(), "invalid".into()),
                ]),
                "接入方式须为",
            ),
            (
                Editor::CreateNetwork,
                BTreeMap::from([("passphrase".into(), password.into())]),
                "请先选择运行中的本机Node",
            ),
            (Editor::Avatar, BTreeMap::new(), "请先打开一个 Bot 的对话"),
            (
                Editor::AssignTask("missing-task".into(), 1),
                BTreeMap::from([("bot_id".into(), "missing-bot".into())]),
                "执行Bot未加载",
            ),
            (
                Editor::Provider,
                BTreeMap::from([("device_id".into(), "missing-node".into())]),
                "请先连接目标设备，再修改 Provider",
            ),
            (Editor::Transfer, BTreeMap::new(), "仅管理设备可准备订单"),
        ];
        for (editor, values, expected) in cases {
            tx.send(Command::Save {
                editor,
                values,
                conversation_id: None,
            })
            .await?;
            let error = tokio::time::timeout(Duration::from_secs(15), async {
                loop {
                    let event = events
                        .recv()
                        .await
                        .context("native form actor ended before validation")?;
                    let event = match event {
                        Event::Scoped { event, .. } => *event,
                        event => event,
                    };
                    if let Event::Error(error) = event {
                        return Ok::<_, anyhow::Error>(error);
                    }
                }
            })
            .await??;
            ensure!(
                error.contains(expected),
                "form reached an unexpected validation boundary: {error}"
            );
        }
        stop.send_replace(true);
        task.await?;
        drop(tx);
        std::fs::remove_dir_all(profile)?;
        Ok(())
    }
}
