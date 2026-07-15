//! The generalized "scan a data set for the first survivor a candidate
//! collapses into, per a pluggable match/merge strategy" procedure (#1352
//! Tier 2).
//!
//! Extracted from the PR-review pipeline's probe-flag dedup stage
//! (`darkmux-lab`'s `review.rs::dedup_flags`). Per #1352's own framing: the
//! matching ALGORITHM (review's "mechanism-family keying" — same file, same
//! mechanism family, overlapping referenced symbol, overlapping diff
//! anchor) is legitimately bespoke review domain logic and stays in
//! `review.rs` as a [`DedupStrategy`] impl; what generalizes is the
//! survivor-scan PROCEDURE around it — first-match-in-input-order,
//! aggregate-on-collapse, never silently drop — which has no review-specific
//! knowledge at all (no `ProbeFlag`, no diff text, no symbol extraction):
//! it is generic over the caller's own item type `T` and key type `K`.
//!
//! "Pluggable strategy" here means exactly that: any `DedupStrategy<T>` impl
//! the caller constructs and passes to [`dedup`] — there is deliberately no
//! runtime name-keyed registry (unlike [`super::super::registry::
//! StepKindRegistry`]'s `Arc<dyn StepKind>` map) because a registry over a
//! type-parameterized trait has no single monomorphic home, and with
//! exactly one real strategy today a registry would be speculative
//! complexity with no second caller to justify it (YAGNI) — add one, keyed
//! the same way `StepKindRegistry` is, if/when a second strategy needs
//! runtime (not compile-time) selection.

/// A named, pluggable dedup strategy over item type `T` (#1352 Tier 2). The
/// [`dedup`] procedure calls this trait's methods; the trait supplies the
/// domain-specific "are these the same finding, and how do they merge"
/// algorithm.
pub trait DedupStrategy<T> {
    /// Per-item key material this strategy needs to decide a match —
    /// derived once per item (via [`Self::key`]) so [`dedup`]'s survivor
    /// scan compares cheap derived keys, not the raw items repeatedly.
    type Key;

    /// Derive `item`'s dedup key.
    fn key(&self, item: &T) -> Self::Key;

    /// `true` iff `candidate`'s key matches an existing survivor's key —
    /// the strategy's whole "is this the same finding" predicate.
    fn matches(&self, survivor: &Self::Key, candidate: &Self::Key) -> bool;

    /// Fold a matched `candidate`'s key into the surviving key — called on
    /// every collapse so a LATER candidate can match against the
    /// aggregate, not just the first survivor that landed.
    fn merge_key(&self, survivor: &mut Self::Key, candidate: Self::Key);
}

/// [`dedup`]'s result: the surviving items (in input order, one per
/// distinct finding) plus the raw/deduped counts the caller's own
/// bookkeeping is typically sourced from.
pub struct DedupOutcome<T> {
    pub items: Vec<T>,
    pub raw: usize,
    pub deduped: usize,
}

/// Scan `items` in input order via `strategy`. Each item's key
/// ([`DedupStrategy::key`]) is compared against every already-kept
/// survivor's key, in order, for the FIRST one [`DedupStrategy::matches`]
/// accepts:
///
/// - **Match** — `on_collapse(survivor, candidate)` lets the caller fold the
///   candidate's own payload into the survivor (review: append the
///   candidate's charge text to the survivor's `also_flagged`), then the
///   survivor's key is updated via [`DedupStrategy::merge_key`] so a LATER
///   candidate can match against the aggregate.
/// - **No match** — the candidate becomes a new survivor. `on_new(&mut
///   candidate, &key)` runs first, letting the caller copy any
///   strategy-computed key data back onto the item itself before it's kept
///   (review: writes the computed diff anchor onto the surviving
///   `ProbeFlag`).
///
/// Never drops an item — every input item either becomes a survivor or is
/// folded into one via `on_collapse`; nothing is silently discarded.
pub fn dedup<T, S: DedupStrategy<T>>(
    items: Vec<T>,
    strategy: &S,
    mut on_new: impl FnMut(&mut T, &S::Key),
    mut on_collapse: impl FnMut(&mut T, T),
) -> DedupOutcome<T> {
    let raw = items.len();
    let mut keys: Vec<S::Key> = Vec::new();
    let mut out: Vec<T> = Vec::new();

    for item in items {
        let key = strategy.key(&item);
        let target = keys.iter().position(|k| strategy.matches(k, &key));
        match target {
            Some(i) => {
                strategy.merge_key(&mut keys[i], key);
                on_collapse(&mut out[i], item);
            }
            None => {
                let mut item = item;
                on_new(&mut item, &key);
                keys.push(key);
                out.push(item);
            }
        }
    }

    let deduped = out.len();
    DedupOutcome { items: out, raw, deduped }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug, Clone, PartialEq, Eq)]
    struct Item {
        group: &'static str,
        note: String,
        absorbed: Vec<String>,
        tagged_key: Option<&'static str>,
    }

    struct GroupStrategy;

    impl DedupStrategy<Item> for GroupStrategy {
        type Key = &'static str;

        fn key(&self, item: &Item) -> &'static str {
            item.group
        }

        fn matches(&self, survivor: &&'static str, candidate: &&'static str) -> bool {
            survivor == candidate
        }

        fn merge_key(&self, _survivor: &mut &'static str, _candidate: &'static str) {
            // Groups never change identity on merge — nothing to fold.
        }
    }

    fn item(group: &'static str, note: &str) -> Item {
        Item { group, note: note.to_string(), absorbed: Vec::new(), tagged_key: None }
    }

    #[test]
    fn same_group_collapses_in_input_order() {
        let items = vec![item("a", "first"), item("b", "other"), item("a", "second")];
        let outcome = dedup(
            items,
            &GroupStrategy,
            |it, key| it.tagged_key = Some(key),
            |survivor, candidate| survivor.absorbed.push(candidate.note),
        );
        assert_eq!(outcome.raw, 3);
        assert_eq!(outcome.deduped, 2);
        assert_eq!(outcome.items.len(), 2);
        assert_eq!(outcome.items[0].note, "first");
        assert_eq!(outcome.items[0].absorbed, vec!["second".to_string()]);
        assert_eq!(outcome.items[1].note, "other");
        assert!(outcome.items[1].absorbed.is_empty());
    }

    #[test]
    fn on_new_fires_for_every_survivor_not_for_collapsed_items() {
        let items = vec![item("a", "first"), item("a", "second")];
        let outcome = dedup(
            items,
            &GroupStrategy,
            |it, key| it.tagged_key = Some(key),
            |_survivor, _candidate| {},
        );
        assert_eq!(outcome.items.len(), 1);
        assert_eq!(outcome.items[0].tagged_key, Some("a"));
    }

    #[test]
    fn no_collapse_when_nothing_matches() {
        let items = vec![item("a", "1"), item("b", "2"), item("c", "3")];
        let outcome = dedup(items, &GroupStrategy, |_, _| {}, |_, _| {});
        assert_eq!(outcome.raw, 3);
        assert_eq!(outcome.deduped, 3);
    }

    #[test]
    fn empty_input_yields_empty_output() {
        let outcome: DedupOutcome<Item> = dedup(Vec::new(), &GroupStrategy, |_, _| {}, |_, _| {});
        assert_eq!(outcome.raw, 0);
        assert_eq!(outcome.deduped, 0);
        assert!(outcome.items.is_empty());
    }
}
