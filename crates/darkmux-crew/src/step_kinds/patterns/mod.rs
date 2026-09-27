//! Tier 2 patterns (#1352): genuinely generic, reusable control-flow
//! *shapes* — the outer procedure is shared infrastructure; the
//! domain-specific *algorithm* plugs in as a caller-supplied strategy
//! (a closure, or a small trait impl).
//!
//! Contrast with the sibling tiers, per #1352's physical-enforcement rule:
//!
//! - `step_kinds::builtins` (Tier 1) — generic AND config-driven; no new
//!   control flow, just values on an existing kind. The default; check
//!   there first, always, before reaching for anything in this module.
//! - `step_kinds::patterns` (here, Tier 2) — the procedure's *shape* is new
//!   and reusable, but the algorithm inside it is supplied per caller.
//!   Nothing here depends on any mission's own types (this crate
//!   has no `darkmux-lab` dependency, and never will — see the workspace's
//!   crate-dependency direction) — that's what keeps a Tier 2 pattern
//!   actually reusable rather than one mission's code with extra ceremony.
//! - A mission's own module (e.g. `darkmux-lab`'s `review.rs`, this
//!   crate's own `coder_phase.rs` caller in the `darkmux` binary) — Tier 3,
//!   genuinely bespoke, stays physically co-located with the mission that
//!   owns it, never migrates here "just in case."
//!
//! One pattern lives here today:
//!
//! - [`plan_sites::plan_site_units`] — the "prefilter hits over a source,
//!   window each hit, pack windows into sizing-bounded units" procedure
//!   (#2310 P4b), generalized from the crawl planner so a diff-scoped
//!   planner (the `review` config's `plan.sites` step) plugs in a different
//!   SOURCE without duplicating the windowing/packing math.
//!
//! (4.0) Two earlier patterns, `multi_pass_confirm` and `dedup`, were
//! generalized from the PR-review funnel's judge and dedup stages. Their
//! only consumer was that funnel, deleted in #2310 P4d, so they were
//! deleted too.

pub mod plan_sites;
