use anyhow::{Context, Result};
use std::io::{self, Read};
use tokio::sync::oneshot;

pub(super) enum DesktopParent {
    Independent,
    Owned(oneshot::Receiver<io::Result<()>>),
}

impl DesktopParent {
    pub(super) fn watch(enabled: bool) -> Result<Self> {
        if !enabled {
            return Ok(Self::Independent);
        }
        Self::from_reader(io::stdin())
    }

    fn from_reader(reader: impl Read + Send + 'static) -> Result<Self> {
        let (sender, receiver) = oneshot::channel();
        // A standard thread does not hold up Tokio's shutdown when another stop reason wins.
        // Only the desktop owns the write end; no command payload travels on this pipe.
        std::thread::Builder::new()
            .name("desktop-parent".into())
            .spawn(move || {
                let mut reader = reader;
                let mut byte = [0];
                let result = loop {
                    match reader.read(&mut byte) {
                        Ok(0) => break Ok(()),
                        Ok(_) => {
                            break Err(io::Error::new(
                                io::ErrorKind::InvalidData,
                                "desktop lifetime pipe does not accept commands",
                            ));
                        }
                        Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                        Err(error) => break Err(error),
                    }
                };
                let _ = sender.send(result);
            })
            .context("watch desktop parent lifetime")?;
        Ok(Self::Owned(receiver))
    }

    pub(super) async fn closed(&mut self) -> Result<()> {
        match self {
            Self::Independent => std::future::pending().await,
            Self::Owned(receiver) => receiver
                .await
                .context("desktop parent watcher ended unexpectedly")?
                .context("read desktop parent lifetime"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{io::Write, time::Duration};

    #[tokio::test]
    async fn owned_core_waits_until_every_parent_writer_closes() -> Result<()> {
        let (reader, writer) = io::pipe()?;
        let another_writer = writer.try_clone()?;
        let mut parent = DesktopParent::from_reader(reader)?;
        assert!(
            tokio::time::timeout(Duration::from_millis(20), parent.closed())
                .await
                .is_err()
        );
        drop(writer);
        assert!(
            tokio::time::timeout(Duration::from_millis(20), parent.closed())
                .await
                .is_err()
        );
        drop(another_writer);
        tokio::time::timeout(Duration::from_secs(2), parent.closed()).await??;
        Ok(())
    }

    #[tokio::test]
    async fn standalone_core_has_no_parent_pipe_shutdown() -> Result<()> {
        let mut parent = DesktopParent::watch(false)?;
        assert!(
            tokio::time::timeout(Duration::from_millis(20), parent.closed())
                .await
                .is_err()
        );
        Ok(())
    }

    #[tokio::test]
    async fn lifetime_pipe_rejects_commands_instead_of_processing_them() -> Result<()> {
        let (reader, mut writer) = io::pipe()?;
        let mut parent = DesktopParent::from_reader(reader)?;
        writer.write_all(b"command")?;
        let error = tokio::time::timeout(Duration::from_secs(2), parent.closed())
            .await?
            .expect_err("a lifetime pipe is not a control endpoint");
        assert!(error.to_string().contains("desktop parent lifetime"));
        assert!(format!("{error:#}").contains("does not accept commands"));
        Ok(())
    }
}
