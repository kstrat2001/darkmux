# StepKind tiering

Claude Code loads this file when it works in this directory. It holds the rules for the code here, moved verbatim from the root `CLAUDE.md` (#3133), which keeps a one-line pointer to it.

## StepKind tiering — physical enforcement (#1352)

Mission work runs as `Task`/`Step` graphs (`darkmux-crew`'s `scheduler`), and a `Step`'s `kind` field resolves to a registered Rust implementation of the `StepKind` trait. #1230's redesign arc grew nine of these in one pass — much faster than this codebase's own precedent for an extension point (`WorkloadProvider` stayed at three implementations across a long history) — which is exactly the "hard-wire every use case" failure mode this project exists to fight at the model-orchestration layer, recurring at the code-extension layer instead. #1352 stopped that drift with a real decision procedure, enforced PHYSICALLY (a directory a fresh session can read, not a rule that only lives in a paragraph and gets skipped under time pressure):

**The test:** a new `StepKind` is justified only when the CONTROL FLOW itself is genuinely new — not when only the DATA differs (that's config), and not when only the internal ALGORITHM differs while the outer procedure shape stays the same (that's a pluggable strategy inside an existing generic kind, not a new type).

**The three physical locations, and what each one means:**

```
crates/darkmux-crew/src/step_kinds/
    builtins.rs   — Tier 1: generic, config-driven, no new control flow.
                    dispatch.internal, dispatch.single_shot,
                    procedural.shell, procedural.noop. THE DEFAULT — check
                    here first, always, before writing new code.
    patterns/     — Tier 2: a genuinely new, reusable control-flow SHAPE,
                    with the domain-specific ALGORITHM plugged in as a
                    caller-supplied strategy (deliberately NO runtime
                    name-keyed strategy registry). plan_sites.rs (the
                    "prefilter hits over a source, window each hit, pack
                    windows into sizing-bounded units" procedure, shared
                    by the crawl planner and the diff-scoped `plan.sites`
                    step). Nothing here depends on any mission's own
                    types, which is what keeps a Tier 2 pattern actually
                    reusable rather than one mission's code with extra
                    ceremony. (The funnel-era multi_pass_confirm.rs and
                    dedup.rs were deleted in 5.0: their only consumer was
                    the funnel #2310 P4d removed.)
    types.rs      — the StepKind trait itself.
    registry.rs   — StepKindRegistry.
```

Tier 3 — genuinely bespoke, single-purpose kinds — **never lives in `darkmux-crew` at all.** It stays physically co-located with the mission module that owns it: today's one surviving exemplar is the coder-phase pipeline's worktree/coder/verify kinds, living in `src/coder_phase.rs` (the launch-owned module — `mission run` retired in #1426, ship-4). (Historical: this section used to name a second exemplar — the PR-review pipeline's bundle/probe/dedup/judge/verify/synthesis kinds in `crates/darkmux-lab/src/lab/review.rs` — but that bespoke pipeline was deleted in #2310 P4d; `review` now runs on the shared Tier-1/Tier-2 building blocks like any other mission config, see `crates/darkmux-crew/src/step_kinds/` and `crates/darkmux-lab/src/crawl/` in the file map above.) This is reserved for when a second plausible use case genuinely isn't visible yet — revisit if one shows up, same as any other "not yet, but named" call.

**The physical location IS the enforceable test.** Is this in `step_kinds/builtins.rs`? Config it. Is it in `step_kinds/patterns/`? Reuse it, plug in your own strategy. Is it inside a mission's own module? It's bespoke on purpose — don't look here for shared infrastructure. A fresh agent session asking "where does my new Step behavior go" answers the question by reading the directory, not by re-deriving the decision procedure from a comment that may have drifted.

One audited finding worth knowing before proposing a collapse yourself (a second, about the now-deleted PR-review pipeline's probe/verify kinds, is moot since #2310 P4d removed the code it was about): the coder-phase pipeline's coder kind (`src/coder_phase.rs`) wraps the SAME `crew::dispatch::dispatch` primitive Tier 1's `dispatch.internal` wraps, a genuine follow-up candidate, but its CLI printing, its own `mission.coder` flow-record vocabulary, and its `result_slot` readback mechanism are real differences a collapse would have to resolve first. Documented in place (code comments citing #1352) rather than forced. The general rule: a collapse that changes observable behavior isn't a tiering fix, it's a feature change wearing a tiering fix's clothes.
