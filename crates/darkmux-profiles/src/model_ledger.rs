//! Global memory ledger (#1286) — per resident model, POTENTIAL (the
//! commitment: weights + KV cache at the loaded ctx + transient margin) vs
//! CURRENT (the materialized: observed inference-worker footprint), color-
//! stated per model and machine-total.
//!
//! ONE implementation feeds three surfaces: the `darkmux machine resources` CLI
//! verb, the serve daemon's `GET /machine/resources`, and (through that
//! endpoint) the viewer's `#lens=machine`. It lives in `darkmux-profiles`
//! because this crate is the shared floor of that dependency graph — the
//! binary and `darkmux-serve` both already depend on it, and the I/O
//! adapters the gather composes ([`crate::lms`], `gestalt_host::
//! ArchFactsReader`, the `run_bounded` deadline mechanics) are all here.
//! `darkmux-gestalt` supplies the pure arithmetic ([`ArchFacts`],
//! [`ArchEstimator`]) and stays I/O-free.
//!
//! # Observer-effect constraints (BINDING — #1286 design note)
//!
//! *The observer must not join the observed.*
//!
//! 1. **Zero model dispatches anywhere in this path.** The gather reads
//!    kernel counters (`vm_stat`, `sysctl`, `ps`) and `lms` metadata calls
//!    (`ps --json` / `ls --json`) only — zero tokens, zero Metal work.
//! 2. **Display renders off-machine.** This module emits data; chart
//!    rendering cost lands on the client (the phone over the tailnet), never
//!    on the measured host.
//! 3. **The gather stamps its own cost** — [`ModelLedger::gather_ms`] records
//!    the elapsed wall-clock of the gather itself, so "the observer was
//!    negligible" is verifiable in the data, not assumed.
//! 4. **Every external command is bounded** — all probes run through the
//!    #1276 `run_bounded` mechanism (spawn + poll + kill), never an
//!    unbounded `Command::output()`. Cadence knobs (the endpoint cache TTL)
//!    are recorded in the payload by the serving layer.
//!
//! # The two numbers (#1286)
//!
//! - **Potential**: `catalog size_bytes + kv_per_token(arch) × loaded_ctx +
//!   transient margin` via [`ArchEstimator`] — what the loaded config CAN
//!   grow to. GGUF pays it at load; MLX drifts toward it lazily.
//! - **Current**: best-effort attribution of the LMStudio inference-worker
//!   (`llmworker` node process) resident set sizes. The attribution quality
//!   is itself a field ([`Attribution`]) plus a prose note — degraded
//!   attribution is DOCUMENTED IN THE OUTPUT, never silently precise.
//!
//! KV-cache dtype width is the documented v1 default
//! ([`KV_BYTES_PER_ELEMENT_V1`] = 2, fp16 — the MLX default): it is NOT
//! derivable from the config.json weight quantization; LMStudio KV-quant
//! settings arrive later via #1257 load-config provenance.
//!
//! # Color semantics (#1286)
//!
//! - **green** — Σ potential ≤ limit: guaranteed fit even if every context
//!   fills.
//! - **amber — "made it by luck"** — Σ current ≤ limit < Σ potential:
//!   running under the limit only because lazy allocation hasn't
//!   materialized. The ledger names the config shrink (which model + ctx
//!   reduction) that reaches green at load time.
//! - **red** — Σ current > limit, OR pressure signals active (swap in use /
//!   memory-pressure free% low — the silent-failure tells for unified
//!   memory).
//!
//! The limit is the #1243 AI-RAM budget when configured; no budget field is
//! wired in `config.json` on main yet, so v1 falls back to the physical pool
//! capacity with the fallback named in [`LimitSource`].

use crate::gestalt_host::lms_host::{run_bounded, StdoutMode};
use crate::gestalt_host::ArchFactsReader;
use darkmux_gestalt::{
    ArchEstimator, ArchFacts, CatalogFact, Deadline, FootprintEstimator,
};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::process::Command;
use std::time::Duration;

/// Ledger payload schema (plain semver on the DATA shape, minor-bump +
/// lenient-on-read like the other darkmux data shapes — contract 5).
pub const LEDGER_SCHEMA_VERSION: &str = "1.0";

/// v1 KV-cache dtype width: 2 bytes/element (fp16 cache, the MLX default).
/// Deliberately a named constant, not a guess from weight quantization —
/// see the module docs (#1286 wiring note; refined later by #1257).
pub const KV_BYTES_PER_ELEMENT_V1: u32 = 2;

/// Bound on each `lms` metadata call (`ps --json` / `ls --json`). Generous
/// for a healthy CLI; a wedged one is killed rather than hanging the ledger
/// (#1276 mechanics), and two of these still fit the serve daemon's 30 s
/// request timeout.
const LMS_PROBE_BOUND: Duration = Duration::from_secs(5);

/// Bound on each kernel-counter probe (`vm_stat` / `sysctl` / `ps`). These
/// return in milliseconds when healthy.
const SYS_PROBE_BOUND: Duration = Duration::from_secs(3);

/// Swap-in-use red threshold (#1286 pressure signal). Set ABOVE incidental
/// residue: macOS retains swap long after the pressure that created it
/// (live-observed while building this: 1.7 GB used at 94% memorystatus
/// free on a healthy 128 GB box), so a small used-swap figure is stale
/// evidence, not an active signal — the crisp "pressure NOW" tell is
/// [`MEMORY_FREE_PERCENT_RED`]. Multiple gigabytes of swap on an AI
/// workstation is past the residue band.
pub const SWAP_USED_RED_BYTES: u64 = 4 << 30; // 4 GiB

/// `kern.memorystatus_level` red threshold (#1286 pressure signal) — the
/// kernel counter behind `memory_pressure`'s "system-wide memory free
/// percentage". Healthy systems idle in the 40–70 band; below ~15 the
/// kernel is in its warn/critical pressure band.
pub const MEMORY_FREE_PERCENT_RED: u64 = 15;

/// Floor for the amber shrink hint's suggested context — suggesting less
/// than 4 K ctx stops being a usable dispatch config.
const SHRINK_CTX_FLOOR: u64 = 4096;

// ── payload types (ONE shape for --json and /machine/resources) ────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LedgerState {
    Green,
    Amber,
    Red,
    /// Not decidable from this snapshot (unpriceable model, no limit, …) —
    /// surfaced honestly instead of defaulting to green.
    Unknown,
}

impl LedgerState {
    pub fn as_str(&self) -> &'static str {
        match self {
            LedgerState::Green => "green",
            LedgerState::Amber => "amber",
            LedgerState::Red => "red",
            LedgerState::Unknown => "unknown",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Owner {
    /// `darkmux:*`-namespaced instance (darkmux-managed).
    Darkmux,
    /// Everything else — user state (the namespace contract).
    User,
}

/// How CURRENT bytes were attributed to models — a first-class field so a
/// degraded attribution is visible in the output itself (#1286: never
/// silently precise).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Attribution {
    /// One inference worker per resident: workers rank-matched to models
    /// (largest RSS ↔ largest potential).
    PerProcess,
    /// Worker count ≠ resident count: the worker TOTAL is split across
    /// models proportional to potential (weights when unpriceable).
    Estimated,
    /// Worker enumeration failed or found nothing — current is unknown.
    Unavailable,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LimitSource {
    /// The #1243 AI-RAM budget (not yet wired into `config.json` on main —
    /// this arm activates when that field lands).
    Budget,
    /// Fallback: the physical unified-pool capacity (documented — see the
    /// module docs).
    PhysicalPool,
    /// No budget and no readable pool — no limit to color against.
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct PoolSnapshot {
    pub capacity_bytes: u64,
    /// Conservative `Pages free × page size` — the same tilt as the gestalt
    /// `MacProbe` (inactive/speculative/purgeable deliberately excluded);
    /// gathered here through the ledger's own bounded runner so one
    /// `vm_stat` read feeds both the pool row and the compressor row.
    pub available_bytes: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct PressureSnapshot {
    /// `sysctl vm.swapusage` used bytes.
    pub swap_used_bytes: Option<u64>,
    /// `vm_stat` "Pages occupied by compressor" × page size. Surfaced as a
    /// row; NOT a red trigger in v1 (growth detection needs history a
    /// single snapshot doesn't have — #1247 telemetry series will).
    pub compressor_bytes: Option<u64>,
    /// `sysctl kern.memorystatus_level` — the `memory_pressure` free%.
    pub memory_free_percent: Option<u64>,
    /// Whether any red-zone pressure signal is active (see the thresholds).
    pub red: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelRow {
    pub identifier: String,
    pub model_key: String,
    pub owner: Owner,
    pub loaded_ctx: u64,
    /// Catalog `size_bytes` (on-disk weights).
    pub weights_bytes: Option<u64>,
    /// `kv_per_token(arch)` — `None` when arch facts are unreadable.
    pub kv_per_token_bytes: Option<u64>,
    /// `kv_per_token × loaded_ctx`.
    pub kv_bytes_at_ctx: Option<u64>,
    /// weights + KV@ctx + transient margin ([`ArchEstimator`]); `None` =
    /// unpriceable (missing arch facts or catalog size — the documented
    /// unknowable path, never guessed).
    pub potential_bytes: Option<u64>,
    /// Attributed current footprint — `None` under
    /// [`Attribution::Unavailable`].
    pub current_bytes: Option<u64>,
    pub state: LedgerState,
    /// Amber only: the config shrink that reaches green at load time.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub shrink_hint: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MachineTotals {
    /// Σ potential over PRICEABLE residents. When `unpriced_models > 0`
    /// this UNDERCOUNTS — a warning names the gap.
    pub potential_bytes: u64,
    /// Residents whose potential is unknowable (counted as 0 above).
    pub unpriced_models: u32,
    /// Total inference-worker footprint; `None` under
    /// [`Attribution::Unavailable`].
    pub current_bytes: Option<u64>,
    pub state: LedgerState,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub shrink_hint: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelLedger {
    pub schema_version: String,
    pub generated_at_ms: u64,
    /// Observer-cost stamp (#1286 binding constraint 3): wall-clock ms the
    /// gather itself took. 0 for a purely-computed ledger (tests).
    pub gather_ms: u64,
    pub limit_bytes: Option<u64>,
    pub limit_source: LimitSource,
    pub pool: Option<PoolSnapshot>,
    pub pressure: PressureSnapshot,
    pub models: Vec<ModelRow>,
    pub machine: MachineTotals,
    pub attribution: Attribution,
    /// Prose companion to [`Self::attribution`] — says exactly what the
    /// attribution did (rank pairing / proportional split / why
    /// unavailable), so a degraded number can never read as precise.
    pub attribution_note: String,
    pub warnings: Vec<String>,
}

// ── pure inputs (the test seam: canned/fixture data, never real probes) ──

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResidentInput {
    pub identifier: String,
    pub model_key: String,
    pub loaded_ctx: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WorkerProc {
    pub pid: i64,
    pub rss_bytes: u64,
}

#[derive(Debug, Clone, Default)]
pub struct LedgerInputs {
    pub residents: Vec<ResidentInput>,
    pub catalog: Vec<CatalogFact>,
    /// model_key → arch facts (KV dtype already fixed to the v1 default by
    /// the gather; tests inject directly).
    pub arch: BTreeMap<String, ArchFacts>,
    pub pool: Option<PoolSnapshot>,
    /// The #1243 budget in bytes — `None` until the config field is wired.
    pub budget_bytes: Option<u64>,
    pub swap_used_bytes: Option<u64>,
    pub compressor_bytes: Option<u64>,
    pub memory_free_percent: Option<u64>,
    /// `None` = enumeration failed; `Some(vec![])` = ran, found none.
    pub workers: Option<Vec<WorkerProc>>,
    pub warnings: Vec<String>,
}

// ── the pure core ────────────────────────────────────────────────────────

/// Fold gathered inputs into the ledger. Pure — all I/O lives in
/// [`gather`]; every test drives this directly with injected inputs.
pub fn compute_ledger(inputs: LedgerInputs, generated_at_ms: u64) -> ModelLedger {
    let LedgerInputs {
        residents,
        catalog,
        arch,
        pool,
        budget_bytes,
        swap_used_bytes,
        compressor_bytes,
        memory_free_percent,
        workers,
        mut warnings,
    } = inputs;

    let estimator = ArchEstimator::new(arch.clone());

    // Per-model potential math.
    let mut rows: Vec<ModelRow> = residents
        .iter()
        .map(|r| {
            let weights_bytes = catalog
                .iter()
                .find(|c| c.model_key == r.model_key)
                .and_then(|c| c.size_bytes);
            let kv_per_token_bytes = arch.get(&r.model_key).map(|a| a.kv_per_token());
            let kv_bytes_at_ctx = kv_per_token_bytes.map(|k| k * r.loaded_ctx);
            // ctx is u32 at the estimator port; clamp (a >4B-token ctx does
            // not exist in practice, but never wrap silently).
            let ctx32 = u32::try_from(r.loaded_ctx).unwrap_or(u32::MAX);
            let potential_bytes = estimator.estimate_bytes(&r.model_key, ctx32, Some(&catalog));
            ModelRow {
                identifier: r.identifier.clone(),
                owner: if crate::swap::is_darkmux_owned(&r.identifier) {
                    Owner::Darkmux
                } else {
                    Owner::User
                },
                model_key: r.model_key.clone(),
                loaded_ctx: r.loaded_ctx,
                weights_bytes,
                kv_per_token_bytes,
                kv_bytes_at_ctx,
                potential_bytes,
                current_bytes: None, // attribution below
                state: LedgerState::Unknown,
                shrink_hint: None,
            }
        })
        .collect();

    let sum_potential: u64 = rows.iter().filter_map(|r| r.potential_bytes).sum();
    let unpriced: Vec<&str> = rows
        .iter()
        .filter(|r| r.potential_bytes.is_none())
        .map(|r| r.model_key.as_str())
        .collect();
    if !unpriced.is_empty() {
        warnings.push(format!(
            "{} resident model(s) unpriceable (no readable arch facts or catalog size): {} — machine potential UNDERCOUNTS by their commitment",
            unpriced.len(),
            unpriced.join(", ")
        ));
    }
    let unpriced_models = unpriced.len() as u32;

    // Current-footprint attribution (#1286: the degradation ladder is
    // documented in the output itself, never silently precise).
    let (attribution, attribution_note, current_total) =
        attribute_current(&mut rows, workers.as_deref());

    // Limit: #1243 budget > physical pool capacity > none.
    let (limit_bytes, limit_source) = match (budget_bytes, pool) {
        (Some(b), _) => (Some(b), LimitSource::Budget),
        (None, Some(p)) => (Some(p.capacity_bytes), LimitSource::PhysicalPool),
        (None, None) => (None, LimitSource::Unknown),
    };

    let pressure = PressureSnapshot {
        swap_used_bytes,
        compressor_bytes,
        memory_free_percent,
        red: pressure_red(swap_used_bytes, memory_free_percent),
    };

    // Machine-total color per the #1286 semantics.
    let mut machine_shrink: Option<String> = None;
    let machine_state = match limit_bytes {
        _ if pressure.red => LedgerState::Red,
        Some(limit) if current_total.is_some_and(|c| c > limit) => LedgerState::Red,
        Some(limit) if sum_potential <= limit && unpriced_models == 0 => LedgerState::Green,
        Some(limit) if sum_potential > limit => {
            machine_shrink = Some(shrink_hint(&rows, sum_potential, limit, unpriced_models));
            LedgerState::Amber
        }
        // Under the limit on the KNOWN sum but with unpriceable residents:
        // no fit guarantee exists, and no shrink target is computable.
        Some(_) => LedgerState::Unknown,
        None => LedgerState::Unknown,
    };

    // Per-model tint. Unified memory is ONE pool with shared fate, so the
    // machine state dominates; the per-model color distinguishes who still
    // carries unmaterialized commitment:
    //   machine green → green; machine red → red (everything is at risk);
    //   machine amber → amber while current < potential (this model's lazy
    //   allocation is part of the luck), green once fully materialized (its
    //   commitment is already paid). Unpriceable models stay unknown.
    for row in &mut rows {
        row.state = match (machine_state, row.potential_bytes) {
            (_, None) => LedgerState::Unknown,
            (LedgerState::Green, _) => LedgerState::Green,
            (LedgerState::Red, _) => LedgerState::Red,
            (LedgerState::Amber, Some(pot)) => match row.current_bytes {
                Some(cur) if cur >= pot => LedgerState::Green,
                _ => LedgerState::Amber,
            },
            (LedgerState::Unknown, _) => LedgerState::Unknown,
        };
    }
    // Attach the machine shrink hint to the row it names (single-row hint).
    if let (Some(hint), LedgerState::Amber) = (&machine_shrink, machine_state) {
        if let Some(key) = hint_target_key(&rows, sum_potential, limit_bytes.unwrap_or(0)) {
            if let Some(row) = rows.iter_mut().find(|r| r.model_key == key) {
                row.shrink_hint = Some(hint.clone());
            }
        }
    }

    ModelLedger {
        schema_version: LEDGER_SCHEMA_VERSION.to_string(),
        generated_at_ms,
        gather_ms: 0, // stamped by gather()
        limit_bytes,
        limit_source,
        pool,
        pressure,
        machine: MachineTotals {
            potential_bytes: sum_potential,
            unpriced_models,
            current_bytes: current_total,
            state: machine_state,
            shrink_hint: machine_shrink,
        },
        models: rows,
        attribution,
        attribution_note,
        warnings,
    }
}

/// Red-zone pressure detection (#1286): unified memory fails silent; used
/// swap and a low `memory_pressure` free% are the only tells.
fn pressure_red(swap_used_bytes: Option<u64>, memory_free_percent: Option<u64>) -> bool {
    swap_used_bytes.is_some_and(|s| s > SWAP_USED_RED_BYTES)
        || memory_free_percent.is_some_and(|p| p < MEMORY_FREE_PERCENT_RED)
}

/// Attribute worker footprints to model rows, filling `current_bytes`.
/// Returns `(attribution, note, machine current total)`.
///
/// The ladder (documented in the returned note — never silently precise):
/// - workers == residents (both non-zero): rank-match (largest RSS ↔
///   largest potential) → [`Attribution::PerProcess`]. Pairing is a rank
///   heuristic, and the note says so.
/// - otherwise with ≥1 worker: split the worker TOTAL proportional to each
///   model's potential (weights when unpriceable; equal share when neither
///   is known) → [`Attribution::Estimated`].
/// - enumeration failed or zero workers with residents present →
///   [`Attribution::Unavailable`], every `current_bytes` stays `None`.
fn attribute_current(
    rows: &mut [ModelRow],
    workers: Option<&[WorkerProc]>,
) -> (Attribution, String, Option<u64>) {
    let Some(workers) = workers else {
        return (
            Attribution::Unavailable,
            "inference-worker enumeration failed — current footprint unknown".to_string(),
            None,
        );
    };
    let total: u64 = workers.iter().map(|w| w.rss_bytes).sum();
    if rows.is_empty() {
        // Nothing to attribute to; the worker total (usually 0) is still
        // the honest machine current.
        return (
            Attribution::PerProcess,
            format!(
                "no resident models; {} inference worker(s) totaling {} bytes",
                workers.len(),
                total
            ),
            Some(total),
        );
    }
    if workers.is_empty() {
        return (
            Attribution::Unavailable,
            "no LMStudio inference workers (llmworker processes) visible — current footprint unknown"
                .to_string(),
            None,
        );
    }
    if workers.len() == rows.len() {
        // Rank pairing: sort worker RSS desc; sort row indices by potential
        // (falling back to weights) desc; pair positionally.
        let mut rss: Vec<u64> = workers.iter().map(|w| w.rss_bytes).collect();
        rss.sort_unstable_by(|a, b| b.cmp(a));
        let mut order: Vec<usize> = (0..rows.len()).collect();
        order.sort_by_key(|&i| {
            std::cmp::Reverse(rows[i].potential_bytes.or(rows[i].weights_bytes).unwrap_or(0))
        });
        for (rank, &i) in order.iter().enumerate() {
            rows[i].current_bytes = Some(rss[rank]);
        }
        return (
            Attribution::PerProcess,
            format!(
                "{} worker(s) for {} resident(s) — per-model RSS, workers rank-matched to models by size (largest worker ↔ largest potential)",
                workers.len(),
                rows.len()
            ),
            Some(total),
        );
    }
    // Proportional split of the shared total.
    let weights: Vec<u64> = rows
        .iter()
        .map(|r| r.potential_bytes.or(r.weights_bytes).unwrap_or(0))
        .collect();
    let denom: u64 = weights.iter().sum();
    let mut assigned: u64 = 0;
    let n = rows.len();
    for (i, row) in rows.iter_mut().enumerate() {
        let share = if denom > 0 {
            ((total as u128 * weights[i] as u128) / denom as u128) as u64
        } else {
            total / n as u64
        };
        // Last row absorbs integer-division remainder so the split sums to
        // the observed total exactly.
        let share = if i == n - 1 { total - assigned } else { share };
        assigned += share;
        row.current_bytes = Some(share);
    }
    (
        Attribution::Estimated,
        format!(
            "{} worker(s) for {} resident(s) — per-model numbers are the worker TOTAL split proportional to potential, not per-process measurements",
            workers.len(),
            n
        ),
        Some(total),
    )
}

/// Which model the amber shrink hint targets: the priceable resident whose
/// ctx reduction saves the most per token (highest `kv_per_token` with
/// shrinkable ctx), preferring one whose full reduction covers the
/// overshoot alone.
fn hint_target_key(rows: &[ModelRow], sum_potential: u64, limit: u64) -> Option<String> {
    let overshoot = sum_potential.saturating_sub(limit);
    let candidates: Vec<&ModelRow> = rows
        .iter()
        .filter(|r| {
            r.kv_per_token_bytes.unwrap_or(0) > 0 && r.loaded_ctx > SHRINK_CTX_FLOOR
        })
        .collect();
    let covering = candidates
        .iter()
        .filter(|r| {
            let kv = r.kv_per_token_bytes.unwrap_or(0);
            kv * (r.loaded_ctx - SHRINK_CTX_FLOOR) >= overshoot
        })
        .max_by_key(|r| r.kv_per_token_bytes.unwrap_or(0));
    covering
        .or_else(|| {
            candidates.iter().max_by_key(|r| {
                r.kv_per_token_bytes.unwrap_or(0) * (r.loaded_ctx - SHRINK_CTX_FLOOR)
            })
        })
        .map(|r| r.model_key.clone())
}

/// The amber "config shrink to green" hint (#1286): names the model + the
/// reloaded ctx that brings Σ potential under the limit at load time, or
/// says honestly that no single-model ctx cut reaches green.
///
/// When `unpriced_models > 0` the promised fit is NOT green — green requires
/// zero unpriceable residents (their commitment is uncounted), so applying
/// the shrink lands the machine total at Unknown, not Green. The hint carries
/// that caveat rather than over-promising green (#1286 honesty).
fn shrink_hint(rows: &[ModelRow], sum_potential: u64, limit: u64, unpriced_models: u32) -> String {
    let overshoot = sum_potential.saturating_sub(limit);
    let base = match hint_target_key(rows, sum_potential, limit) {
        None => format!(
            "over the limit by {} with no shrinkable context — unload a resident or load a smaller quant to reach green at load time",
            fmt_bytes(overshoot)
        ),
        Some(key) => {
            let row = rows.iter().find(|r| r.model_key == key).expect("target from rows");
            let kv = row.kv_per_token_bytes.unwrap_or(0).max(1);
            let max_saving = kv * (row.loaded_ctx - SHRINK_CTX_FLOOR);
            if max_saving >= overshoot {
                let cut_tokens = overshoot.div_ceil(kv);
                // Round the suggested ctx DOWN to a 4 K multiple (still ≥ the
                // floor) so the hint reads as a real load config.
                let new_ctx = ((row.loaded_ctx - cut_tokens) / SHRINK_CTX_FLOOR * SHRINK_CTX_FLOOR)
                    .max(SHRINK_CTX_FLOOR);
                let saved = kv * (row.loaded_ctx - new_ctx);
                format!(
                    "reload {} at ctx {} (now {}) — cuts {} of KV commitment; Σ potential then fits the limit at load time",
                    row.model_key,
                    new_ctx,
                    row.loaded_ctx,
                    fmt_bytes(saved)
                )
            } else {
                format!(
                    "no single ctx reduction reaches green — largest single saving is {} at ctx {} ({}); shrink several contexts, unload a resident, or load a smaller quant",
                    row.model_key,
                    SHRINK_CTX_FLOOR,
                    fmt_bytes(max_saving)
                )
            }
        }
    };
    if unpriced_models > 0 {
        format!(
            "{base} — note: {unpriced_models} unpriceable resident(s) are uncounted, so even this shrink leaves the machine total UNKNOWN, not green (no fit guarantee)"
        )
    } else {
        base
    }
}

// ── gather (the I/O edge; every probe bounded) ──────────────────────────

/// Assemble the live ledger: `lms ps/ls --json` + per-model arch facts +
/// kernel counters, all through bounded child runs (#1276 mechanics), then
/// the pure [`compute_ledger`] — and stamp the gather's own cost (#1286
/// observer constraint 3). Never errors: every probe degrades to a warning
/// in the payload.
pub fn gather() -> ModelLedger {
    gather_with_bin(&crate::lms::lms_bin())
}

/// [`gather`] with an explicit `lms` binary — the test seam (tests point at
/// a nonexistent binary / a stub and never touch the operator's real
/// LMStudio; with no ls entries the arch reader touches no files either).
pub fn gather_with_bin(lms_bin: &str) -> ModelLedger {
    let started = std::time::Instant::now();
    let mut warnings = Vec::new();

    let ps_rows = bounded_json_rows(lms_bin, &["ps", "--json"], "ps", &mut warnings);
    let ls_rows = bounded_json_rows(lms_bin, &["ls", "--json"], "ls", &mut warnings);

    let residents: Vec<ResidentInput> = ps_rows.iter().map(resident_from_ps_json).collect();
    let catalog: Vec<CatalogFact> = ls_rows
        .iter()
        .filter_map(|v| {
            let model_key = v.get("modelKey").and_then(|s| s.as_str())?.to_string();
            Some(CatalogFact {
                model_key,
                size_bytes: v.get("sizeBytes").and_then(|n| n.as_u64()),
            })
        })
        .collect();

    // Arch facts for each distinct resident model — located via the ls
    // entries' path fields (the #1290 reader), priced with the v1 fp16 KV
    // width. An unreadable model simply stays out of the map (the
    // estimator's unknowable path, warned about in compute_ledger).
    let reader = ArchFactsReader::from_ls_entries(&ls_rows);
    let mut arch: BTreeMap<String, ArchFacts> = BTreeMap::new();
    for r in &residents {
        if !arch.contains_key(&r.model_key) {
            if let Some(raw) = reader.read(&r.model_key) {
                arch.insert(r.model_key.clone(), arch_facts_v1(&raw));
            }
        }
    }

    // Kernel counters — ONE vm_stat read feeds both the conservative pool
    // availability (MacProbe's tilt) and the compressor row.
    let vm_stat = bounded_stdout("vm_stat", &[], "vm_stat", SYS_PROBE_BOUND, &mut warnings);
    let (available_bytes, compressor_bytes) = match vm_stat.as_deref() {
        Some(out) => {
            let page = parse_vm_stat_page_size(out);
            (
                parse_vm_stat_pages(out, "Pages free").map(|p| p * page),
                parse_vm_stat_pages(out, "Pages occupied by compressor").map(|p| p * page),
            )
        }
        None => (None, None),
    };
    let capacity_bytes =
        bounded_stdout("sysctl", &["-n", "hw.memsize"], "hw.memsize", SYS_PROBE_BOUND, &mut warnings)
            .and_then(|s| s.trim().parse::<u64>().ok());
    let pool = capacity_bytes.map(|capacity_bytes| PoolSnapshot { capacity_bytes, available_bytes });
    let swap_used_bytes = bounded_stdout(
        "sysctl",
        &["-n", "vm.swapusage"],
        "vm.swapusage",
        SYS_PROBE_BOUND,
        &mut warnings,
    )
    .and_then(|s| parse_swapusage_used_bytes(&s));
    let memory_free_percent = bounded_stdout(
        "sysctl",
        &["-n", "kern.memorystatus_level"],
        "memorystatus",
        SYS_PROBE_BOUND,
        &mut warnings,
    )
    .and_then(|s| s.trim().parse::<u64>().ok());

    // LMStudio inference workers (the llmworker node processes, #1286).
    let workers = bounded_stdout(
        "ps",
        &["-axo", "pid=,rss=,command="],
        "ps-workers",
        SYS_PROBE_BOUND,
        &mut warnings,
    )
    .map(|out| parse_worker_rss(&out));

    let mut ledger = compute_ledger(
        LedgerInputs {
            residents,
            catalog,
            arch,
            pool,
            // #1243: no budget field is wired into config.json on main yet;
            // when `runtime.max_model_ram_gb` lands, resolve it here and the
            // Budget limit arm activates.
            budget_bytes: None,
            swap_used_bytes,
            compressor_bytes,
            memory_free_percent,
            workers,
            warnings,
        },
        now_ms(),
    );
    ledger.gather_ms = started.elapsed().as_millis() as u64;
    ledger
}

/// `ArchFactsRaw` → gestalt [`ArchFacts`] with the v1 KV dtype width. NOT a
/// mechanical conversion (#1286 wiring note): `kv_bytes_per_element` is the
/// KV-CACHE width, not derivable from the config's weight-quant bits —
/// fixed at fp16 until #1257 load-config provenance refines it.
fn arch_facts_v1(raw: &crate::gestalt_host::ArchFactsRaw) -> ArchFacts {
    let clamp = |v: u64| u32::try_from(v).unwrap_or(u32::MAX);
    ArchFacts {
        total_layers: clamp(raw.num_hidden_layers),
        full_attention_layers: clamp(raw.full_attention_layers),
        kv_heads: clamp(raw.num_key_value_heads),
        head_dim: clamp(raw.head_dim),
        kv_bytes_per_element: KV_BYTES_PER_ELEMENT_V1,
    }
}

fn resident_from_ps_json(v: &serde_json::Value) -> ResidentInput {
    // Same field fallback chains as crate::lms::model_from_json.
    let identifier = v
        .get("identifier")
        .or_else(|| v.get("id"))
        .and_then(|x| x.as_str())
        .unwrap_or_default()
        .to_string();
    let model_key = v
        .get("modelKey")
        .or_else(|| v.get("model"))
        .or_else(|| v.get("id"))
        .and_then(|x| x.as_str())
        .unwrap_or_default()
        .to_string();
    let loaded_ctx = v
        .get("contextLength")
        .or_else(|| v.get("context"))
        .and_then(|x| x.as_u64())
        .unwrap_or(0);
    ResidentInput { identifier, model_key, loaded_ctx }
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Run `bin args…` bounded, returning stdout on a zero exit; any failure
/// (spawn, non-zero, timeout) becomes a warning — the ledger degrades loud,
/// never errors.
fn bounded_stdout(
    bin: &str,
    args: &[&str],
    phase: &'static str,
    bound: Duration,
    warnings: &mut Vec<String>,
) -> Option<String> {
    let mut cmd = Command::new(bin);
    cmd.args(args);
    // Warnings ride /machine/resources to remote viewers, so they carry the
    // binary's BASENAME, never a full configured path — a `DARKMUX_LMS_BIN`
    // under a home dir must not leak off-machine (#1286 observer/privacy).
    // The runner's own error text (which repeats the full path) is likewise
    // reduced to the label before it lands in the payload.
    let label = bin_label(bin);
    match run_bounded(cmd, phase, Deadline(bound), StdoutMode::Capture) {
        Ok(run) if run.status.success() => Some(run.stdout),
        Ok(run) => {
            let detail = run.exit_detail().replace(bin, label);
            warnings.push(format!("`{label} {}` {detail}", args.join(" ")));
            None
        }
        Err(e) => {
            let detail = e.to_string().replace(bin, label);
            warnings.push(format!("`{label} {}` failed: {detail}", args.join(" ")));
            None
        }
    }
}

/// Basename of a probe binary — the stable, path-free label used in served
/// warnings (#1286). A bare command (`ps`, `vm_stat`) is returned unchanged;
/// a configured absolute path (`/Users/…/lms`) collapses to its file name.
fn bin_label(bin: &str) -> &str {
    bin.rsplit(['/', '\\']).next().unwrap_or(bin)
}

/// `lms <args>` bounded → parsed JSON array rows (empty + warning on any
/// failure — same leniency as the rest of the gather).
fn bounded_json_rows(
    bin: &str,
    args: &[&str],
    phase: &'static str,
    warnings: &mut Vec<String>,
) -> Vec<serde_json::Value> {
    let Some(out) = bounded_stdout(bin, args, phase, LMS_PROBE_BOUND, warnings) else {
        return Vec::new();
    };
    match serde_json::from_str::<serde_json::Value>(&out) {
        Ok(serde_json::Value::Array(rows)) => rows,
        _ => {
            warnings.push(format!("`{bin} {}` output is not a JSON array", args.join(" ")));
            Vec::new()
        }
    }
}

// ── pure parsers (canned-output tests below) ─────────────────────────────

/// Page size from vm_stat's own header (`page size of N bytes`), defaulting
/// to 16384 on Apple Silicon — same parse as the gestalt `MacProbe`.
fn parse_vm_stat_page_size(vm_stat: &str) -> u64 {
    vm_stat
        .lines()
        .next()
        .and_then(|l| l.split("page size of").nth(1))
        .and_then(|s| s.split_whitespace().next())
        .and_then(|n| n.parse::<u64>().ok())
        .unwrap_or(16384)
}

/// Page count for a labeled vm_stat row (`<label>: NNN.`).
fn parse_vm_stat_pages(vm_stat: &str, label: &str) -> Option<u64> {
    vm_stat
        .lines()
        .find(|l| l.trim_start().starts_with(label))
        .and_then(|l| l.rsplit(':').next())
        .and_then(|v| v.trim().trim_end_matches('.').parse::<u64>().ok())
}

/// Used bytes out of `sysctl -n vm.swapusage`:
/// `total = 2048.00M  used = 1058.25M  free = 989.75M  (encrypted)`.
/// Values are binary-suffixed (the kernel reports MiB-scaled figures).
fn parse_swapusage_used_bytes(s: &str) -> Option<u64> {
    let after = s.split("used =").nth(1)?;
    let tok = after.split_whitespace().next()?;
    let (num, mult) = match tok.chars().last()? {
        'K' | 'k' => (&tok[..tok.len() - 1], 1u64 << 10),
        'M' | 'm' => (&tok[..tok.len() - 1], 1u64 << 20),
        'G' | 'g' => (&tok[..tok.len() - 1], 1u64 << 30),
        _ => (tok, 1u64),
    };
    let val: f64 = num.parse().ok()?;
    Some((val * mult as f64) as u64)
}

/// LMStudio inference workers out of `ps -axo pid=,rss=,command=` output:
/// rows that match the actual worker SIGNATURE — `llmworker.js` run under a
/// JS runtime (the `LM Studio.app` electron bundle or a `node`/`electron`
/// binary), live-verified on the M5 Max probes behind #1286. Requiring the
/// runtime prefix (not a bare `llmworker` substring) rejects the phantom-
/// worker false positives — an editor/pager/grep that merely NAMES the file
/// (`vim llmworker.js`, `grep llmworker`, `tail …/llmworker.js`) is not an
/// inference process and must not be counted as inference RAM. ps reports RSS
/// in KiB.
fn parse_worker_rss(ps_out: &str) -> Vec<WorkerProc> {
    ps_out
        .lines()
        .filter(|l| is_lmstudio_worker_cmd(l))
        .filter_map(|l| {
            let mut it = l.split_whitespace();
            let pid: i64 = it.next()?.parse().ok()?;
            let rss_kib: u64 = it.next()?.parse().ok()?;
            Some(WorkerProc { pid, rss_bytes: rss_kib * 1024 })
        })
        .collect()
}

/// True when a `ps` command line is an LMStudio inference worker: it runs the
/// `llmworker.js` script AND the text before that script names a JS runtime
/// (`node`, the `LM Studio.app` electron bundle, or an `electron` binary).
/// The runtime requirement is what kills the phantom class (#1286) — an
/// editor/pager/grep line reaches `llmworker.js` without any runtime prefix.
fn is_lmstudio_worker_cmd(line: &str) -> bool {
    let Some(idx) = line.find("llmworker.js") else {
        return false;
    };
    let prefix = &line[..idx];
    prefix.contains("node")
        || prefix.contains("LM Studio.app")
        || prefix.to_ascii_lowercase().contains("electron")
}

// ── human rendering (the CLI table; tested here, printed by main.rs) ─────

/// Decimal-GB byte formatting, matching the `lms` display convention used
/// elsewhere in darkmux ("X.XX GB").
pub fn fmt_bytes(b: u64) -> String {
    if b >= 1_000_000_000 {
        format!("{:.2} GB", b as f64 / 1_000_000_000.0)
    } else if b >= 1_000_000 {
        format!("{:.0} MB", b as f64 / 1_000_000.0)
    } else if b >= 1_000 {
        format!("{:.0} KB", b as f64 / 1_000.0)
    } else {
        format!("{b} B")
    }
}

fn fmt_opt(b: Option<u64>) -> String {
    b.map(fmt_bytes).unwrap_or_else(|| "—".to_string())
}

/// Truncate an identifier to at most `max` CHARACTERS on a char boundary,
/// appending an ellipsis when cut. Identifiers are operator-controllable and
/// legal LMStudio state can be CJK / accented, so a raw byte slice
/// (`&s[..46]`) panics when the byte offset lands mid-codepoint — the module
/// contract is "degrades loud, never errors" (#1286). Char-count truncation
/// keeps the SAME measure the `{:<46}` column padding uses, so alignment
/// stays sane.
fn truncate_ident(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        // Reserve one char for the ellipsis so the result is ≤ `max` chars.
        let mut out: String = s.chars().take(max.saturating_sub(1)).collect();
        out.push('…');
        out
    }
}

/// The `darkmux machine resources` table + machine rows + gather-cost line.
pub fn render_human(ledger: &ModelLedger) -> String {
    let mut out = String::new();
    out.push_str("model ledger — potential vs current (#1286)\n\n");
    out.push_str(&format!(
        "{:<46} {:<8} {:>8} {:>10} {:>10} {:>10} {:>10}  {}\n",
        "MODEL", "OWNER", "CTX", "WEIGHTS", "KV@CTX", "POTENTIAL", "CURRENT", "STATE"
    ));
    if ledger.models.is_empty() {
        out.push_str("  (no models loaded)\n");
    }
    for m in &ledger.models {
        let ident = truncate_ident(&m.identifier, 46);
        out.push_str(&format!(
            "{:<46} {:<8} {:>8} {:>10} {:>10} {:>10} {:>10}  {}\n",
            ident,
            match m.owner {
                Owner::Darkmux => "darkmux",
                Owner::User => "user",
            },
            m.loaded_ctx,
            fmt_opt(m.weights_bytes),
            fmt_opt(m.kv_bytes_at_ctx),
            fmt_opt(m.potential_bytes),
            fmt_opt(m.current_bytes),
            m.state.as_str(),
        ));
        if let Some(h) = &m.shrink_hint {
            out.push_str(&format!("  ↳ {h}\n"));
        }
    }
    let limit_desc = match ledger.limit_source {
        LimitSource::Budget => "the #1243 AI-RAM budget".to_string(),
        LimitSource::PhysicalPool => {
            "physical pool — no #1243 budget configured".to_string()
        }
        LimitSource::Unknown => "no budget and no readable pool".to_string(),
    };
    out.push_str(&format!(
        "\nmachine: potential {}{} · current {} · limit {} ({}) → {}\n",
        fmt_bytes(ledger.machine.potential_bytes),
        if ledger.machine.unpriced_models > 0 {
            format!(" (+{} unpriced)", ledger.machine.unpriced_models)
        } else {
            String::new()
        },
        fmt_opt(ledger.machine.current_bytes),
        fmt_opt(ledger.limit_bytes),
        limit_desc,
        ledger.machine.state.as_str(),
    ));
    if let Some(h) = &ledger.machine.shrink_hint {
        out.push_str(&format!("  ↳ {h}\n"));
    }
    out.push_str(&format!(
        "pressure: swap used {} · compressor {} · memory free {}{}\n",
        fmt_opt(ledger.pressure.swap_used_bytes),
        fmt_opt(ledger.pressure.compressor_bytes),
        ledger
            .pressure
            .memory_free_percent
            .map(|p| format!("{p}%"))
            .unwrap_or_else(|| "—".to_string()),
        if ledger.pressure.red { "  [PRESSURE RED]" } else { "" },
    ));
    out.push_str(&format!("attribution: {}\n", ledger.attribution_note));
    out.push_str(&format!(
        "gather: {} ms (kernel counters + lms metadata only — zero model dispatches, #1286)\n",
        ledger.gather_ms
    ));
    for w in &ledger.warnings {
        out.push_str(&format!("warning: {w}\n"));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The probed #1286 hybrid 35B judge arch (10/40 full-attn, kv 2×256,
    /// fp16 cache — 20 KB/token).
    fn judge_arch() -> ArchFacts {
        ArchFacts {
            total_layers: 40,
            full_attention_layers: 10,
            kv_heads: 2,
            head_dim: 256,
            kv_bytes_per_element: 2,
        }
    }

    /// Dense devstral-class arch (40/40 full-attn, kv 8×128 — 160 KB/token).
    fn dense_arch() -> ArchFacts {
        ArchFacts {
            total_layers: 40,
            full_attention_layers: 40,
            kv_heads: 8,
            head_dim: 128,
            kv_bytes_per_element: 2,
        }
    }

    fn resident(id: &str, key: &str, ctx: u64) -> ResidentInput {
        ResidentInput { identifier: id.into(), model_key: key.into(), loaded_ctx: ctx }
    }

    fn base_inputs() -> LedgerInputs {
        LedgerInputs {
            residents: vec![
                resident("darkmux:judge", "judge", 65_536),
                resident("devstral", "devstral", 32_768),
            ],
            catalog: vec![
                CatalogFact { model_key: "judge".into(), size_bytes: Some(17_180_000_000) },
                CatalogFact { model_key: "devstral".into(), size_bytes: Some(13_000_000_000) },
            ],
            arch: BTreeMap::from([
                ("judge".to_string(), judge_arch()),
                ("devstral".to_string(), dense_arch()),
            ]),
            pool: Some(PoolSnapshot {
                capacity_bytes: 137_438_953_472,
                available_bytes: Some(3_738_599_424),
            }),
            budget_bytes: None,
            swap_used_bytes: Some(0),
            compressor_bytes: Some(2_000_000_000),
            memory_free_percent: Some(43),
            workers: Some(vec![
                WorkerProc { pid: 1, rss_bytes: 18_000_000_000 },
                WorkerProc { pid: 2, rss_bytes: 15_000_000_000 },
            ]),
            warnings: Vec::new(),
        }
    }

    // Expected potentials from the probed arithmetic (same rows as the
    // estimator tests): judge@65536 = weights + 1,342,177,280 + margin;
    // devstral@32768 = weights + 5,368,709,120 + margin.
    const JUDGE_POTENTIAL: u64 = 17_180_000_000 + 1_342_177_280 + 750_000_000;
    const DEVSTRAL_POTENTIAL: u64 = 13_000_000_000 + 5_368_709_120 + 750_000_000;

    #[test]
    fn potential_math_matches_arch_estimator_rows() {
        let ledger = compute_ledger(base_inputs(), 1);
        let judge = &ledger.models[0];
        assert_eq!(judge.weights_bytes, Some(17_180_000_000));
        assert_eq!(judge.kv_per_token_bytes, Some(20_480));
        assert_eq!(judge.kv_bytes_at_ctx, Some(1_342_177_280));
        assert_eq!(judge.potential_bytes, Some(JUDGE_POTENTIAL));
        let dev = &ledger.models[1];
        assert_eq!(dev.potential_bytes, Some(DEVSTRAL_POTENTIAL));
        assert_eq!(
            ledger.machine.potential_bytes,
            JUDGE_POTENTIAL + DEVSTRAL_POTENTIAL
        );
    }

    #[test]
    fn ownership_partitions_on_the_darkmux_namespace() {
        let ledger = compute_ledger(base_inputs(), 1);
        assert_eq!(ledger.models[0].owner, Owner::Darkmux);
        assert_eq!(ledger.models[1].owner, Owner::User);
    }

    #[test]
    fn green_when_sum_potential_fits_the_limit() {
        // 128 GiB pool, ~37 GB of potential → green, and the limit falls
        // back to the physical pool with the fallback NAMED (#1243 budget
        // unwired on main).
        let ledger = compute_ledger(base_inputs(), 1);
        assert_eq!(ledger.machine.state, LedgerState::Green);
        assert_eq!(ledger.limit_source, LimitSource::PhysicalPool);
        assert_eq!(ledger.limit_bytes, Some(137_438_953_472));
        assert!(ledger.models.iter().all(|m| m.state == LedgerState::Green));
        assert!(ledger.machine.shrink_hint.is_none());
    }

    #[test]
    fn budget_wins_over_physical_pool_as_the_limit() {
        let mut inputs = base_inputs();
        inputs.budget_bytes = Some(64_000_000_000);
        let ledger = compute_ledger(inputs, 1);
        assert_eq!(ledger.limit_source, LimitSource::Budget);
        assert_eq!(ledger.limit_bytes, Some(64_000_000_000));
    }

    #[test]
    fn amber_made_it_by_luck_names_a_shrink_hint() {
        // Budget between Σ current (33 GB) and Σ potential (~37.6 GB):
        // running under the limit only because lazy allocation hasn't
        // materialized — amber, with the config shrink named.
        let mut inputs = base_inputs();
        inputs.budget_bytes = Some(35_000_000_000);
        let ledger = compute_ledger(inputs, 1);
        assert_eq!(ledger.machine.state, LedgerState::Amber);
        let hint = ledger.machine.shrink_hint.as_deref().expect("amber names the shrink");
        // devstral is the KV hog (160 KB/token vs the judge's 20) — the
        // hint targets it, and its full reduction covers the overshoot.
        assert!(hint.contains("devstral"), "hint targets the KV hog: {hint}");
        assert!(hint.contains("reload"), "covering hint suggests a reload ctx: {hint}");
        // The row carries the same hint.
        let dev = ledger.models.iter().find(|m| m.model_key == "devstral").unwrap();
        assert_eq!(dev.shrink_hint.as_deref(), Some(hint));
    }

    #[test]
    fn amber_row_tint_distinguishes_materialized_from_lucky() {
        // Machine amber; judge worker RSS ≥ its potential (fully
        // materialized — its commitment is paid → green row), devstral
        // below its potential (still lucky → amber row).
        let mut inputs = base_inputs();
        inputs.budget_bytes = Some(35_000_000_000);
        inputs.workers = Some(vec![
            WorkerProc { pid: 1, rss_bytes: JUDGE_POTENTIAL + 1_000_000 },
            WorkerProc { pid: 2, rss_bytes: 10_000_000_000 },
        ]);
        let ledger = compute_ledger(inputs, 1);
        assert_eq!(ledger.machine.state, LedgerState::Amber);
        let judge = ledger.models.iter().find(|m| m.model_key == "judge").unwrap();
        let dev = ledger.models.iter().find(|m| m.model_key == "devstral").unwrap();
        assert_eq!(judge.state, LedgerState::Green, "materialized commitment is paid");
        assert_eq!(dev.state, LedgerState::Amber, "unmaterialized commitment is the luck");
    }

    #[test]
    fn red_when_current_exceeds_the_limit() {
        let mut inputs = base_inputs();
        inputs.budget_bytes = Some(30_000_000_000); // Σ current 33 GB > 30 GB
        let ledger = compute_ledger(inputs, 1);
        assert_eq!(ledger.machine.state, LedgerState::Red);
        assert!(ledger.models.iter().all(|m| m.state == LedgerState::Red));
    }

    #[test]
    fn red_on_swap_pressure_signal() {
        let mut inputs = base_inputs();
        inputs.swap_used_bytes = Some(SWAP_USED_RED_BYTES + 1);
        let ledger = compute_ledger(inputs, 1);
        assert!(ledger.pressure.red);
        assert_eq!(ledger.machine.state, LedgerState::Red);
    }

    #[test]
    fn red_on_memory_pressure_free_percent_signal() {
        let mut inputs = base_inputs();
        inputs.memory_free_percent = Some(MEMORY_FREE_PERCENT_RED - 1);
        let ledger = compute_ledger(inputs, 1);
        assert!(ledger.pressure.red);
        assert_eq!(ledger.machine.state, LedgerState::Red);
    }

    #[test]
    fn compressor_alone_is_a_row_not_a_red_trigger_v1() {
        // Growth detection needs history a single snapshot doesn't have —
        // documented v1 scope.
        let mut inputs = base_inputs();
        inputs.compressor_bytes = Some(60_000_000_000);
        let ledger = compute_ledger(inputs, 1);
        assert!(!ledger.pressure.red);
        assert_eq!(ledger.machine.state, LedgerState::Green);
    }

    #[test]
    fn per_process_attribution_rank_matches_and_documents_itself() {
        let ledger = compute_ledger(base_inputs(), 1);
        assert_eq!(ledger.attribution, Attribution::PerProcess);
        assert!(ledger.attribution_note.contains("rank-matched"));
        // Largest worker (18 GB) ↔ largest potential (judge, ~19.3 GB).
        let judge = ledger.models.iter().find(|m| m.model_key == "judge").unwrap();
        assert_eq!(judge.current_bytes, Some(18_000_000_000));
        let dev = ledger.models.iter().find(|m| m.model_key == "devstral").unwrap();
        assert_eq!(dev.current_bytes, Some(15_000_000_000));
        assert_eq!(ledger.machine.current_bytes, Some(33_000_000_000));
    }

    #[test]
    fn shared_worker_degrades_to_estimated_split_documented_in_output() {
        // One worker for two residents (the #1286 open question's fallback):
        // the total splits proportional to potential, the attribution field
        // says "estimated", and the note says exactly what happened.
        let mut inputs = base_inputs();
        inputs.workers = Some(vec![WorkerProc { pid: 1, rss_bytes: 30_000_000_000 }]);
        let ledger = compute_ledger(inputs, 1);
        assert_eq!(ledger.attribution, Attribution::Estimated);
        assert!(ledger.attribution_note.contains("split proportional to potential"));
        let judge = ledger.models.iter().find(|m| m.model_key == "judge").unwrap();
        let dev = ledger.models.iter().find(|m| m.model_key == "devstral").unwrap();
        // Split sums to the observed total EXACTLY (last row absorbs
        // integer remainder), proportions follow potential.
        assert_eq!(
            judge.current_bytes.unwrap() + dev.current_bytes.unwrap(),
            30_000_000_000
        );
        assert!(judge.current_bytes.unwrap() > dev.current_bytes.unwrap());
        assert_eq!(ledger.machine.current_bytes, Some(30_000_000_000));
    }

    #[test]
    fn no_workers_with_residents_is_unavailable_not_zero() {
        let mut inputs = base_inputs();
        inputs.workers = Some(Vec::new());
        let ledger = compute_ledger(inputs, 1);
        assert_eq!(ledger.attribution, Attribution::Unavailable);
        assert!(ledger.machine.current_bytes.is_none());
        assert!(ledger.models.iter().all(|m| m.current_bytes.is_none()));
    }

    #[test]
    fn failed_worker_enumeration_is_unavailable() {
        let mut inputs = base_inputs();
        inputs.workers = None;
        let ledger = compute_ledger(inputs, 1);
        assert_eq!(ledger.attribution, Attribution::Unavailable);
        assert!(ledger.attribution_note.contains("enumeration failed"));
    }

    #[test]
    fn unpriceable_resident_undercount_is_warned_and_blocks_green() {
        // A resident with no arch facts / catalog entry: potential None,
        // machine sum undercounts → warned, and green is NOT claimed even
        // though the known sum fits (no fit guarantee exists).
        let mut inputs = base_inputs();
        inputs.residents.push(resident("mystery", "mystery-model", 8_192));
        let ledger = compute_ledger(inputs, 1);
        assert_eq!(ledger.machine.unpriced_models, 1);
        assert!(ledger.warnings.iter().any(|w| w.contains("mystery-model")));
        assert_eq!(ledger.machine.state, LedgerState::Unknown);
        let mystery = ledger.models.iter().find(|m| m.model_key == "mystery-model").unwrap();
        assert_eq!(mystery.state, LedgerState::Unknown);
        assert!(mystery.potential_bytes.is_none());
    }

    #[test]
    fn no_pool_and_no_budget_is_unknown_limit() {
        let mut inputs = base_inputs();
        inputs.pool = None;
        let ledger = compute_ledger(inputs, 1);
        assert_eq!(ledger.limit_source, LimitSource::Unknown);
        assert!(ledger.limit_bytes.is_none());
        assert_eq!(ledger.machine.state, LedgerState::Unknown);
    }

    #[test]
    fn pressure_red_wins_even_without_a_limit() {
        let mut inputs = base_inputs();
        inputs.pool = None;
        inputs.swap_used_bytes = Some(SWAP_USED_RED_BYTES * 2);
        let ledger = compute_ledger(inputs, 1);
        assert_eq!(ledger.machine.state, LedgerState::Red);
    }

    #[test]
    fn empty_machine_is_green_under_a_limit() {
        let inputs = LedgerInputs {
            pool: base_inputs().pool,
            workers: Some(Vec::new()),
            ..Default::default()
        };
        let ledger = compute_ledger(inputs, 1);
        assert_eq!(ledger.machine.state, LedgerState::Green);
        assert_eq!(ledger.machine.potential_bytes, 0);
        assert_eq!(ledger.machine.current_bytes, Some(0));
    }

    // ── parsers over canned output ──

    const VM_STAT: &str = "Mach Virtual Memory Statistics: (page size of 16384 bytes)\n\
        Pages free:                              228186.\n\
        Pages active:                           2733923.\n\
        Pages inactive:                         2115594.\n\
        Pages occupied by compressor:            131072.\n\
        Pages wired down:                        450334.\n";

    #[test]
    fn vm_stat_parsers_read_free_and_compressor_pages() {
        assert_eq!(parse_vm_stat_page_size(VM_STAT), 16_384);
        assert_eq!(parse_vm_stat_pages(VM_STAT, "Pages free"), Some(228_186));
        assert_eq!(
            parse_vm_stat_pages(VM_STAT, "Pages occupied by compressor"),
            Some(131_072)
        );
        assert_eq!(parse_vm_stat_pages(VM_STAT, "Pages purgeable"), None);
    }

    #[test]
    fn swapusage_used_bytes_parses_the_kernel_shape() {
        let s = "total = 2048.00M  used = 1058.25M  free = 989.75M  (encrypted)";
        assert_eq!(
            parse_swapusage_used_bytes(s),
            Some((1058.25f64 * (1u64 << 20) as f64) as u64)
        );
        let zero = "total = 0.00M  used = 0.00M  free = 0.00M  (encrypted)";
        assert_eq!(parse_swapusage_used_bytes(zero), Some(0));
        let gig = "total = 4.00G  used = 1.50G  free = 2.50G  (encrypted)";
        assert_eq!(parse_swapusage_used_bytes(gig), Some((1.5f64 * (1u64 << 30) as f64) as u64));
        assert_eq!(parse_swapusage_used_bytes("nonsense"), None);
    }

    #[test]
    fn worker_rss_filters_llmworker_rows_and_scales_kib() {
        let ps = "  735  18432000 /Applications/LM Studio.app/Contents/Resources/app/.webpack/main/llmworker.js --stdio\n\
             812      2048 /usr/libexec/somethingelse\n\
             990    512000 node /opt/lmstudio/llmworker.js\n";
        let workers = parse_worker_rss(ps);
        assert_eq!(workers.len(), 2);
        assert_eq!(workers[0], WorkerProc { pid: 735, rss_bytes: 18_432_000 * 1024 });
        assert_eq!(workers[1], WorkerProc { pid: 990, rss_bytes: 512_000 * 1024 });
    }

    #[test]
    fn worker_rss_rejects_phantom_llmworker_lines() {
        // The phantom class: an editor/pager/grep that merely NAMES the file
        // must not be counted as an inference worker (#1286). Only the real
        // runtime-prefixed rows (735 / 990) survive.
        let ps = "  735  18432000 /Applications/LM Studio.app/Contents/Resources/app/.webpack/main/llmworker.js --stdio\n\
             990    512000 node /opt/lmstudio/llmworker.js\n\
             111      4096 vim llmworker.js\n\
             222      8192 grep -r llmworker src/\n\
             333      2048 tail -f /opt/lmstudio/logs/llmworker.js\n";
        let workers = parse_worker_rss(ps);
        assert_eq!(workers.len(), 2, "only the two real workers match, not vim/grep/tail");
        assert_eq!(workers[0].pid, 735);
        assert_eq!(workers[1].pid, 990);
        // Direct assertions on the matcher.
        assert!(is_lmstudio_worker_cmd(
            "  990 512000 node /opt/lmstudio/llmworker.js"
        ));
        assert!(!is_lmstudio_worker_cmd("  111 4096 vim llmworker.js"));
        assert!(!is_lmstudio_worker_cmd("  222 8192 grep -r llmworker src/"));
    }

    #[test]
    fn fmt_bytes_matches_the_lms_decimal_convention() {
        assert_eq!(fmt_bytes(17_180_000_000), "17.18 GB");
        assert_eq!(fmt_bytes(512_000_000), "512 MB");
        assert_eq!(fmt_bytes(20_480), "20 KB");
        assert_eq!(fmt_bytes(0), "0 B");
    }

    #[test]
    fn render_human_carries_the_observer_cost_and_attribution() {
        let mut ledger = compute_ledger(base_inputs(), 1);
        ledger.gather_ms = 42;
        let text = render_human(&ledger);
        assert!(text.contains("gather: 42 ms"), "observer-cost stamp rendered");
        assert!(text.contains("zero model dispatches"));
        assert!(text.contains("rank-matched"), "attribution note rendered verbatim");
        assert!(text.contains("physical pool — no #1243 budget configured"));
        assert!(text.contains("darkmux:judge"));
    }

    #[test]
    fn json_payload_round_trips_and_names_the_fields_the_viewer_reads() {
        let ledger = compute_ledger(base_inputs(), 123);
        let v = serde_json::to_value(&ledger).expect("serializes");
        assert_eq!(v["schema_version"], LEDGER_SCHEMA_VERSION);
        assert_eq!(v["machine"]["state"], "green");
        assert_eq!(v["attribution"], "per_process");
        assert_eq!(v["limit_source"], "physical_pool");
        assert_eq!(v["models"][0]["owner"], "darkmux");
        let back: ModelLedger = serde_json::from_value(v).expect("round-trips");
        assert_eq!(back, ledger);
    }

    /// The one gather test: a nonexistent lms binary — every probe degrades
    /// to warnings, the operator's real LMStudio is never touched (no ls
    /// entries ⇒ the arch reader opens no files), and the observer-cost
    /// stamp is populated. Kernel-counter probes run for real (read-only),
    /// same as the MacProbe tests.
    #[test]
    fn gather_with_missing_lms_degrades_loud_and_stamps_cost() {
        let ledger = gather_with_bin("/nonexistent/darkmux-test-lms-bin");
        assert!(ledger.models.is_empty());
        assert!(
            ledger.warnings.iter().any(|w| w.contains("ps")),
            "lms ps failure surfaces as a warning: {:?}",
            ledger.warnings
        );
        assert_eq!(ledger.schema_version, LEDGER_SCHEMA_VERSION);
        assert!(ledger.generated_at_ms > 1_700_000_000_000);
        // gather_ms is stamped (may legitimately be 0 ms on a fast box, so
        // just assert the render path carries it).
        assert!(render_human(&ledger).contains("gather:"));
    }

    #[test]
    fn render_human_truncates_multibyte_identifiers_without_panic() {
        // Byte 46 falls mid-codepoint for both ids — the old `&s[..46]` byte
        // slice panicked; char-boundary truncation degrades loud (#1286).
        let accented = format!("{}é", "a".repeat(45)); // 46 chars, 47 bytes
        let cjk = "模型".repeat(30); // 60 CJK chars, 180 bytes
        let inputs = LedgerInputs {
            residents: vec![resident(&accented, "m1", 4096), resident(&cjk, "m2", 4096)],
            pool: base_inputs().pool,
            workers: Some(Vec::new()),
            ..Default::default()
        };
        let ledger = compute_ledger(inputs, 1);
        let text = render_human(&ledger); // must not panic
        // 46-char id fits untruncated; the 60-char CJK id is ellipsis-cut.
        assert!(text.contains(&accented), "46-char id renders whole: {text}");
        assert!(text.contains('…'), "over-long CJK id is ellipsis-truncated");
        // Every rendered identifier stays within the 46-char column.
        assert_eq!(truncate_ident(&accented, 46).chars().count(), 46);
        assert_eq!(truncate_ident(&cjk, 46).chars().count(), 46);
        assert!(truncate_ident("short", 46).chars().count() <= 46);
    }

    #[test]
    fn probe_warnings_use_basename_not_home_path() {
        // A configured absolute lms path must not leak off-machine through the
        // served warnings (#1286): only the basename is embedded.
        let mut warnings = Vec::new();
        let got = bounded_stdout(
            "/Users/someone/private/bin/lms-does-not-exist",
            &["ps", "--json"],
            "ps",
            SYS_PROBE_BOUND,
            &mut warnings,
        );
        assert!(got.is_none());
        assert_eq!(warnings.len(), 1);
        let w = &warnings[0];
        assert!(!w.contains("/Users/"), "no home path in the served warning: {w}");
        assert!(w.contains("lms-does-not-exist"), "basename still names the binary: {w}");
        assert_eq!(bin_label("/Users/x/lms"), "lms");
        assert_eq!(bin_label("vm_stat"), "vm_stat");
    }

    #[test]
    fn sum_potential_equal_to_limit_is_green_inclusive() {
        // Σ potential == limit: the green arm's `≤` is inclusive at equality.
        let mut inputs = base_inputs();
        inputs.budget_bytes = Some(JUDGE_POTENTIAL + DEVSTRAL_POTENTIAL);
        let ledger = compute_ledger(inputs, 1);
        assert_eq!(ledger.machine.state, LedgerState::Green);
        assert!(ledger.machine.shrink_hint.is_none());
    }

    #[test]
    fn swap_used_exactly_at_threshold_is_not_red() {
        // swap_used == SWAP_USED_RED_BYTES: the test is strict `>`, so equality
        // is NOT red.
        let mut inputs = base_inputs();
        inputs.swap_used_bytes = Some(SWAP_USED_RED_BYTES);
        let ledger = compute_ledger(inputs, 1);
        assert!(!ledger.pressure.red);
        assert_eq!(ledger.machine.state, LedgerState::Green);
    }

    #[test]
    fn memory_free_exactly_at_threshold_is_not_red() {
        // memory_free_percent == 15: the test is strict `<`, so equality is
        // NOT red.
        let mut inputs = base_inputs();
        inputs.memory_free_percent = Some(MEMORY_FREE_PERCENT_RED);
        let ledger = compute_ledger(inputs, 1);
        assert!(!ledger.pressure.red);
        assert_eq!(ledger.machine.state, LedgerState::Green);
    }

    #[test]
    fn per_model_current_equal_to_potential_is_materialized_green() {
        // cur == pot exactly: the row-tint `cur >= pot` is inclusive → the
        // commitment is materialized, so the row is green under machine amber.
        let mut inputs = base_inputs();
        inputs.budget_bytes = Some(35_000_000_000); // machine amber
        inputs.workers = Some(vec![
            WorkerProc { pid: 1, rss_bytes: JUDGE_POTENTIAL }, // cur == pot
            WorkerProc { pid: 2, rss_bytes: 10_000_000_000 },
        ]);
        let ledger = compute_ledger(inputs, 1);
        assert_eq!(ledger.machine.state, LedgerState::Amber);
        let judge = ledger.models.iter().find(|m| m.model_key == "judge").unwrap();
        assert_eq!(judge.current_bytes, Some(JUDGE_POTENTIAL));
        assert_eq!(judge.state, LedgerState::Green, "materialized commitment is paid");
    }

    #[test]
    fn rank_match_tie_preserves_lms_ps_order() {
        // Two residents with IDENTICAL potential: the potential sort is stable
        // (`sort_by_key`), so they keep lms ps order and the first-listed
        // resident pairs with the largest worker.
        let inputs = LedgerInputs {
            residents: vec![
                resident("darkmux:a", "twin", 32_768),
                resident("darkmux:b", "twin", 32_768),
            ],
            catalog: vec![CatalogFact {
                model_key: "twin".into(),
                size_bytes: Some(10_000_000_000),
            }],
            arch: BTreeMap::from([("twin".to_string(), judge_arch())]),
            pool: base_inputs().pool,
            workers: Some(vec![
                WorkerProc { pid: 1, rss_bytes: 12_000_000_000 },
                WorkerProc { pid: 2, rss_bytes: 8_000_000_000 },
            ]),
            ..Default::default()
        };
        let ledger = compute_ledger(inputs, 1);
        assert_eq!(ledger.attribution, Attribution::PerProcess);
        assert_eq!(ledger.models[0].identifier, "darkmux:a");
        assert_eq!(ledger.models[0].current_bytes, Some(12_000_000_000));
        assert_eq!(ledger.models[1].current_bytes, Some(8_000_000_000));
    }

    /// Parse the suggested ctx out of a "reload … at ctx <N> (now …)" hint.
    fn parse_hint_ctx(hint: &str) -> Option<u64> {
        hint.split("at ctx ").nth(1)?.split_whitespace().next()?.parse().ok()
    }

    #[test]
    fn shrink_hint_ctx_actually_reaches_green_when_applied() {
        // Property: derive the hinted ctx, reload the target at it, and the
        // recomputed ledger must be Green. Pins the floor-rounding DIRECTION —
        // a future ceil-flip that shipped a hint landing just shy of green
        // would fail here.
        let mut inputs = base_inputs();
        inputs.budget_bytes = Some(35_000_000_000);
        let ledger = compute_ledger(inputs, 1);
        assert_eq!(ledger.machine.state, LedgerState::Amber);
        let hint = ledger.machine.shrink_hint.as_deref().expect("amber names a shrink");
        let new_ctx = parse_hint_ctx(hint).expect("covering hint names a ctx");

        let mut applied = base_inputs();
        applied.budget_bytes = Some(35_000_000_000);
        for r in &mut applied.residents {
            if r.model_key == "devstral" {
                r.loaded_ctx = new_ctx;
            }
        }
        let regreen = compute_ledger(applied, 1);
        assert_eq!(
            regreen.machine.state,
            LedgerState::Green,
            "the hint's ctx must reach green, not land just shy of it"
        );
    }

    #[test]
    fn amber_hint_flags_undercount_when_unpriceable_residents_exist() {
        // Amber WITH an unpriceable resident: the promised fit would land
        // Unknown (green needs unpriced == 0), so the hint carries the
        // undercount caveat instead of over-promising green (#1286).
        let mut inputs = base_inputs();
        inputs.budget_bytes = Some(35_000_000_000); // < Σ priceable potential ⇒ amber
        inputs.residents.push(resident("mystery", "mystery-model", 8_192));
        let ledger = compute_ledger(inputs, 1);
        assert_eq!(ledger.machine.state, LedgerState::Amber);
        assert_eq!(ledger.machine.unpriced_models, 1);
        let hint = ledger.machine.shrink_hint.as_deref().unwrap();
        let lower = hint.to_lowercase();
        assert!(
            lower.contains("unpriceable") && lower.contains("unknown"),
            "hint carries the undercount caveat: {hint}"
        );
    }
}
