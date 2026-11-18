//! Owns the process tree and stderr reader used by the Claude and Pi runtimes.

use std::{fmt::Display, process::Stdio, time::Duration};
use tokio::{
    io::{AsyncRead, AsyncReadExt as _},
    process::{Child, ChildStdin, ChildStdout},
    task::JoinHandle,
};
use vibeface_bridge::{
    agent,
    failure::{ExecFailure, FailureKind},
    platform::tree,
};

const STDERR_BYTES: usize = 2_000;

pub(super) struct PipedChild {
    child: Child,
    tree: tree::Tree,
    reaped: bool,
    stderr: Option<JoinHandle<String>>,
}

impl PipedChild {
    pub(super) fn spawn(
        mut command: std::process::Command,
        key: &str,
        program: impl Display,
    ) -> Result<Self, ExecFailure> {
        command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let (tree, child) = tree::Tree::spawn(command, key, true).map_err(|error| {
            let kind = match error
                .downcast_ref::<std::io::Error>()
                .map(std::io::Error::kind)
            {
                Some(std::io::ErrorKind::NotFound) => FailureKind::MissingBinary,
                _ => FailureKind::Unknown,
            };
            ExecFailure::new(kind, format!("could not start {program}: {error:#}"))
        })?;
        Ok(Self {
            child,
            tree,
            reaped: false,
            stderr: None,
        })
    }

    pub(super) fn token(&self) -> &str {
        self.tree.token()
    }

    pub(super) fn root(&self) -> u32 {
        self.tree.root()
    }

    pub(super) fn take_pipes(&mut self) -> Option<(ChildStdin, ChildStdout)> {
        let (Some(stdin), Some(stdout), Some(stderr)) = (
            self.child.stdin.take(),
            self.child.stdout.take(),
            self.child.stderr.take(),
        ) else {
            return None;
        };
        self.stderr = Some(tokio::spawn(read_stderr(stderr)));
        Some((stdin, stdout))
    }

    /// Waits for a provider that has closed its input, then ends any remaining tree members.
    pub(super) async fn finish(&mut self, grace: Duration) -> bool {
        let _ = tokio::time::timeout(grace, tree::observe_exit(&mut self.child)).await;
        self.terminate().await
    }

    pub(super) async fn terminate(&mut self) -> bool {
        if !self.reaped {
            self.reaped = agent::terminate_process_tree(&mut self.child, &mut self.tree).await;
        }
        self.reaped
    }

    pub(super) async fn stderr(&mut self, within: Duration) -> String {
        let Some(reader) = self.stderr.as_mut() else {
            return String::new();
        };
        // Keep the handle owned if the caller cancels this wait. Timing out must end the reader.
        let tail = match tokio::time::timeout(within, &mut *reader).await {
            Ok(result) => result.unwrap_or_default(),
            Err(_) => {
                reader.abort();
                let _ = reader.await;
                String::new()
            }
        };
        self.stderr = None;
        tail
    }
}

impl Drop for PipedChild {
    fn drop(&mut self) {
        if !self.reaped && (cfg!(windows) || self.child.id() == Some(self.tree.root())) {
            self.tree.kill();
        }
        if let Some(stderr) = self.stderr.take() {
            stderr.abort();
        }
    }
}

async fn read_stderr(mut stderr: impl AsyncRead + Unpin) -> String {
    let mut tail = Vec::with_capacity(STDERR_BYTES);
    let mut buffer = [0; 4096];
    while let Ok(read @ 1..) = stderr.read(&mut buffer).await {
        let bytes = &buffer[read.saturating_sub(STDERR_BYTES)..read];
        let over = (tail.len() + bytes.len()).saturating_sub(STDERR_BYTES);
        tail.drain(..over);
        tail.extend_from_slice(bytes);
    }
    String::from_utf8_lossy(&tail).trim().to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn stderr_keeps_the_last_bytes_before_lossy_decoding_and_trimming() {
        let mut raw = vec![b'x'; 8192];
        raw.extend_from_slice("€".as_bytes());
        raw.extend(std::iter::repeat_n(b'y', 1995));
        raw.extend_from_slice(b" \n\t");
        let expected = format!("\u{fffd}\u{fffd}{}", "y".repeat(1995));
        assert_eq!(read_stderr(raw.as_slice()).await, expected);
        assert_eq!(read_stderr(&b"  message\r\n"[..]).await, "message");
    }

    #[cfg(unix)]
    mod unix {
        use super::*;
        use std::process::Command;
        use tokio::io::{AsyncBufReadExt as _, BufReader};
        use tokio::time::timeout;

        const LIMIT: Duration = Duration::from_secs(5);

        fn shell(script: &str) -> PipedChild {
            let mut command = Command::new("/bin/sh");
            command.args(["-c", script]);
            PipedChild::spawn(command, "test-child-pipes", "test child").unwrap()
        }

        async fn ready(stdout: ChildStdout) {
            let line = timeout(LIMIT, BufReader::new(stdout).lines().next_line())
                .await
                .unwrap()
                .unwrap();
            assert_eq!(line.as_deref(), Some("ready"));
        }

        #[tokio::test]
        async fn a_missing_pipe_drops_the_tree_even_before_a_provider_owns_it() {
            let mut child = shell("sleep 12 & printf 'ready\\n'; wait");
            ready(child.child.stdout.take().unwrap()).await;
            let stderr = child.child.stderr.take().unwrap();
            assert!(child.take_pipes().is_none());
            drop(child);
            // The descendant inherits stderr and will hold it open if only the root is killed.
            assert_eq!(timeout(LIMIT, read_stderr(stderr)).await.unwrap(), "");
        }

        #[tokio::test]
        async fn stderr_wait_cancellation_preserves_ownership_and_timeout_aborts_it() {
            let mut child = shell("printf 'ready\\n'; IFS= read -r line");
            let (stdin, stdout) = child.take_pipes().unwrap();
            ready(stdout).await;
            let reader = child.stderr.as_ref().unwrap().abort_handle();
            assert!(timeout(Duration::ZERO, child.stderr(LIMIT)).await.is_err());
            assert!(child.stderr.is_some());
            assert!(!reader.is_finished());
            assert_eq!(child.stderr(Duration::ZERO).await, "");
            assert!(reader.is_finished());
            assert!(child.stderr.is_none());
            drop(stdin);
            assert!(child.finish(LIMIT).await);
        }

        #[tokio::test]
        async fn finishing_after_exit_grace_terminates_a_child_that_ignores_stdin() {
            let mut child = shell("trap 'exit 0' TERM; printf 'ready\\n'; while :; do :; done");
            let (stdin, stdout) = child.take_pipes().unwrap();
            ready(stdout).await;
            drop(stdin);
            assert!(
                timeout(LIMIT, child.finish(Duration::from_millis(20)))
                    .await
                    .unwrap()
            );
            assert!(!tree::token_is_running(child.token()));
        }
    }
}
