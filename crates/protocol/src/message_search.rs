//! Read-only search over committed, visible message text in one explicit scope.
use crate::workspace::WorkspaceSpeaker;
use serde::{Deserialize, Serialize};

pub const MESSAGE_SEARCH_DEFAULT_LIMIT: usize = 50;
pub const MESSAGE_SEARCH_MAX_LIMIT: usize = 100;

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum MessageSearchScope {
    BotMain { bot_id: String },
    Task { task_id: String },
    Channel { channel_id: String },
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct MessageSearchRequest {
    pub scope: MessageSearchScope,
    pub query: String,
    #[serde(default)]
    pub cursor: Option<String>,
    #[serde(default = "default_limit")]
    pub limit: usize,
}
fn default_limit() -> usize {
    MESSAGE_SEARCH_DEFAULT_LIMIT
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct MessageSearchItem {
    pub event_hash: String,
    pub message_id: String,
    pub conversation_id: String,
    pub rank: String,
    pub created_at_ms: i64,
    pub text: String,
    pub speaker: WorkspaceSpeaker,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct MessageSearchCoverage {
    pub source: MessageSearchSource,
    pub limit: usize,
}
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum MessageSearchSource {
    CommittedServerHistory,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct MessageSearchPage {
    pub scope: MessageSearchScope,
    pub query: String,
    pub items: Vec<MessageSearchItem>,
    pub next_cursor: Option<String>,
    pub complete: bool,
    pub coverage: MessageSearchCoverage,
}
