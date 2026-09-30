//! The run of consecutive unproductive compactions (#2793, #2805).
//!
//! A compaction is UNPRODUCTIVE when it leaves the thread at or above the
//! occupancy that triggered it, so the next turn compacts again. A run of
//! them is an EPISODE: any compaction that gets below the trigger, or any
//! turn that needed no compaction at all, ends it. The loop escalates when
//! the episode reaches `UNPRODUCTIVE_COMPACTION_TURNS`.

#[derive(Debug, Default)]
pub struct UnproductiveCompactions {
    consecutive: u32,
}

impl UnproductiveCompactions {
    pub fn new() -> Self {
        Self::default()
    }

    /// One installed compaction: `tokens_after` is the thread's size after
    /// it, `trigger` the occupancy that summoned it. Returns the run length.
    pub fn record(&mut self, tokens_after: u32, trigger: u32) -> u32 {
        self.consecutive = if tokens_after >= trigger { self.consecutive.saturating_add(1) } else { 0 };
        self.consecutive
    }

    /// A turn that needed no compaction: the thread came back under the
    /// line on its own, so the episode is over. Without this the count would
    /// carry across a resolved episode and fire early on the next one.
    pub fn end_episode(&mut self) {
        self.consecutive = 0;
    }

    pub fn count(&self) -> u32 {
        self.consecutive
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn consecutive_unproductive_compactions_accumulate() {
        let mut u = UnproductiveCompactions::new();
        assert_eq!((u.record(900, 500), u.record(700, 500), u.record(500, 500)), (1, 2, 3));
    }

    #[test]
    fn a_productive_compaction_ends_the_episode() {
        let mut u = UnproductiveCompactions::new();
        u.record(900, 500);
        u.record(900, 500);
        assert_eq!(u.record(100, 500), 0);
    }

    /// 3 unproductive, one turn that needed no compaction, 3 more: two short
    /// episodes, never one of six.
    #[test]
    fn an_uncompacted_turn_ends_the_episode() {
        let mut u = UnproductiveCompactions::new();
        for _ in 0..3 {
            u.record(900, 500);
        }
        u.end_episode();
        assert_eq!(u.count(), 0);
        for expected in 1..=3 {
            assert_eq!(u.record(900, 500), expected, "the count restarts after the healthy turn");
        }
    }
}
