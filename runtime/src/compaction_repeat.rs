//! Post-compaction re-read detection (#3013).
//!
//! Failure shape: every compaction SUCCEEDS (the thread comes back under the
//! trigger, so the unproductive-compaction counter never grows), the
//! compactor's summary and the post-compaction nudge both say not to re-read,
//! and the model re-reads the same files on its very next turn anyway. Each
//! turn re-fills the context, the next compaction empties it again, and the
//! run is unbounded: the cycle detector is warn-only over a short window, and
//! `bail_after_compactions` is off by default.
//!
//! What is counted: a compaction is a REPEAT when the first turn after it
//! inspects exactly the same (tool, target) set as the last turn before it.
//! Consecutive repeats are counted; any post-compaction turn that inspects
//! something else, or does anything but inspect (an edit, a command), ends
//! the run of repeats. Only inspection tools count ([`Tool::inspects_only`]):
//! re-editing or re-running is progress, re-reading after being told the
//! result is not.

use std::collections::BTreeSet;

use crate::lmstudio::ToolCall;
use crate::tools::Tool;
use crate::cycle_detector::canonical_args;

/// One inspected thing: the tool and its canonical arguments (the cycle
/// detector's own reading: a read is its path and range, a search its
/// pattern and path), so two reads of different ranges of one file differ.
pub type Inspected = (&'static str, String);

/// The (tool, target) set a turn inspected, or EMPTY when the turn did anything
/// other than inspect. An empty set is never a repeat: a turn that edits or
/// runs a command moved the work forward, so it cannot be a re-read.
pub fn inspected_set(calls: &[ToolCall]) -> BTreeSet<Inspected> {
    let mut set = BTreeSet::new();
    for call in calls {
        let Some(tool) = Tool::from_name(&call.function.name).filter(|t| t.inspects_only()) else {
            return BTreeSet::new();
        };
        set.insert((tool.name(), canonical_args(tool.name(), &call.function.arguments)));
    }
    set
}

/// Counts consecutive compactions whose next turn repeated the turn before.
#[derive(Debug, Default)]
pub struct CompactionRepeat {
    last: BTreeSet<Inspected>,
    reference: Option<BTreeSet<Inspected>>,
    consecutive: u32,
}

impl CompactionRepeat {
    pub fn new() -> Self {
        Self::default()
    }

    /// A compaction just installed: the turn before it is the reference the
    /// next turn is compared against. A compaction after a turn that was not
    /// a pure inspection has nothing to repeat and ends the run.
    pub fn compacted(&mut self) {
        if self.last.is_empty() {
            self.consecutive = 0;
            self.reference = None;
        } else {
            self.reference = Some(self.last.clone());
        }
    }

    /// One turn's inspected set (see [`inspected_set`]). Compared against the
    /// pre-compaction reference only when a compaction sits directly before
    /// this turn; a turn with no compaction before it leaves the count alone.
    pub fn record_turn(&mut self, set: BTreeSet<Inspected>) {
        if let Some(reference) = self.reference.take() {
            if !set.is_empty() && set == reference {
                self.consecutive = self.consecutive.saturating_add(1);
            } else {
                self.consecutive = 0;
            }
        }
        self.last = set;
    }

    /// Consecutive compactions followed by a repeat of the turn before them.
    pub fn consecutive(&self) -> u32 {
        self.consecutive
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn set(paths: &[&str]) -> BTreeSet<Inspected> {
        paths.iter().map(|p| ("read", (*p).to_string())).collect()
    }

    /// The #3013 shape: read the same files, compact, read them again.
    #[test]
    fn the_same_read_set_after_each_compaction_is_counted_across_compactions() {
        let mut d = CompactionRepeat::new();
        d.record_turn(set(&["a", "b", "c", "d"]));
        for expected in 1..=5 {
            d.compacted();
            d.record_turn(set(&["a", "b", "c", "d"]));
            assert_eq!(d.consecutive(), expected);
        }
    }

    #[test]
    fn a_post_compaction_turn_that_reads_something_else_ends_the_run() {
        let mut d = CompactionRepeat::new();
        d.record_turn(set(&["a"]));
        d.compacted();
        d.record_turn(set(&["a"]));
        assert_eq!(d.consecutive(), 1);
        d.compacted();
        d.record_turn(set(&["b"]));
        assert_eq!(d.consecutive(), 0, "a different read set is progress");
    }

    #[test]
    fn a_turn_that_did_not_only_inspect_is_never_a_repeat() {
        let mut d = CompactionRepeat::new();
        d.record_turn(BTreeSet::new());
        d.compacted();
        d.record_turn(BTreeSet::new());
        assert_eq!(d.consecutive(), 0, "an empty set matches nothing, not even itself");
    }

    #[test]
    fn a_turn_between_compactions_does_not_reset_the_count() {
        let mut d = CompactionRepeat::new();
        d.record_turn(set(&["a"]));
        d.compacted();
        d.record_turn(set(&["a"]));
        d.record_turn(set(&["a"]));
        assert_eq!(d.consecutive(), 1, "no compaction before it: compared to nothing");
        d.compacted();
        d.record_turn(set(&["a"]));
        assert_eq!(d.consecutive(), 2);
    }

    fn call(name: &str, args: &str) -> ToolCall {
        serde_json::from_value(serde_json::json!({
            "id": "c", "type": "function", "function": {"name": name, "arguments": args}
        }))
        .unwrap()
    }

    #[test]
    fn only_a_turn_of_pure_inspection_has_a_set() {
        let reads = [call("read", r#"{"path":"/w/a"}"#), call("read", r#"{"path":"/w/b"}"#)];
        assert_eq!(inspected_set(&reads).len(), 2);
        let mixed = [call("read", r#"{"path":"/w/a"}"#), call("bash", r#"{"command":"ls"}"#)];
        assert!(inspected_set(&mixed).is_empty(), "a command ran: not a re-read");
        let edit = [call("edit", r#"{"path":"/w/a"}"#)];
        assert!(inspected_set(&edit).is_empty(), "re-editing is progress");
        let ranges = [call("read", r#"{"path":"/w/a","offset":1,"limit":50}"#)];
        let later = [call("read", r#"{"path":"/w/a","offset":51,"limit":50}"#)];
        assert_ne!(inspected_set(&ranges), inspected_set(&later), "another range of one file is not a re-read");
    }
}
