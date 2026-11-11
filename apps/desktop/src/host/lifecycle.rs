use super::model::{NodeIdentity, OwnedNode};
use anyhow::{Context, Result};
use std::{
    path::Path,
    process::{ExitStatus, Stdio},
    time::Duration,
};
use tokio::process::{Child, ChildStdin, Command};

pub(crate) struct OwnedChild {
    child: Child,
    stdin: Option<ChildStdin>,
    identity: NodeIdentity,
}
impl OwnedChild {
    pub(crate) fn start(core: &Path, identity: NodeIdentity) -> Result<Self> {
        let mut command = Command::new(core);
        command
            .arg("--data-dir")
            .arg(&identity.data_dir)
            .arg("--desktop-parent-stdin")
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(false);
        #[cfg(windows)]
        command.creation_flags(0x08000000);
        let mut child = command.spawn().context("无法启动本机 Core。")?;
        let stdin = child.stdin.take().context("自有 Core 缺少父进程管道。")?;
        Ok(Self {
            child,
            stdin: Some(stdin),
            identity,
        })
    }
    pub(crate) fn snapshot(&self) -> OwnedNode {
        OwnedNode {
            identity: self.identity.clone(),
            process_id: self.child.id(),
        }
    }
    pub(crate) fn poll(&mut self) -> Result<Option<ExitStatus>> {
        Ok(self.child.try_wait()?)
    }
    pub(crate) async fn stop(&mut self, grace: Duration) -> Result<()> {
        // EOF also happens if the GUI crashes or its OwnedChild is dropped.
        self.stdin.take();
        if tokio::time::timeout(grace, self.child.wait())
            .await
            .is_ok_and(|result| result.is_ok())
        {
            return Ok(());
        }
        if self.child.try_wait()?.is_some() {
            return Ok(());
        }
        // Tokio holds the unreaped child, so escalation never resolves an arbitrary PID.
        self.child
            .start_kill()
            .context("无法停止 App 自有 Core。")?;
        tokio::time::timeout(Duration::from_secs(2), self.child.wait())
            .await
            .context("无法确认自有 Core 已退出，仍保留进程所有权。")??;
        Ok(())
    }
}
