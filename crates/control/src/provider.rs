//! Shared provider wire identity; execution and installation stay with their consumers.
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Provider {
    #[default]
    Codex,
    Cursor,
    Claude,
    Pi,
    Opencode,
}

impl Provider {
    pub const fn name(self) -> &'static str {
        match self {
            Self::Codex => "codex",
            Self::Cursor => "cursor",
            Self::Claude => "claude",
            Self::Pi => "pi",
            Self::Opencode => "opencode",
        }
    }
}
