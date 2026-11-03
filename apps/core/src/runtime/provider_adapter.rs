//! Evidence that stays at the local provider boundary.
//!
//! Provider output is untrusted diagnostic text. The types here keep it bounded and scrubbed
//! before a probe can attach a safe category to a public readiness result.

use std::process::ExitStatus;

const STDERR_TAIL_LIMIT: usize = 512;
const STDERR_CAPTURE_LIMIT: usize = STDERR_TAIL_LIMIT * 8;

/// The safe end of a provider's stderr stream. The raw stream never leaves the child boundary.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(super) struct ScrubbedStderrTail {
    text: String,
    category: Option<&'static str>,
}

impl ScrubbedStderrTail {
    pub(super) fn from_bytes(bytes: &[u8]) -> Self {
        let text = String::from_utf8_lossy(bytes);
        let category = classify_error(text.as_ref());
        let text = crate::logging::scrub(&text);
        let mut lines = Vec::new();
        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            let line = if looks_sensitive(line) {
                "[redacted]".to_owned()
            } else {
                redact_path_tokens(line)
            };
            if !line.is_empty() {
                lines.push(line);
            }
        }
        let text = lines.join(" ");
        let tail = text
            .chars()
            .rev()
            .take(STDERR_TAIL_LIMIT)
            .collect::<String>()
            .chars()
            .rev()
            .collect();
        Self {
            text: tail,
            category,
        }
    }

    /// Read at most a small rolling window while the child keeps its stderr pipe drained.
    pub(super) async fn read<R>(mut reader: R) -> Self
    where
        R: tokio::io::AsyncRead + Unpin,
    {
        use tokio::io::AsyncReadExt as _;

        let mut bytes = Vec::with_capacity(STDERR_CAPTURE_LIMIT);
        let mut chunk = [0_u8; 4096];
        while let Ok(read) = reader.read(&mut chunk).await {
            if read == 0 {
                break;
            }
            bytes.extend_from_slice(&chunk[..read]);
            if bytes.len() > STDERR_CAPTURE_LIMIT {
                let excess = bytes.len() - STDERR_CAPTURE_LIMIT;
                bytes.drain(..excess);
            }
        }
        Self::from_bytes(&bytes)
    }

    pub(super) fn is_empty(&self) -> bool {
        self.text.is_empty()
    }

    /// Categorize without returning provider wording, credentials, or filesystem paths.
    pub(super) fn safe_error(&self) -> Option<&'static str> {
        self.category
    }
}

fn classify_error(text: &str) -> Option<&'static str> {
    let lower = text.to_ascii_lowercase();
    if lower.trim().is_empty() {
        return None;
    }
    Some(
        if lower.contains("auth")
            || lower.contains("credential")
            || lower.contains("api key")
            || lower.contains("unauthor")
            || lower.contains("forbidden")
            || lower.contains("401")
            || lower.contains("403")
        {
            "authentication_failed"
        } else if lower.contains("model") || lower.contains("no such") {
            "model_unavailable"
        } else if lower.contains("timeout") || lower.contains("timed out") {
            "provider_timeout"
        } else if lower.contains("connect")
            || lower.contains("network")
            || lower.contains("refused")
            || lower.contains("unreachable")
        {
            "provider_unreachable"
        } else if lower.contains("permission") || lower.contains("denied") {
            "permission_denied"
        } else {
            "provider_error"
        },
    )
}

fn looks_sensitive(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    crate::model::looks_secret(text)
        || [
            "api key",
            "apikey",
            "authorization",
            "bearer",
            "credential",
            "password",
            "passwd",
            "secret",
            "token",
        ]
        .iter()
        .any(|marker| lower.contains(marker))
}

fn redact_path_tokens(text: &str) -> String {
    text.split_whitespace()
        .map(|token| {
            if token.contains('/')
                || token.contains('\\')
                || token.starts_with('~')
                || token.contains(":\\")
            {
                "[local-path]"
            } else {
                token
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct TerminationEvidence {
    pub(super) success: bool,
    pub(super) code: Option<i32>,
    pub(super) signal: Option<i32>,
}

impl From<ExitStatus> for TerminationEvidence {
    fn from(status: ExitStatus) -> Self {
        #[cfg(unix)]
        use std::os::unix::process::ExitStatusExt as _;

        Self {
            success: status.success(),
            code: status.code(),
            #[cfg(unix)]
            signal: status.signal(),
            #[cfg(not(unix))]
            signal: None,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum PendingDisconnectClassification {
    NoPending,
    Unknown,
}

/// A pending request remains an unknown result when the provider disconnects.
pub(super) fn classify_pending_disconnect(
    pending: bool,
    _termination: Option<&TerminationEvidence>,
) -> PendingDisconnectClassification {
    if pending {
        PendingDisconnectClassification::Unknown
    } else {
        PendingDisconnectClassification::NoPending
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stderr_tail_is_bounded_scrubbed_and_keeps_the_end() {
        let tail = ScrubbedStderrTail::from_bytes(
            format!(
                "{}\nAuthorization: Bearer fixture-secret\nfailed at /private/path/provider",
                "prefix ".repeat(200)
            )
            .as_bytes(),
        );
        assert!(tail.text.chars().count() <= STDERR_TAIL_LIMIT);
        assert!(!tail.text.contains("fixture-secret"));
        assert!(!tail.text.contains("/private/path"));
        assert_eq!(tail.safe_error(), Some("authentication_failed"));
    }

    #[test]
    fn termination_evidence_keeps_exit_facts() {
        let status = std::process::Command::new("sh")
            .args(["-c", "exit 7"])
            .status()
            .expect("fixture process");
        let evidence = TerminationEvidence::from(status);
        assert!(!evidence.success);
        assert_eq!(evidence.code, Some(7));
    }

    #[test]
    fn pending_disconnect_is_unknown_only_when_a_request_was_pending() {
        let evidence = TerminationEvidence {
            success: false,
            code: Some(1),
            signal: None,
        };
        assert_eq!(
            classify_pending_disconnect(false, Some(&evidence)),
            PendingDisconnectClassification::NoPending
        );
        assert_eq!(
            classify_pending_disconnect(true, Some(&evidence)),
            PendingDisconnectClassification::Unknown
        );
    }
}
