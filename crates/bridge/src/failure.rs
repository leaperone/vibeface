//! Why an execution failed, as the person can act on it. This repository's own failures name
//! their kind where they happen; the wording of upstream errors (Cursor SDK, Codex App Server)
//! is recognized here and nowhere else.
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FailureKind {
    /// The AI provider has no key, or it is malformed.
    MissingKey,
    /// The provider refused the key or login.
    AuthFailed,
    /// A usage or rate limit was reached.
    Quota,
    /// The provider's program is not on this computer.
    MissingBinary,
    /// Executing work is turned off on this device for the provider it asked for.
    ExecutionDisabled,
    /// No model is chosen, or the provider does not know it.
    NoModel,
    /// Another Codex process is writing the thread.
    WriterBusy,
    /// The provider refused the request; nothing ran.
    Rejected,
    /// The provider could not be reached, or the connection broke.
    Transport,
    /// The provider did not answer in time.
    TimedOut,
    /// The device restarted while the turn ran.
    Restarted,
    /// The process running the turn ended unexpectedly.
    Lost,
    /// None of the above.
    Unknown,
}

/// `detail` is the original text, for technical details only; `kind` is what a person reads.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExecFailure {
    pub kind: FailureKind,
    pub detail: String,
}

/// Upstream wording, lowercase, by the kind it means; the first match wins.
const UPSTREAM: &[(FailureKind, &[&str])] = &[
    (FailureKind::WriterBusy, &["already has an active writer"]),
    (
        FailureKind::MissingKey,
        &[
            "api_key is required",
            "api_key is missing",
            "missing environment variable",
        ],
    ),
    (
        FailureKind::MissingBinary,
        &["executable was not found", "command not found", "enoent"],
    ),
    (
        FailureKind::Quota,
        &["usage limit", "rate limit", "quota", "too many requests"],
    ),
    (
        FailureKind::AuthFailed,
        &[
            "unauthorized",
            "authentication failed",
            "not logged in",
            "invalid api key",
        ],
    ),
    (
        FailureKind::NoModel,
        &[
            "model_not_found",
            "model not found",
            "unknown model",
            "model is not supported",
            "does not exist or you do not have access",
        ],
    ),
];

/// Codex refuses to resume a thread whose first turn has not written its rollout yet.
const NO_ROLLOUT: &str = "no rollout found";

fn recognize(detail: &str) -> Option<FailureKind> {
    let detail = detail.to_lowercase();
    UPSTREAM
        .iter()
        .find(|(_, needles)| needles.iter().any(|needle| detail.contains(needle)))
        .map(|(kind, _)| *kind)
}

/// Whether Codex refused because the thread has no rollout yet; retrying later helps.
pub(crate) fn no_rollout(detail: &str) -> bool {
    detail.contains(NO_ROLLOUT)
}

impl ExecFailure {
    pub fn new(kind: FailureKind, detail: impl Into<String>) -> Self {
        Self {
            kind,
            detail: detail.into(),
        }
    }

    /// Text an upstream service produced, by its wording, or `otherwise`.
    pub fn upstream(detail: impl Into<String>, otherwise: FailureKind) -> Self {
        let detail = detail.into();
        Self {
            kind: recognize(&detail).unwrap_or(otherwise),
            detail,
        }
    }

    /// The `error` of a Codex turn, by its `codexErrorInfo` and then its message; `None` when
    /// the turn carries no error.
    pub fn of_turn_error(error: &Value) -> Option<Self> {
        let detail = error["message"].as_str()?.to_owned();
        let info = &error["codexErrorInfo"];
        let named = match info.as_str() {
            Some("usageLimitExceeded" | "sessionBudgetExceeded") => Some(FailureKind::Quota),
            Some("unauthorized") => Some(FailureKind::AuthFailed),
            Some("serverOverloaded" | "internalServerError") => Some(FailureKind::Transport),
            // The other variants carry details in an object named after them.
            None if info.as_object().is_some_and(|variant| {
                variant.keys().any(|name| {
                    matches!(
                        name.as_str(),
                        "httpConnectionFailed"
                            | "responseStreamConnectionFailed"
                            | "responseStreamDisconnected"
                            | "responseTooManyFailedAttempts"
                    )
                })
            }) =>
            {
                Some(FailureKind::Transport)
            }
            _ => None,
        };
        let otherwise = if info == "badRequest" {
            FailureKind::Rejected
        } else {
            FailureKind::Unknown
        };
        Some(match named {
            Some(kind) => Self { kind, detail },
            None => Self::upstream(detail, otherwise),
        })
    }
}

impl std::fmt::Display for ExecFailure {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.detail)
    }
}

impl std::error::Error for ExecFailure {}

#[cfg(test)]
mod tests {
    use super::*;
    use FailureKind::*;
    use serde_json::json;

    #[test]
    fn upstream_wording_names_its_kind() {
        for (text, kind) in [
            ("Missing environment variable: CURSOR_API_KEY", MissingKey),
            ("[unauthenticated] api_key is required", MissingKey),
            ("Error: spawn cursor-agent ENOENT", MissingBinary),
            ("sh: cursor-agent: command not found", MissingBinary),
            ("You've hit your usage limit. Try again later.", Quota),
            ("429 Too Many Requests", Quota),
            ("401 Unauthorized", AuthFailed),
            ("Not logged in. Run codex login.", AuthFailed),
            ("Invalid API key provided", AuthFailed),
            ("model_not_found: gpt-9", NoModel),
            (
                "The model `gpt-9` does not exist or you do not have access to it.",
                NoModel,
            ),
            (
                "thread t-1 already has an active writer in another process",
                WriterBusy,
            ),
            ("the agent wrote a broken patch", Unknown),
        ] {
            assert_eq!(ExecFailure::upstream(text, Unknown).kind, kind, "{text}");
        }
        assert_eq!(
            ExecFailure::upstream("no such method", Rejected).kind,
            Rejected
        );
        assert_eq!(ExecFailure::upstream("x", Unknown).detail, "x");
    }

    #[test]
    fn a_codex_turn_error_names_its_kind_by_its_info_first() {
        for (error, kind) in [
            (
                json!({"message":"limit","codexErrorInfo":"usageLimitExceeded"}),
                Quota,
            ),
            (
                json!({"message":"denied","codexErrorInfo":"unauthorized"}),
                AuthFailed,
            ),
            (
                json!({"message":"stream","codexErrorInfo":{"responseStreamDisconnected":{"httpStatusCode":null}}}),
                Transport,
            ),
            (
                json!({"message":"busy","codexErrorInfo":"serverOverloaded"}),
                Transport,
            ),
            (
                json!({"message":"The model `x` does not exist or you do not have access to it.","codexErrorInfo":"badRequest"}),
                NoModel,
            ),
            (
                json!({"message":"bad input","codexErrorInfo":"badRequest"}),
                Rejected,
            ),
            (json!({"message":"rate limit reached"}), Quota),
            (
                json!({"message":"context window","codexErrorInfo":"contextWindowExceeded"}),
                Unknown,
            ),
        ] {
            let failure = ExecFailure::of_turn_error(&error).expect("an error");
            assert_eq!(failure.kind, kind, "{error}");
            assert_eq!(failure.detail, error["message"].as_str().unwrap());
        }
        assert_eq!(ExecFailure::of_turn_error(&Value::Null), None);
    }

    #[test]
    fn a_failure_serializes_its_kind_in_snake_case() {
        let failure = ExecFailure::new(ExecutionDisabled, "off");
        assert_eq!(
            serde_json::to_value(&failure).unwrap(),
            json!({"kind":"execution_disabled","detail":"off"})
        );
    }
}
