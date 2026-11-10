//! Bounded conversation history. Cursors are opaque to clients.
use serde::{Deserialize, Serialize};

pub const HISTORY_DEFAULT_LIMIT: usize = 50;
pub const HISTORY_MAX_LIMIT: usize = 100;
pub const HISTORY_PAGE_BYTES: usize = 512 * 1024;
/// Leaves room for RPC metadata and encrypted transport framing below 4 MiB.
pub const HISTORY_RESPONSE_BYTES: usize = 3 * 1024 * 1024 - 64 * 1024;

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum HistorySection {
    Messages,
    Works,
    Changes,
    Questions,
    Sessions,
    Jobs,
    ScopeJobs,
    OwnerJobs,
    OwnerScopeJobs,
    OwnerMemoryConflicts,
    Checks,
    Deliveries,
    Unanswered,
    Skills,
    Schedules,
    Memory,
    MemoryConflicts,
    MemorySyncConflicts,
    Overwritten,
}

impl HistorySection {
    pub const ALL: [Self; 19] = [
        Self::Messages, Self::Works, Self::Changes, Self::Questions, Self::Sessions,
        Self::Jobs, Self::ScopeJobs, Self::OwnerJobs, Self::OwnerScopeJobs,
        Self::OwnerMemoryConflicts, Self::Checks, Self::Deliveries, Self::Unanswered,
        Self::Skills, Self::Schedules, Self::Memory, Self::MemoryConflicts,
        Self::MemorySyncConflicts, Self::Overwritten,
    ];

    pub fn snapshot_path(self) -> &'static [&'static str] {
        match self {
            Self::Messages => &["messages"], Self::Works => &["works"],
            Self::Changes => &["changes"], Self::Questions => &["questions"],
            Self::Sessions => &["sessions"], Self::Checks => &["checks"],
            Self::Deliveries => &["deliveries"], Self::Overwritten => &["overwritten"],
            Self::Jobs => &["scheduler", "jobs"], Self::ScopeJobs => &["scheduler", "scope_jobs"],
            Self::Unanswered => &["scheduler", "unanswered"],
            Self::OwnerJobs => &["scheduler", "remote", "jobs"],
            Self::OwnerScopeJobs => &["scheduler", "remote", "scope_jobs"],
            Self::OwnerMemoryConflicts => &["scheduler", "remote", "memory_conflicts"],
            Self::Skills => &["bot_settings", "skills"],
            Self::Schedules => &["bot_settings", "schedules"],
            Self::Memory => &["bot_settings", "memory"],
            Self::MemoryConflicts => &["bot_settings", "memory_conflicts"],
            Self::MemorySyncConflicts => &["bot_settings", "memory_sync_conflicts", "items"],
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct HistoryCoverage {
    pub next_cursor: Option<String>,
    pub complete: bool,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct HistoryPage<T> {
    pub conversation_id: String,
    pub section: HistorySection,
    pub history_authority: String,
    pub items: Vec<T>,
    pub next_cursor: Option<String>,
    pub complete: bool,
}

impl<T> HistoryPage<T> {
    pub fn coverage(&self) -> HistoryCoverage {
        HistoryCoverage {
            next_cursor: self.next_cursor.clone(),
            complete: self.complete,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct HistoryRequest {
    pub conversation_id: String,
    pub section: HistorySection,
    #[serde(default)]
    pub cursor: Option<String>,
    #[serde(default = "default_limit")]
    pub limit: usize,
}

fn default_limit() -> usize {
    HISTORY_DEFAULT_LIMIT
}
