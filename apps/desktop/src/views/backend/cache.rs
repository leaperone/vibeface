use super::*;
use crate::views::offline::{self, CacheScope};
impl Backend {
    fn projection_scope(&self) -> Result<CacheScope> {
        let client = self.client()?;
        Ok(CacheScope {
            profile_id: self.current_profile_id()?,
            network_id: client.network_id().into(),
            client_id: client.client_id().into(),
            target_node: client.target_node().into(),
        })
    }
    fn projection_path(&self) -> Result<PathBuf> {
        let target = self.client()?.target_node();
        ensure!(
            target.len() == 64
                && target
                    .bytes()
                    .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)),
            "离线视图Node标识无效"
        );
        Ok(self
            .client_directory()?
            .join("workspace-cache")
            .join(format!("{target}.json")))
    }
    pub(super) async fn invalidate_projection(&self) -> Result<()> {
        let path = self.projection_path()?;
        tokio::task::spawn_blocking(move || match std::fs::remove_file(path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error.into()),
        })
        .await?
    }
    pub(super) async fn save_projection(&self) -> Result<()> {
        if !self.state.online
            || !(self.state.synced || self.state.projection_valid)
            || !(self.client()?.can_write() || self.state.local_session)
        {
            return Ok(());
        }
        let scope = self.projection_scope()?;
        let snapshot = self.state.clone();
        let conversation = self.conversation.clone();
        let path = self.projection_path()?;
        tokio::task::spawn_blocking(move || {
            let bytes = offline::encode(&snapshot, &scope, conversation.as_deref())?;
            vibeface_bridge::platform::privatefs::create_dir(
                path.parent().context("离线视图缺少父目录")?,
            )?;
            vibeface_bridge::platform::privatefs::write_atomic(&path, &bytes)
        })
        .await?
    }
    pub(super) async fn load_projection(&mut self) -> Result<()> {
        let client = self.client()?.clone();
        let scope = self.projection_scope()?;
        let path = self.projection_path()?;
        let cached = tokio::task::spawn_blocking(move || -> Result<_> {
            if !path.try_exists()? {
                return Ok(None);
            }
            let bytes = vibeface_bridge::platform::privatefs::read_private_bounded(
                &path,
                offline::MAX_CACHE_BYTES as u64,
                "离线工作区视图",
            )?;
            Ok(Some(offline::decode(&bytes, &scope)?))
        })
        .await??;
        if let Some(cached) = cached {
            self.conversation = cached.conversation_id().map(str::to_owned);
            self.state = cached.into_snapshot(self.state.generation, client.can_save_locally());
        } else {
            self.state.online = false;
            self.state.synced = false;
            self.state.writable = false;
            self.state.can_save_locally = false;
            self.state.status="身份已在本机解锁，但此Node没有可验证工作区缓存。已有待发记录保留；连接后再读取输入目标。".into();
        }
        self.state.private_block = client.offline_block_reason().map(|reason| {
            match reason {
                vibeface_client::OfflineBlockReason::Revoked => "此身份已被撤销",
                vibeface_client::OfflineBlockReason::Forked => "已知记录存在分叉",
                vibeface_client::OfflineBlockReason::NotEnrolled => "此身份尚未加入个人网络",
                vibeface_client::OfflineBlockReason::NotMember => "已验证目录不包含此成员",
                vibeface_client::OfflineBlockReason::NodeRemoved => "原Node已从有效目录移除",
            }
            .to_owned()
        });
        if let Some(reason) = &self.state.private_block {
            self.state.can_save_locally = false;
            self.state.can_control = false;
            self.state.writable = false;
            self.state.status =
                format!("{reason}。这里只显示本机私有缓存与待发记录，不提供网络权限，不发送操作。");
        }
        self.state.drafts = load_draft_summaries(self.draft_dir()?).await?;
        self.state.pending = client
            .pending()
            .await?
            .into_iter()
            .map(|operation| Pending {
                id: operation.id,
                state: operation_label(operation.state).into(),
                target_node: operation.target_node,
            })
            .collect();
        self.state.can_configure_owner = self.host.snapshot().selected.is_some();
        self.publish();
        Ok(())
    }
}
