//! A Codex turn as the App Server reports it, read in one place.
use crate::failure::ExecFailure;
use serde_json::Value;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TurnStatus {
    InProgress,
    Completed,
    Interrupted,
    Failed,
    /// A status this build does not know, kept as Codex wrote it.
    Other(String),
}

impl TurnStatus {
    pub fn of(status: &str) -> Self {
        match status {
            "inProgress" => Self::InProgress,
            "completed" => Self::Completed,
            "interrupted" => Self::Interrupted,
            "failed" => Self::Failed,
            other => Self::Other(other.into()),
        }
    }

    pub fn as_str(&self) -> &str {
        match self {
            Self::InProgress => "inProgress",
            Self::Completed => "completed",
            Self::Interrupted => "interrupted",
            Self::Failed => "failed",
            Self::Other(status) => status,
        }
    }

    /// Completed, interrupted or failed: the turn will not change again.
    pub fn ended(&self) -> bool {
        matches!(self, Self::Completed | Self::Interrupted | Self::Failed)
    }
}

/// `status` is `None` when the turn carries none; `error` is the turn's own error, if any.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TurnSnapshot {
    pub id: Option<String>,
    pub status: Option<TurnStatus>,
    pub error: Option<ExecFailure>,
}

/// Reads a turn object, as `thread/read` lists it or `turn/completed` carries it.
pub fn turn_snapshot(turn: &Value) -> TurnSnapshot {
    TurnSnapshot {
        id: turn["id"].as_str().map(str::to_owned),
        status: turn["status"].as_str().map(TurnStatus::of),
        error: ExecFailure::of_turn_error(&turn["error"]),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::failure::FailureKind;
    use serde_json::json;

    #[test]
    fn statuses_read_back_as_written() {
        for (text, status, ended) in [
            ("inProgress", TurnStatus::InProgress, false),
            ("completed", TurnStatus::Completed, true),
            ("interrupted", TurnStatus::Interrupted, true),
            ("failed", TurnStatus::Failed, true),
            ("paused", TurnStatus::Other("paused".into()), false),
        ] {
            assert_eq!(TurnStatus::of(text), status);
            assert_eq!(status.as_str(), text);
            assert_eq!(status.ended(), ended, "{text}");
        }
    }

    #[test]
    fn a_snapshot_reads_id_status_and_error() {
        let turn =
            json!({"id":"t-1","status":"failed","error":{"message":"429 Too Many Requests"}});
        let snapshot = turn_snapshot(&turn);
        assert_eq!(snapshot.id.as_deref(), Some("t-1"));
        assert_eq!(snapshot.status, Some(TurnStatus::Failed));
        assert_eq!(
            snapshot.error.map(|error| error.kind),
            Some(FailureKind::Quota)
        );
        let bare = turn_snapshot(&json!({"id":"t-2"}));
        assert_eq!((bare.status, bare.error), (None, None));
        assert_eq!(
            turn_snapshot(&json!({"status":"completed","error":null})).error,
            None
        );
    }
}
