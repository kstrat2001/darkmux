//! `darkmux optimize` — guided "optimize for my workload" wizard.
//!
//! Issue tracking: <https://github.com/kstrat2001/darkmux/issues/35>
//!
//! This module implements Phase 1 of a multi-phase build-out. It provides the
//! directory structure, subcommand registration, scaffold module with inline
//! design doc, and stub functions. Subsequent phases will land one step at a time
//! in separate PRs (see the phase plan below).
//!
//! # Design doc — "optimize for my workload" wizard (eventual shape)
//!
//! The wizard composes existing darkmux primitives into an opinionated,
//! reproducible optimization loop that helps operators discover the best profile
//! for their specific workload and hardware. It is **interactive by default** — it
//! gathers intent at runtime, never mutates user state silently, and writes its
//! output (a profile JSON diff) to stdout for the operator to apply.
//!
//! ## Anti-patterns (inviolable)
//!
//! - **Never auto-mutate user state.** The wizard writes its output (profile JSON,
//!   diffs) to stdout. The operator applies changes themselves. No `darkmux swap`
//!   inside the wizard — that's a separate command the operator runs after reviewing.
//! - **Respect existing state.** The wizard reads `profiles.json` but never overwrites
//!   it. It produces a recommended diff the operator can inspect before applying.
//! - **Single-variable changes only.** Each iteration tests exactly one knob at a time;
//!   never change two variables simultaneously (see CLAUDE.md for full doctrine).
//! - **Composes, doesn't reinvent.** The wizard delegates to existing primitives:
//!   [`darkmux scan`], [`lab characterize`], [`lab tune`],
//!   [`heuristics::suggest_profile`], and eureka rules.
//!
//! ## Six-step wizard flow (target architecture)
//!
//! Step 1 — **Intent capture**
//!   Asks the operator what they want to optimize for: throughput (fastest dispatch),
//!   fidelity (best quality, context-heavy), or balanced. Collects task class, target
//!   metric, and hardware spec (auto-detected but reported for verification).
//!
//! Step 2 — **Baseline**
//!   Takes the current active profile (from `profiles.json`), records a run via
//!   `lab characterize` or a single `lab run`, and establishes a wall-clock baseline.
//!   Reads profile id, model metadata, and runtime config to produce a `Baseline`.
//!
//! Step 3 — **Characterize**
//!   Runs `lab tune` (multi-run characterization) on the current profile to build a
//!   bimodal distribution. Returns `CharacterizeReport` with fast/slow cluster data,
//!   compaction firing rates, and mode classification.
//!
//! Step 4 — **Iterate**
//!   Identifies which knobs to adjust from eureka findings (context window mismatch,
//!   compactor not loaded, memory headroom tight, etc.). Runs single-variable changes:
//!   adjusts one knob at a time (primary n_ctx, compactor pairing, context tokens)
//!   and measures wall-clock delta. Returns `IterationResult` with profile id,
//!   delta_walltime_pct (signed, + means faster), and whether the change was kept.
//!
//! Step 5 — **Synthesize**
//!   Compiles all iteration results into a recommended profiles.json diff. Writes the
//!   recommended profile JSON to stdout alongside a unified-diff-style change list
//!   showing what changed across each knob. This is the "output" step — no mutation,
//!   just a structured recommendation the operator reviews and applies.
//!
//! Step 6 — **Reproducible**
//!   Persists the winning profile configuration as a named entry in `profiles.json`
//!   under an `optimize/<workload>` profile family. Also saves the iteration log as a
//!   notebook entry so future operators can see what was tried and why. This step is
//!   the only one that writes user state (appending a new profile entry), and it only
//!   runs after the operator has reviewed and confirmed.
//!
//! ## Data types (between steps)
//!
//! ```rust,ignore
//! pub struct Intent {
//!     task_class: TaskClass,        // from heuristics::TaskClass (Fast/Mid/Long)
//!     target: TargetMetric,         // Throughput, Fidelity, Balanced
//!     hardware: HardwareSpec,       // auto-detected via hardware::detect()
//! }
//!
//! pub struct Baseline {
//!     profile_id: String,           // name of the active profile from profiles.json
//!     run_id: String,               // id returned by lab characterize/run
//!     wall_time_ms: u128,           // measured baseline wall clock
//! }
//!
//! pub struct CharacterizeReport {
//!     fast_cluster_wall_ms: u128,   // mean of fast cluster
//!     slow_cluster_wall_ms: u128,   // mean of slow cluster
//!     bimodal: bool,                // does it have a fast/slow split?
//!     compaction_fire_rate: f64,    // fraction of runs where compaction fired
//!     mode: RunMode,                // fast / slow (from workloads::types)
//! }
//!
//! pub struct IterationResult {
//!     profile_id: String,           // name of the modified profile being tested
//!     delta_walltime_pct: f64,      // signed % change vs baseline (negative = faster)
//!     kept: bool,                   // was this knob change an improvement?
//! }
//!
//! pub struct Recommendation {
//!     recommended_profile: serde_json::Value,  // full profile JSON to apply
//!     changes: Vec<String>,                    // human-readable diff summary
//! }
//! ```
//!
//! ## Step-by-step primitive wiring (future phases)
//!
//! | Step | Primitives called | Data consumed | Output |
//! |------|-------------------|---------------|--------|
//! | 1    | `hardware::detect()` | None (interactive prompt) | `Intent` |
//! | 2    | `profiles::load_registry()`, `lab characterize()` | Active profile from registry | `Baseline` |
//! | 3    | `lab tune()` | Baseline profile + workload id | CharacterizeReport |
//! | 4    | `eureka::evaluate_all()`, `lab tune()` (single-var) | Eureka findings + baseline | IterationResult[] |
//! | 5    | Iterate results, `heuristics::suggest_profile()` (as reference) | IterationResult[] | Recommendation |
//! | 6    | `profiles::save_profile()` (append) | Recommendation | persisted profile + notebook entry |
//!
//! ## Phase plan (current build-out)
//!
//! - **Phase 1 (this PR):** Scaffold module + subcommand registration. Stub functions,
//!   design doc as code comments (this file). Tests verify run() doesn't panic and step
//!   stubs return their bail! shape.
//! - **Phase 2:** Implement Step 1 (intent capture) + Step 2 (baseline). Wire through
//!   `heuristics::suggest_profile` for draft profile suggestion.
//! - **Phase 3:** Implement Step 3 (characterize) — dispatch `lab tune` with bimodal
//!   analysis.
//! - **Phase 4:** Implement Step 4 (iterate) — identify knobs from eureka rules, run
//!   single-variable change loop.
//! - **Phase 5:** Implement Step 5 (synthesize) — emit recommended profiles.json diff
//!   to stdout.
//! - **Phase 6:** Implement Step 6 (reproducible) — persist winning config to profiles.json
//!   under `optimize/<workload>` family, save iteration log as notebook entry.

use crate::hardware::HardwareSpec;
use crate::heuristics::TaskClass;
use anyhow::{bail, Result};

#[allow(dead_code)]
/// Target metric the operator wants to optimize for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TargetMetric {
    /// Maximize throughput — fastest dispatch wall-clock.
    Throughput,
    /// Maximize output quality / fidelity — use maximum context, accept slower dispatch.
    Fidelity,
    /// Balance throughput and quality — middle-ground heuristic.
    Balanced,
}

#[allow(dead_code)]
/// Capture of the operator's optimization intent (Step 1 output).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Intent {
    /// Task class the operator wants to target.
    pub task_class: TaskClass,
    /// Which metric to optimize for.
    pub target: TargetMetric,
    /// Auto-detected hardware spec (reported for verification).
    pub hardware: HardwareSpec,
}

#[allow(dead_code)]
/// Baseline measurement from Step 2 — the starting point for iteration.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Baseline {
    /// Name of the active profile used as baseline.
    pub profile_id: String,
    /// Run id from lab characterize/run that produced this baseline.
    pub run_id: String,
}

#[allow(dead_code)]
/// Characterization report from Step 3 (bimodal lab tune results).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct OptimizeCharReport {
    /// Mean wall-clock of fast cluster.
    pub fast_cluster_wall_ms: u128,
    /// Mean wall-clock of slow cluster.
    pub slow_cluster_wall_ms: u128,
    /// Does it have a fast/slow split?
    pub bimodal: bool,
    /// Fraction of runs where compaction fired.
    pub compaction_fire_rate: f64,
}

#[allow(dead_code)]
/// Result of a single iteration in Step 4 — one knob adjustment measured against baseline.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct IterationResult {
    /// Profile name being tested in this iteration.
    pub profile_id: String,
    /// Signed percentage wall-clock change vs baseline (negative = faster).
    pub delta_walltime_pct: f64,
    /// Whether this knob change was an improvement (kept for next iteration).
    pub kept: bool,
}

#[allow(dead_code)]
/// Recommended profile to apply after Step 5 synthesizes all iterations.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Recommendation {
    /// Full profile JSON to apply.
    pub recommended_profile: serde_json::Value,
}

/// Entry point for the `darkmux optimize` wizard.
///
/// Phase 1: prints a structured scaffold message listing the six steps and their
/// current status (all "todo"). Returns Ok(0) so `darkmux optimize` exits cleanly
/// and the operator sees where the wizard stands.
pub fn run() -> Result<i32> {
    println!("darkmux optimize — guided optimization wizard");
    println!();
    println!("Phase 1 scaffold — design doc is inline in src/optimize.rs");
    println!();
    println!("Steps:");
    println!("  1. Intent capture — todo");
    println!("  2. Baseline         — todo");
    println!("  3. Characterize     — todo");
    println!("  4. Iterate          — todo");
    println!("  5. Synthesize       — todo");
    println!("  6. Reproducible     — todo");
    println!();
    println!(
        "Full plan: https://github.com/kstrat2001/darkmux/issues/35"
    );
    Ok(0)
}

// ─── Step stubs (each phase will fill one in) ─────────────────────────

#[allow(dead_code)]
/// Step 1: Capture operator intent (task class, target metric, hardware spec).
fn step_1_intent() -> Result<Intent> {
    bail!("not yet implemented — see #35 phase plan (step 1)");
}

#[allow(dead_code)]
/// Step 2: Establish baseline by running lab characterize on the active profile.
fn step_2_baseline() -> Result<Baseline> {
    bail!("not yet implemented — see #35 phase plan (step 2)");
}

#[allow(dead_code)]
/// Step 3: Run multi-run characterization with bimodal analysis (lab tune).
fn step_3_characterize() -> Result<OptimizeCharReport> {
    bail!("not yet implemented — see #35 phase plan (step 3)");
}

#[allow(dead_code)]
/// Step 4: Iterate — identify knobs from eureka findings, single-variable change loop.
fn step_4_iterate(_base: &Baseline) -> Result<Vec<IterationResult>> {
    bail!("not yet implemented — see #35 phase plan (step 4)");
}

#[allow(dead_code)]
/// Step 5: Synthesize — emit recommended profiles.json diff to stdout.
fn step_5_synthesize(_its: &[IterationResult]) -> Result<Recommendation> {
    bail!("not yet implemented — see #35 phase plan (step 5)");
}

#[allow(dead_code)]
/// Step 6: Reproducible — persist winning config and save iteration log as notebook entry.
fn step_6_reproducible(_rec: &Recommendation) -> Result<()> {
    bail!("not yet implemented — see #35 phase plan (step 6)");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn run_does_not_panic() {
        let result = run();
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), 0);
    }

    #[test]
    fn step_1_intent_returns_bail() {
        let err = step_1_intent().unwrap_err();
        assert!(err.to_string().contains("#35"));
    }

    #[test]
    fn step_2_baseline_returns_bail() {
        let err = step_2_baseline().unwrap_err();
        assert!(err.to_string().contains("#35"));
    }

    #[test]
    fn step_3_characterize_returns_bail() {
        let err = step_3_characterize().unwrap_err();
        assert!(err.to_string().contains("#35"));
    }

    #[test]
    fn step_4_iterate_returns_bail() {
        let err = step_4_iterate(&Baseline { profile_id: "test".to_string(), run_id: "test".to_string() });
        assert!(err.is_err());
    }

    #[test]
    fn step_5_synthesize_returns_bail() {
        let err = step_5_synthesize(&[]);
        assert!(err.is_err());
    }

    #[test]
    fn step_6_reproducible_returns_bail() {
        let rec = Recommendation { recommended_profile: serde_json::json!({}) };
        let err = step_6_reproducible(&rec);
        assert!(err.is_err());
    }

    #[test]
    fn target_metric_serializes_lowercase() {
        let throughput = TargetMetric::Throughput;
        assert_eq!(serde_json::to_string(&throughput).unwrap(), "\"throughput\"");
        let fidelity = TargetMetric::Fidelity;
        assert_eq!(serde_json::to_string(&fidelity).unwrap(), "\"fidelity\"");
        let balanced = TargetMetric::Balanced;
        assert_eq!(serde_json::to_string(&balanced).unwrap(), "\"balanced\"");
    }

    #[test]
    fn target_metric_parses_lowercase() {
        let parsed: TargetMetric = serde_json::from_str("\"throughput\"").unwrap();
        assert_eq!(parsed, TargetMetric::Throughput);
    }

    #[test]
    fn target_metric_rejects_bad_variant() {
        let result: Result<TargetMetric, _> = serde_json::from_str("\"speed\"");
        assert!(result.is_err());
    }

    #[test]
    fn iteration_result_fields_round_trip() {
        let r = IterationResult {
            profile_id: "test".to_string(),
            delta_walltime_pct: -12.5,
            kept: true,
        };
        let json = serde_json::to_string(&r).unwrap();
        let back: IterationResult = serde_json::from_str(&json).unwrap();
        assert_eq!(back.profile_id, "test");
        assert!((back.delta_walltime_pct - (-12.5)).abs() < 0.01);
        assert!(back.kept);
    }

    #[test]
    fn baseline_fields_round_trip() {
        let b = Baseline {
            profile_id: "fast".to_string(),
            run_id: "abc123".to_string(),
        };
        let json = serde_json::to_string(&b).unwrap();
        let back: Baseline = serde_json::from_str(&json).unwrap();
        assert_eq!(back.profile_id, "fast");
        assert_eq!(back.run_id, "abc123");
    }

    #[test]
    fn recommendation_serializes() {
        let rec = Recommendation {
            recommended_profile: serde_json::json!({
                "optimized": {
                    "_notes": "auto-generated",
                    "models": []
                }
            }),
        };
        let json = serde_json::to_string(&rec).unwrap();
        assert!(json.contains("optimized"));
    }

    #[test]
    fn task_class_serializes_lowercase() {
        assert_eq!(serde_json::to_string(&TaskClass::Fast).unwrap(), "\"fast\"");
        assert_eq!(serde_json::to_string(&TaskClass::Mid).unwrap(), "\"mid\"");
        assert_eq!(serde_json::to_string(&TaskClass::Long).unwrap(), "\"long\"");
    }
}
