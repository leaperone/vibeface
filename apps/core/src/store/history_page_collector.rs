//! Page consumption shared by readers that bound scanned candidates, including filtered rows.
use super::conversation_history::{HistoryBoundary, history_page};
use anyhow::Result;
use serde::Serialize;
use vibeface_protocol::history::{HISTORY_PAGE_BYTES, HistoryPage, HistorySection};

/// Serialized items include one separator byte each. The first item may exceed the soft
/// budget; `history_page` still enforces the hard response limit on the complete page.
#[derive(Default)]
pub(super) struct SerializedByteBudget {
    used: usize,
}

impl SerializedByteBudget {
    pub(super) fn try_include(&mut self, item: &impl Serialize) -> Result<bool> {
        let size = serde_json::to_vec(item)?.len() + 1;
        if self.used != 0 && self.used + size > HISTORY_PAGE_BYTES {
            return Ok(false);
        }
        self.used += size;
        Ok(true)
    }
}

pub(super) struct HistoryCollector<T> {
    boundary: Option<HistoryBoundary>,
    items: Vec<T>,
    bytes: SerializedByteBudget,
    candidate_limit: usize,
    scanned: usize,
    has_more: bool,
}

impl<T: Serialize> HistoryCollector<T> {
    pub(super) fn new(before: Option<HistoryBoundary>, candidate_limit: usize) -> Self {
        Self {
            boundary: before,
            items: Vec::new(),
            bytes: SerializedByteBudget::default(),
            candidate_limit,
            scanned: 0,
            has_more: false,
        }
    }

    /// Call only after fetching a candidate, before decoding or projecting it. A full
    /// page is incomplete only if that extra candidate actually exists.
    pub(super) fn can_scan_candidate(&mut self) -> bool {
        if self.scanned == self.candidate_limit {
            self.has_more = true;
            return false;
        }
        true
    }

    pub(super) fn remaining_with_lookahead(&self) -> usize {
        self.candidate_limit - self.scanned + 1
    }

    /// Filtered candidates consume a slot and advance the boundary. A byte-rejected
    /// item consumes neither, so the continuation will retrieve that whole item.
    pub(super) fn include(&mut self, boundary: HistoryBoundary, item: Option<T>) -> Result<bool> {
        if let Some(item) = &item
            && !self.bytes.try_include(item)?
        {
            self.has_more = true;
            return Ok(false);
        }
        self.boundary = Some(boundary);
        self.scanned += 1;
        if let Some(item) = item {
            self.items.push(item);
        }
        Ok(true)
    }

    pub(super) fn finish(
        mut self,
        network: &str,
        conversation: &str,
        section: HistorySection,
        authority: String,
    ) -> Result<HistoryPage<T>> {
        self.items.reverse();
        history_page(
            network,
            conversation,
            section,
            authority,
            self.boundary,
            self.items,
            self.has_more,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn byte_budget_accepts_exact_fit_and_an_oversized_first_item() -> Result<()> {
        let mut exact = SerializedByteBudget::default();
        // A JSON string has two quote bytes and the page accounts for one separator.
        assert!(exact.try_include(&"x".repeat(HISTORY_PAGE_BYTES - 3))?);
        assert!(!exact.try_include(&"")?);
        let mut oversized = SerializedByteBudget::default();
        assert!(oversized.try_include(&"x".repeat(HISTORY_PAGE_BYTES))?);
        assert!(!oversized.try_include(&"")?);
        Ok(())
    }
}
