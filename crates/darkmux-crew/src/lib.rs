//! Crew architecture — Role, Crew, Mission, Phase schema + loaders.
//!
//! The doctrine in `CLAUDE.md` names "Role + Crew (not Team)" — composition
//! is operator-defined per mission, no fixed membership. This module groups
//! the four schema types and their on-disk loaders. Future Phase B work
//! (SQLite-backed derived index, CRUD verbs) lands here as additional
//! submodules; Phase C (orchestration / dispatch) likewise.
//!
//! Module layout:
//!
//!   crew::types  — schema (Role, Crew, Mission, Phase, Skill, …)
//!   crew::loader — read JSON manifests from `~/.darkmux/crew/<entity>/`
//!                  with binary-embedded built-ins as fallback

// (#1230 Packet 1) Bounded concurrent-dispatch executor over gestalt's
// `plan_waves` — see the module doc. No production caller in this packet;
// Packet 2's `run_step_graph` scheduler is the intended first consumer.
pub mod brief_refs;
pub mod concurrent_dispatch;
/// Unified-diff parsing (#2310 P4b) — moved down from `darkmux-lab`'s
/// `bundle::diff` so both the lab bundler and this crate's own
/// `deliver_github_review` step kind share ONE parser instead of two. See
/// this module's own doc.
pub mod diff;
// (#849 / #1426) The persisted adjudication corrections — darkmux's second
// memory kind. Read-only by construction: the review path records them as flow
// notes, so there is nothing to author here. Shared by the coder-brief
// injection path and `darkmux memory correction list`.
pub mod corrections;
pub mod dispatch;
// (#1509) `darkmux dispatch` as a crew of one — routes the CLI verb through
// the same `run_step_graph` engine every mission/coder-phase/review run
// uses, so its residency participates in the #1487 lease/reconcile regime.
pub mod dispatch_as_crew_of_one;
pub mod dispatch_internal;
// (#1684 Packet 2) The operator sign-off gate — `gate: "operator"` on a
// mission-config step (schema 2.2), the mechanism behind gated panel verbs
// like `pr-merge`/`pr-approve`. `GateDecision`/`GateHandler`/`resolve_gate`
// are the caller-supplied-seam types `scheduler::run_step_graph` takes;
// `refusal_handler`/`tty_prompt_handler` are the non-interactive/CLI
// production handlers. See the module doc.
pub mod gate;
// (#2108) The in-process host probe — mach tick counters, IOReport power +
// DVFS residency, thermal state, and the IOKit GPU read — that replaced the
// `top`/`vm_stat`/`sysctl`/`ioreg` shell-outs `telemetry_sampler` used to
// make. `telemetry_sampler::sample_host` still exists with the same
// signature; it now reads THROUGH this probe.
pub mod host_probe;
// (#1284 Packet 2) The standard output contract every mission emits +
// generalized finalization. `ReviewEnvelope` (darkmux-lab) maps INTO
// `MissionEnvelope::payload` — this crate has no reverse dependency on
// darkmux-lab, so the mapping lives at the caller (`src/pr_review.rs`).
pub mod envelope;
/// (#2265) The finding record — what was observed, stored write-once under
/// `<findings dir>/<dispatch>/<seq>/finding.json`.
pub mod findings;
pub mod index;
pub mod lessons;
pub mod lifecycle;
pub mod loader;
/// (#2265) The mod record — how something could change, stored write-once
/// under `<mods dir>/<key>/mod.json` with the mod's own `attachments/`.
pub mod mods;
// (#1284 Packet 1) Mission configs — missions as DATA. Schema + loader +
// built-in transcriptions of `build_review_graph`/`default_phase_graph`'s
// former hand-built graphs. Packet 3 added `mission_config::interpret`,
// which those two functions (`darkmux-lab::lab::review`, `src/coder_phase.rs`)
// now call as thin launchers — the configs ARE the executable graphs.
pub mod mission_config;
// (#1877 first extraction) The shared remote-token-bucket type — the
// promotion of what was two hand-copied buckets (`step_kinds::MapRemoteBucket`
// and `darkmux-lab`'s own `RemoteBucket`) into one public home both the
// `dispatch.map` fan-out and `darkmux-lab`'s review pipeline construct.
pub mod remote_budget;
pub mod resourcing;
// (#1959) The rule registry — a named, searchable property bound to files
// by glob, with match/no-match prose. Promoted out of the crawl module
// (originally `darkmux_lab::crawl::rules`) to a general template kind so
// any role's mission can bind to a rule, not only the crawler. See the
// module doc for the search-order + merge-override contract.
pub mod rules;
// (#1877 item 5, motivated by #1876) The generic Complete/Partial/Empty run
// outcome — see the module doc for why a budget-exhausted-but-mostly-
// completed run needs a THIRD state, not just a binary degenerate flag.
pub mod run_outcome;
// (#1877 item 2) The run-record substrate — StepRecord/MemberRecord and the
// resolved-staffing snapshot — extracted out of `darkmux_lab::lab::review`,
// the one mission that had built it for itself. See the module doc.
pub mod run_record;
// (#1877 item 2) The run-observability RAII substrate — the sink-agnostic
// emitter seam, the host telemetry sampling loop, and the guard that ties
// a run's samples and `step result` records to its own lifetime. See the
// module doc.
pub mod run_obs;
pub mod select;
// (#2112) The `PreventUserIdleSystemSleep` RAII assertion held for a
// mission/crawl's life — see the module doc for why it does not override a
// thermal-emergency sleep.
pub mod sleep_assertion;
// (#1230 Packet 2) Generic dependency-graph scheduler over `Step`/`Phase`
// — see the module doc for the DependencyNode/is_ready/reachable/
// run_step_graph shape.
pub mod scheduler;
pub mod single_shot;
// (#1230 Packet 2) Step-kind registry — the execution contract
// `scheduler::run_step_graph` dispatches through.
/// (#2301) `Output<T>` — the typed envelope every step output rides in.
/// One consumer reads another's value THROUGH a struct; the deserialize is
/// the validation.
pub mod step_output;
pub mod step_kinds;
pub mod telemetry_sampler;
// (#2110/#2109) Host-side thermal governor + breaker — see the module doc.
pub mod thermal_governor;
pub mod types;
// (#1959) A generic mission input: named sources materialized into a
// read-only tree, filtered by include/exclude globs. Promoted out of the
// crawl module's `CorpusManifest` — see the module doc for the descope
// this packet states plainly (the crawl planner doesn't consume
// `Materialized` yet).
pub mod workspace_spec;
