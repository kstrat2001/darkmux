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
use crate::gestalt_host::{ArchFactsReader, GgufFactsReader};
use darkmux_gestalt::{
    ArchEstimator, ArchFacts, CatalogFact, Deadline, FootprintEstimator, V1Estimator,
};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::process::Command;
use std::time::Duration;

/// Ledger payload schema (plain semver on the DATA shape, minor-bump +
/// lenient-on-read like the other darkmux data shapes — contract 5).
///
/// 1.0 → 1.1 (#1819): additive fields only (`ModelRow::potential_source`,
/// `MachineTotals::estimated_models`) — a 1.0 reader tolerates the payload
/// unchanged (contract 5's "minor = additive, safe to ignore").
///
/// 1.1 → 2.0 (#1821): BREAKING — `warnings: Vec<String>` renamed AND
/// retyped to `messages: Vec<LedgerMessage>` (a rename plus a shape change,
/// not additive — the schema rules name exactly this a major bump).
/// Renders uniformly amber with no severity channel, so the #1819 estimate
/// DISCLOSURE and a real degradation both lit the same WARN lamp; `info` /
/// `warn` / `error` gives the disclosure somewhere honest to live.
/// `PoolSnapshot.available_bytes` is ALSO renamed to `free_bytes` (it was
/// always truly-free pages, not the colloquial "available"), with a NEW
/// `used_bytes` and a NEW `available_bytes` meaning the colloquial figure —
/// see that struct's doc. Tolerable as a major: every consumer (CLI,
/// `/machine/resources`, the viewer) ships in this same binary; no external
/// reader is stranded.
///
/// 2.0 → 2.1 (#1854): additive fields only (`ModelRow::over_price_bytes`,
/// `MachineTotals::over_price_models`) — a 2.0 reader tolerates the payload
/// unchanged. The `potential_bytes` totals now count `max(potential,
/// current)` per resident, which is a VALUE change inside an unchanged
/// field, not a shape change: the field's documented meaning ("the most
/// this machine's residents will hold") is what it already claimed to be,
/// and it was simply wrong whenever a resident had outgrown its price.
pub const LEDGER_SCHEMA_VERSION: &str = "2.1";

/// v1 KV-cache dtype width: 2 bytes/element (fp16 cache, the MLX default).
/// Deliberately a named constant, not a guess from weight quantization —
/// see the module docs (#1286 wiring note; refined later by #1257).
pub const KV_BYTES_PER_ELEMENT_V1: u32 = 2;

/// #1819 fallback KV-cache byte rate per context token — used ONLY when a
/// resident's `config.json` arch facts are unreadable AND (#1820) its own
/// GGUF header is unreadable too (a corrupt/unusual download, an ambiguous
/// multi-file directory the GGUF reader declines to guess between, or a
/// weights format neither reader understands) but the model DOES have a
/// catalog `size_bytes`. Before #1820 this fired for EVERY GGUF resident,
/// since a GGUF's architecture lives inside the binary and nothing read it
/// directly; `GgufFactsReader` (`gestalt_host::gguf_facts`) now reads that
/// header as a measurement, so this fallback fires only on the narrower
/// unreadable-format case named above. Feeds
/// [`V1Estimator`] as the [`ArchWithSizeFallback`]'s second stage.
///
/// **Derivation — traceable, not invented, and traced to the exact model
/// this issue is ABOUT.** `microsoft/phi-4` (the GGUF resident #1819's own
/// issue body names) has an MLX sibling, `mlx-community/phi-4-8bit`, which
/// DOES ship a `config.json` — and the same architecture is published
/// verbatim by Microsoft (`huggingface.co/microsoft/phi-4/raw/main/
/// config.json`, fetched 2026-08-15): `num_hidden_layers: 40,
/// num_attention_heads: 40, num_key_value_heads: 10, hidden_size: 5120`
/// (`model_type: "phi3"` — Phi-4 reuses the Phi-3 architecture class; no
/// `sliding_window`, no `rope_scaling` — a homogeneous DENSE decoder, every
/// layer full-attention, GQA 40→10). `head_dim = hidden_size /
/// num_attention_heads = 5120 / 40 = 128`. So:
/// `2 (K and V) × 40 layers × 10 kv_heads × 128 head_dim × 2 bytes fp16 =
/// 204_800 bytes/token` (204.8 KB/token, decimal). See
/// `fallback_kv_constant_matches_phi_4s_own_published_architecture` below,
/// which ties this literal to that same arithmetic so the two can never
/// drift apart silently.
///
/// **This is deliberately NOT the crate's #1286 devstral-24B referent**
/// (163_840 B/token) that an earlier draft of this constant used: devstral
/// undershoots phi-4 itself by ~20%, which would have meant the fallback
/// underprices the exact resident this issue traces, on the very first
/// machine it runs on — the opposite of "conservative." Deriving from
/// phi-4's own real numbers instead removes that gap for the model that
/// motivated the feature, though it remains a REFERENT, not a proven
/// universal ceiling — a denser architecture than phi-4's could still
/// exceed it (see the dense-attention caveat below, which is about a
/// different axis: hybrid-attention UNDER-counting, not dense-model
/// variance).
///
/// **The dense-attention assumption is deliberate and named (#1819 decision
/// 3).** A hybrid linear-attention model (the Qwen 3.5/3.6 generation,
/// #1286) holds a KV cache on only a small fraction of its layers — as few
/// as 1 in 4 — so pricing it at a dense-attention rate OVERSTATES its true
/// KV cost, sometimes by 4× or more. That overstatement is the intended
/// failure direction: this constant only fires when the real architecture
/// is unreadable, and reserving MORE memory than a hybrid model actually
/// needs is the safe mistake. Assuming hybrid and underpricing a genuinely
/// dense GGUF model would be the unsafe one.
pub const V1_FALLBACK_KV_BYTES_PER_CTX_TOKEN: u64 = 204_800;

/// The tier boundaries, on catalog `size_bytes`, and the KV rate each one
/// assumes. **Size-tiered because a single constant could not be honest**
/// (#1819 merge-gate finding, 2026-08-15).
///
/// The first cut used `V1_FALLBACK_KV_BYTES_PER_CTX_TOKEN` alone — phi-4's
/// own exact rate — for EVERY unpriceable model. Review found that this
/// UNDER-reserves for the population most likely to trigger the fallback in
/// the first place: large dense GGUF downloads. Working the crate's own
/// formula (`ArchFacts::kv_per_token`) over published configs:
///
/// | model | layers × kv_heads × head_dim | true B/token | vs 204_800 |
/// |---|---|---|---|
/// | phi-4 | 40 × 10 × 128 | 204_800 | exact |
/// | Qwen3-32B | 64 × 8 × 128 | 262_144 | 1.28× short |
/// | Llama-3.3-70B, Qwen2.5-72B | 80 × 8 × 128 | 327_680 | 1.60× short |
/// | Mistral-Large-2 123B | 88 × 8 × 128 | 360_448 | 1.76× short |
///
/// A 70B at 32K ctx was short by ~4 GB, at 128K by ~16 GB — and none of it
/// absorbed elsewhere, since weights and margin are identical in both arms.
/// That is the one failure direction this feature exists to refuse: the sum
/// comes in low, the cascade promises GREEN, and the machine overruns at
/// materialization.
///
/// The fallback's one available fact is `size_bytes`, and KV rate tracks it
/// through layer count, so the rate is selected from it. Each tier is set at
/// or above the true rate of every modern GQA architecture that lands in it.
///
/// **Where this still under-reserves, stated plainly:** pre-GQA
/// multi-head-attention models (Llama-2-13B: 40 layers × 40 kv_heads × 128 =
/// 819_200 B/token) exceed every tier here, because MHA's kv_heads equals its
/// attention heads instead of a small fraction of them. No size-derived rate
/// can catch that — the size looks small while the KV rate is enormous.
///
/// **#1820 shipped the real fix — reading arch facts out of the GGUF header
/// directly (`gestalt_host::gguf_facts::GgufFactsReader`), tried BEFORE this
/// fallback in `gather_with_bin`.** A Llama-2-13B GGUF now prices from its
/// own measured 819_200 B/token, not this table's tier estimate. This table
/// remains the honest approximation for whatever the GGUF reader itself
/// can't parse (see [`GgufFactsReader`]'s module docs for its own named
/// limitations) — narrower than "every GGUF" now, not eliminated.
///
/// [`GgufFactsReader`]: crate::gestalt_host::GgufFactsReader
const FALLBACK_KV_TIERS: [(u64, u64); 3] = [
    (15 * 1024 * 1024 * 1024, V1_FALLBACK_KV_BYTES_PER_CTX_TOKEN), // ≤15 GiB: phi-4 class
    (45 * 1024 * 1024 * 1024, 327_680),                            // ≤45 GiB: 70B/72B class
    (u64::MAX, 409_600),                                           // above: 123B class and up
];

/// The KV rate this fallback assumes for a model of `size_bytes`. Total and
/// monotonic: a larger model never assumes a smaller rate.
pub fn fallback_kv_rate_for_size(size_bytes: u64) -> u64 {
    FALLBACK_KV_TIERS
        .iter()
        .find(|(ceiling, _)| size_bytes <= *ceiling)
        .map(|(_, rate)| *rate)
        .unwrap_or(409_600)
}

/// Bound on each `lms` metadata call (`ps --json` / `ls --json`). Generous
/// for a healthy CLI; a wedged one is killed rather than hanging the ledger
/// (#1276 mechanics), and two of these still fit the serve daemon's 30 s
/// request timeout.
const LMS_PROBE_BOUND: Duration = Duration::from_secs(5);

/// Bound on each kernel-counter probe (`vm_stat` / `sysctl` / `ps`). These
/// return in milliseconds when healthy.
const SYS_PROBE_BOUND: Duration = Duration::from_secs(3);

// A swap-in-use RED THRESHOLD used to live here (4 GiB). It was wrong, and
// the doc comment that justified it contained its own refutation: "macOS
// retains swap long after the pressure that created it (live-observed: 1.7 GB
// used at 94% memorystatus free on a healthy 128 GB box), so a small used-swap
// figure is stale evidence, not an active signal — the crisp 'pressure NOW'
// tell is MARGIN_PERCENT_RED." All true. The error was concluding that
// residue is BOUNDED, so a bigger number must mean something. It is not
// bounded: swap-in-use is a monotonic high-water mark that macOS never
// reclaims, so it grows with UPTIME, not with distress.
//
// Observed 2026-08-02 on the dev laptop: 6.96 GB swap in use at 94%
// memorystatus free, 15 GB genuinely free, 45 days of uptime. Every card in
// the lens rendered RED — machine total (18% committed against its limit) and
// every model row — because `pressure.red` is the FIRST match arm in the
// machine-state decision and short-circuits the limit comparison entirely. A
// lens that cries wolf is worse than a lens with no color, because after the
// first false alarm the operator stops reading the color at all.
//
// Swap-in-use is now a ROW, not a trigger — the same call already made for
// the compressor row, for the same reason: turning a LEVEL into a pressure
// signal needs a RATE, and a rate needs history a single snapshot does not
// have. If swap ever becomes a trigger again it has to be a delta between
// samples, not a number compared to a constant.

/// `kern.memorystatus_level` red threshold (#1286 pressure signal) — the
/// kernel's own 0–100 pressure-headroom counter, `(capacity - wired -
/// compressor) / capacity`. Healthy systems idle in the 40–70 band; below
/// ~15 the kernel is in its warn/critical pressure band.
///
/// Named `margin`, not `free` (#1821, operator-approved rename): live,
/// same instant, this read 82% while truly-free pages were 30.8% — a
/// 51-point gap under names that both implied "how much RAM is left".
/// `margin` borrows this project's own NASA register (mass margin, power
/// margin, propellant margin) — the redline is where margin runs out, and
/// this is that margin. It does not claim to be a byte count, which both
/// "free" and "available" would.
pub const MARGIN_PERCENT_RED: u64 = 15;

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

/// Logger-style severity for a [`LedgerMessage`] (#1821) — the channel the
/// old `warnings: Vec<String>` never had. Every entry rendered amber
/// regardless of what it said, so a DISCLOSURE (the #1819 estimate note —
/// working as designed, permanently true on a machine with a GGUF resident)
/// lit the same WARN lamp as a real degradation.
///
/// Named `info`, not `note`: `darkmux flow note` is an existing verb for
/// the orchestrator dashboard, and reusing the word here would give one
/// term two meanings inside the same product (the same collision class as
/// compactor/compressor).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Severity {
    /// A disclosure — nothing degraded, working as documented. The #1819
    /// estimate note lives here: an ESTIMATED model is priced by its own
    /// stated convention, not a fault.
    Info,
    /// A real degradation — the answer is worse than it would be without
    /// this condition (e.g. an unpriceable resident undercounts the sum).
    Warn,
    /// The reading itself is untrustworthy — a probe or enumeration
    /// failure (gather failed, LMStudio unreachable, worker enumeration
    /// came back empty for reasons other than "no workers").
    Error,
}

/// One entry in [`ModelLedger::messages`] (#1821) — replaces the old
/// `warnings: Vec<String>`. See [`Severity`] for the three levels and
/// [`LEDGER_SCHEMA_VERSION`]'s 2.0 changelog entry for why this was a
/// breaking rename rather than an additive field.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LedgerMessage {
    pub severity: Severity,
    pub text: String,
}

impl LedgerMessage {
    pub fn info(text: impl Into<String>) -> Self {
        LedgerMessage { severity: Severity::Info, text: text.into() }
    }
    pub fn warn(text: impl Into<String>) -> Self {
        LedgerMessage { severity: Severity::Warn, text: text.into() }
    }
    pub fn error(text: impl Into<String>) -> Self {
        LedgerMessage { severity: Severity::Error, text: text.into() }
    }
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

/// Where a row's `potential_bytes` came from (#1819) — the provenance field
/// that makes a labeled estimate distinguishable from a measurement
/// everywhere the potential figure appears (row chip, kv line, warnings).
/// Absent (not serialized) on a [`ModelRow`] with no potential at all —
/// see [`ModelRow::potential_source`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PotentialSource {
    /// Priced from the model's own architecture facts ([`ArchEstimator`]) —
    /// a measurement, not a guess. Read from a sibling `config.json`, or
    /// (#1820) from the GGUF binary header when the download carries its
    /// architecture inside the file instead of in a sidecar. Both are the
    /// same class of fact and deliberately share this variant: a consumer
    /// cares that the number was MEASURED, not which byte layout it came
    /// from.
    Arch,
    /// Arch facts were unreadable; priced from catalog size + the
    /// conservative dense-attention KV constant instead ([`V1Estimator`]
    /// fallback, #1819). A labeled conservative estimate, never a silent
    /// one — see [`V1_FALLBACK_KV_BYTES_PER_CTX_TOKEN`] for the assumption
    /// this carries.
    Estimated,
}

/// A single machine-wide memory decomposition, all three figures read from
/// the SAME `vm_stat` call the gather already runs (zero added probe cost,
/// #1286 observer constraint). #1821: the prior shape had only
/// `available_bytes` (truly-free pages) and no `used_bytes` at all, so the
/// page could say how much DARKMUX was using but never how full the
/// MACHINE was — the operator misread darkmux's own figure as the
/// machine's for hours because there was nothing to compare it against.
///
/// **Honest naming, not just new fields.** `available_bytes` used to MEAN
/// truly-free pages; it now means the colloquial figure, and the old
/// meaning moved to `free_bytes`. Leaving both names pointed at
/// truly-free-vs-something-else is exactly the defect this rename fixes
/// (issue finding 7: `pool free` and the `% free` tile disagreed by 51
/// points while sharing the word "free").
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct PoolSnapshot {
    pub capacity_bytes: u64,
    /// Activity-Monitor-style: `wired + compressor_occupied + (active +
    /// inactive - purgeable)`. Cross-checked live against `top` (issue
    /// #1821): 69.3 GiB vs `top`'s ~66 GiB used, sampled seconds apart on a
    /// loaded machine — materially closer than the implied
    /// `capacity - free`, which came in at 73.4 GiB the same instant and
    /// had swung between 1.8 and 61 GiB within one earlier session.
    pub used_bytes: Option<u64>,
    /// The colloquial "how much is left": `free + inactive + speculative`.
    /// Neither `free_bytes` alone (too strict — charges ~26 GiB of
    /// reclaimable inactive pages as not-free) nor `kern.memorystatus_level`
    /// (too generous — the `% free` pressure tile; see its own doc) answers
    /// this; this field is the figure that was missing from the page
    /// entirely (issue finding 7).
    pub available_bytes: Option<u64>,
    /// Truly-free pages only (`vm_stat` "Pages free" × page size) — the
    /// SAME conservative figure this field answered to the name
    /// `available_bytes` before #1821's honest rename. Still the same tilt
    /// as the gestalt `MacProbe` (inactive/speculative/purgeable
    /// deliberately excluded).
    pub free_bytes: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct PressureSnapshot {
    /// `sysctl vm.swapusage` used bytes.
    pub swap_used_bytes: Option<u64>,
    /// `vm_stat` "Pages occupied by compressor" × page size. Surfaced as a
    /// row; NOT a red trigger in v1 (growth detection needs history a
    /// single snapshot doesn't have — #1247 telemetry series will).
    pub compressor_bytes: Option<u64>,
    /// `sysctl kern.memorystatus_level` — `(capacity - wired - compressor) /
    /// capacity`, the kernel's own 0–100 pressure headroom. Named `margin`,
    /// not `free` (#1821): it is neither free nor available memory in the
    /// byte-count sense — see [`MARGIN_PERCENT_RED`]'s doc for the live
    /// 82%-vs-30.8% gap that motivated the rename.
    pub margin_percent: Option<u64>,
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
    /// weights + KV@ctx + transient margin ([`ArchEstimator`]), OR weights +
    /// the #1819 size-based fallback estimate ([`V1Estimator`] via
    /// [`ArchWithSizeFallback`]); `None` = genuinely unpriceable (no
    /// readable arch facts AND no catalog size either — the documented
    /// unknowable path, never guessed).
    pub potential_bytes: Option<u64>,
    /// Which estimator answered — `None` only when `potential_bytes` is
    /// also `None` (nothing priced it at all). Not serialized when absent
    /// (`skip_serializing_if`), matching this file's other optional-field
    /// convention (see `shrink_hint`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub potential_source: Option<PotentialSource>,
    /// Attributed current footprint — `None` under
    /// [`Attribution::Unavailable`].
    pub current_bytes: Option<u64>,
    pub state: LedgerState,
    /// #1854: how much this resident holds ABOVE its priced
    /// [`Self::potential_bytes`], when that overage is material (past the
    /// flap floor in `compute_ledger`). `None` is the normal case — the
    /// resident is at or under its price, or one of the two figures is
    /// missing.
    ///
    /// A number rather than a rendered sentence, so the row hint, the
    /// machine caption's count and the CLI all read ONE server-computed
    /// condition instead of each re-deriving `current > potential` with its
    /// own floor. Not serialized when absent, like `shrink_hint`.
    ///
    /// This is an EPISTEMIC field, not a severity one: a row carrying it is
    /// not unhealthy, and it deliberately does not move [`Self::state`].
    /// What was falsified is the estimate's ceiling, not the fit — the fit
    /// is measured.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub over_price_bytes: Option<u64>,
    /// Amber only: the config shrink that reaches green at load time.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub shrink_hint: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MachineTotals {
    /// Σ potential over PRICEABLE residents (arch-priced AND estimated —
    /// see [`Self::estimated_models`]). When `unpriced_models > 0` this
    /// UNDERCOUNTS — a warning names the gap.
    ///
    /// #1854: summed as `max(potential, current)` per resident, not
    /// `potential` alone. The field's meaning is unchanged — "the most this
    /// machine's residents will hold" — but a per-resident maximum sitting
    /// BELOW that resident's own measured footprint is not a conservative
    /// estimate, it is a disproved one, and summing it made this total (and
    /// every verdict downstream of it) optimistic by exactly the overage.
    /// [`Self::over_price_models`] counts how many residents were counted
    /// at their measured size, so the qualification travels with the figure.
    pub potential_bytes: u64,
    /// Residents whose potential is genuinely unknowable — no readable arch
    /// facts AND no catalog size either (counted as 0 above). Distinct from
    /// [`Self::estimated_models`]: an estimated resident IS counted in
    /// `potential_bytes`, just via the #1819 fallback rather than a
    /// measurement.
    pub unpriced_models: u32,
    /// Residents priced by the #1819 size-based fallback rather than
    /// measured arch facts (#1819 decision 2 — provenance is disclosed
    /// wherever the verdict appears). These ARE counted in
    /// `potential_bytes` and do NOT block Green (decision 1) — only
    /// `unpriced_models` does that.
    ///
    /// `#[serde(default)]` (unlike its sibling fields above, which predate
    /// #1819 and were always present): contract 5 requires this schema stay
    /// lenient-on-read, and a NEW non-`Option` field breaks that in the
    /// READ direction without it — a 1.1 binary parsing a 1.0 peer's ledger
    /// (a real path on a heterogeneous fleet, `main.rs::cmd_machine_resources`)
    /// would otherwise hard-fail on the missing key and silently fall back to
    /// raw-JSON dumping instead of the table. Confirmed absent-key
    /// deserialization below (`estimated_models_defaults_to_zero_on_a_pre_
    /// 1819_payload_missing_the_field`).
    #[serde(default)]
    pub estimated_models: u32,
    /// #1854: residents whose measured footprint has materially exceeded
    /// their priced potential (`ModelRow::over_price_bytes`). These ARE
    /// counted in [`Self::potential_bytes`] — at their MEASURED size, not
    /// their priced one, which is the whole point.
    ///
    /// It qualifies the projection rather than changing it. A projection
    /// with zero here is CEILING-backed: it holds even if every resident
    /// grows to its declared maximum. With a non-zero count it is
    /// FLOOR-backed: it holds at the larger of each price and each observed
    /// size, and the declared maxima are known to be wrong for that many
    /// residents. The count is emitted so a consumer can say so; the viewer
    /// discloses it per row (`ModelRow::over_price_bytes`) and in the
    /// warning, never as a machine-level verdict.
    ///
    /// `#[serde(default)]` for the same lenient-on-read reason as
    /// [`Self::estimated_models`] above (contract 5).
    #[serde(default)]
    pub over_price_models: u32,
    /// Total inference-worker footprint; `None` under
    /// [`Attribution::Unavailable`].
    pub current_bytes: Option<u64>,
    /// #1821: what everything else on the machine is holding right now —
    /// `pool.used_bytes - current_bytes`, floored at 0. `None` when
    /// `pool.used_bytes` is unreadable (vm_stat failed). A missing
    /// `current_bytes` (attribution unavailable) is treated as 0 here —
    /// the direction that makes this an OVERESTIMATE of other tenants,
    /// never an underestimate that could hide risk behind a falsely-small
    /// commitment. Emitted so the cascade's arithmetic is checkable from
    /// the JSON, not just trusted (this page's covenant).
    #[serde(default)]
    pub other_used_bytes: Option<u64>,
    /// #1821: `other_used_bytes + potential_bytes` — "if darkmux's own
    /// commitment fully materializes while everything else holds what it
    /// holds now, what is the machine's total?" This is what the
    /// green/amber cascade actually compares against `limit_bytes` now,
    /// replacing the old `potential_bytes <= limit` comparison that
    /// silently assumed darkmux was the machine's only tenant. `None`
    /// exactly when `other_used_bytes` is `None`.
    #[serde(default)]
    pub projected_total_bytes: Option<u64>,
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
    /// #1821: replaces the old `warnings: Vec<String>` — see
    /// [`Severity`]/[`LedgerMessage`] and [`LEDGER_SCHEMA_VERSION`]'s 2.0
    /// changelog entry. A disclosure (`info`) no longer lights the same
    /// lamp as a real degradation (`warn`/`error`).
    pub messages: Vec<LedgerMessage>,
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
    /// `ps` RSS — every resident page mapped into the process, INCLUDING
    /// clean file-backed ones.
    pub rss_bytes: u64,
    /// `proc_pid_rusage`'s `ri_phys_footprint` — the figure Activity Monitor
    /// shows and jetsam judges by. `None` when the syscall is unavailable
    /// (non-macOS, or a process that vanished between enumeration and probe).
    pub footprint_bytes: Option<u64>,
}

impl WorkerProc {
    /// The worker's memory, as `max(rss, footprint)`.
    ///
    /// **This is a documented HEURISTIC, not a precise quantity**, and the
    /// reason is that neither counter is correct alone — which one is right
    /// depends on the inference backend, and darkmux runs both:
    ///
    /// | backend | weights live in | `ps rss` | `phys_footprint` |
    /// |---|---|---|---|
    /// | MLX | Metal / IOAccelerator buffers | ~0 | the real figure |
    /// | llama.cpp (GGUF) | `mmap`ed clean file-backed pages | the real figure | excludes them |
    ///
    /// Measured live on the operator's machine (2026-08-15), same instant:
    /// a 2.15 GB MLX model read `rss 0.25 GiB / footprint 2.77 GiB`, an
    /// 18.45 GB MLX model read `0.14 / 22.27`, and a 9.05 GB GGUF model read
    /// `11.74 / 3.25`. Footprint deliberately excludes clean file-backed
    /// pages because they are evictable without swapping — true, and exactly
    /// wrong for "is this model occupying RAM right now".
    ///
    /// So the two counters cover largely DISJOINT territory across the two
    /// backends, and the union is what this page wants. `max` approximates
    /// that union: `rss + footprint` would double-count dirty anonymous
    /// pages, which appear in both. `max` slightly UNDER-counts the
    /// non-overlapping remainder of whichever is smaller — the safe
    /// direction, and far closer than either counter alone (RSS alone
    /// understated the machine total ~97x, #1821).
    ///
    /// Both raw figures are kept on this struct and serialised, so the
    /// derivation is checkable rather than trusted.
    pub fn memory_bytes(&self) -> u64 {
        self.rss_bytes.max(self.footprint_bytes.unwrap_or(0))
    }
}

/// `proc_pid_rusage(pid, RUSAGE_INFO_V0, &buf).ri_phys_footprint` — the
/// per-process memory figure Activity Monitor displays and jetsam kills on.
///
/// Called directly rather than shelling out to `/usr/bin/footprint`, which
/// returns the same number: measured at **0.05 s per process** versus a
/// gather that currently completes in ~490 ms and repeats every 5 s. This
/// page's own doctrine (#1286: the observer must not join the observed)
/// makes a ~30% gather-cost increase for three subprocess spawns the wrong
/// trade when a frozen libSystem call costs nothing. `vmmap --summary`
/// yields the same figure at 1.3 s/process and was never a candidate.
///
/// `rusage_info_v0` is a stable, frozen ABI — later flavors append fields
/// and never reorder these. `ri_phys_footprint` is its 8th `u64`.
/// Cross-checked against `/usr/bin/footprint -p` on a live worker: 3.25 GiB
/// both ways, to the byte-rounding shown.
#[cfg(target_os = "macos")]
fn phys_footprint(pid: i64) -> Option<u64> {
    #[repr(C)]
    #[derive(Default)]
    struct RUsageInfoV0 {
        uuid: [u8; 16],
        user_time: u64,
        system_time: u64,
        pkg_idle_wkups: u64,
        interrupt_wkups: u64,
        pageins: u64,
        wired_size: u64,
        resident_size: u64,
        phys_footprint: u64,
        proc_start_abstime: u64,
        proc_exit_abstime: u64,
    }
    unsafe extern "C" {
        fn proc_pid_rusage(pid: i32, flavor: i32, buffer: *mut std::ffi::c_void) -> i32;
    }
    let mut buf = RUsageInfoV0::default();
    // SAFETY: `buf` is a correctly-sized, correctly-aligned `#[repr(C)]`
    // mirror of `rusage_info_v0`, and the callee writes only within it. A
    // dead or unreadable pid returns non-zero and we report `None` rather
    // than reading the buffer.
    let rc = unsafe { proc_pid_rusage(pid as i32, 0, (&raw mut buf).cast()) };
    (rc == 0).then_some(buf.phys_footprint)
}

/// Non-macOS: no footprint concept to read. Callers fall back to RSS alone,
/// which is what this page did everywhere before #1821.
#[cfg(not(target_os = "macos"))]
fn phys_footprint(_pid: i64) -> Option<u64> {
    None
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
    pub margin_percent: Option<u64>,
    /// `None` = enumeration failed; `Some(vec![])` = ran, found none.
    pub workers: Option<Vec<WorkerProc>>,
    /// Pre-populated `error`-severity messages from the gather stage (probe
    /// failures) — `compute_ledger` appends its own `warn`/`info` entries
    /// on top (#1821).
    pub messages: Vec<LedgerMessage>,
}

// ── the estimator composition (#1819) ───────────────────────────────────

/// Composes [`ArchEstimator`] (measured, from `config.json`) with
/// [`V1Estimator`] (catalog size + the conservative dense-attention KV
/// constant) as an HONEST fallback, never a silent one (#1819). Neither
/// [`ArchEstimator`] nor [`V1Estimator`] is modified — this type composes
/// the two existing `FootprintEstimator` implementations rather than
/// growing a third estimation algorithm, per `darkmux-gestalt`'s own
/// `ArchEstimator` doc, which names exactly this composition as the
/// legitimate way to add a fallback ("a chain estimator trying Arch then
/// V1 is a legitimate composition — if built, it gets its own tests — the
/// fallback is then visible in the wiring, never implicit in the math").
///
/// Arch first because it is a MEASUREMENT — real per-model layer/head/dtype
/// facts, read either off the model's own `config.json` (`ArchFactsReader`)
/// or, since #1820, off a GGUF download's own binary metadata header
/// (`GgufFactsReader`) — `gather_with_bin` tries both BEFORE this estimator
/// ever sees the model, so `self.arch` already carries GGUF-derived facts
/// as measurements by the time `estimate_with_source` runs; this type has
/// no GGUF-specific branch of its own. The V1 fallback now fires only when
/// NEITHER reader could answer — a corrupt/unusual GGUF, an ambiguous
/// multi-file directory, or a weights format neither reader understands.
///
/// [`Self::estimate_with_source`] is the ONLY port this type exposes —
/// there is no bare `FootprintEstimator::estimate_bytes` impl, because
/// nothing in this module calls one and a private type gains no drop-in
/// value from implementing a trait it has no consumer for; add it back if
/// a real caller needs it.
///
/// An ESTIMATED row's `kv_per_token_bytes` field stays `None` (the flat V1
/// rate is never decomposed into a per-token figure written back onto the
/// row) — but the amber shrink-hint search (`hint_target_key`) does NOT
/// therefore skip estimated rows: it reads the row's kv rate through
/// [`effective_kv_rate`], which falls back to
/// [`V1_FALLBACK_KV_BYTES_PER_CTX_TOKEN`] for an estimated row — the SAME
/// rate it was priced with — rather than treating it as un-shrinkable. An
/// earlier draft left estimated rows out of shrink-hint targeting entirely,
/// which could render the FALSE claim "no shrinkable context" when the
/// estimated resident was in fact the only one with room to shrink;
/// `effective_kv_rate` closes that gap.
struct ArchWithSizeFallback {
    arch: ArchEstimator,
    // No `fallback: V1Estimator` field: since the tiers landed, the fallback's
    // KV rate is chosen PER MODEL from its catalog size, so the estimator is
    // constructed per call in `estimate_with_source` rather than held here at
    // one fixed rate. A held instance would have to carry a single rate — the
    // exact shape that under-reserved large models and the merge gate caught.
}

impl ArchWithSizeFallback {
    fn new(arch: BTreeMap<String, ArchFacts>) -> Self {
        ArchWithSizeFallback { arch: ArchEstimator::new(arch) }
    }

    /// Arch first (a measurement); V1 fallback second (a labeled estimate);
    /// `None` when NEITHER can answer (no arch facts AND no catalog size —
    /// genuinely unpriceable).
    ///
    /// The fallback arm adds [`darkmux_gestalt::DEFAULT_TRANSIENT_MARGIN_BYTES`]
    /// on top of `V1Estimator`'s own `size + kv_rate×ctx` — the SAME
    /// post-load-overhead margin [`ArchEstimator`] already includes. Without
    /// it, an estimated row would be priced on a DIFFERENT accounting basis
    /// than an arch-priced one (750 MB cheaper for identical weights/ctx),
    /// which would make Green systematically easier to reach for an
    /// estimated resident than a measured one — exactly the silent-optimism
    /// this whole feature exists to refuse. Both arms now price on the same
    /// basis; see `an_estimated_row_and_an_arch_priced_row_price_the_same_
    /// weights_and_ctx_identically_up_to_the_kv_rate` below, which pins that
    /// invariant directly.
    fn estimate_with_source(
        &self,
        model_key: &str,
        min_ctx: u32,
        catalog: Option<&[CatalogFact]>,
    ) -> Option<(u64, PotentialSource)> {
        if let Some(bytes) = self.arch.estimate_bytes(model_key, min_ctx, catalog) {
            return Some((bytes, PotentialSource::Arch));
        }
        // The fallback's rate is chosen per model from its catalog size (see
        // `FALLBACK_KV_TIERS`) rather than fixed at construction, so a large
        // dense GGUF is not priced at a small model's KV rate.
        let size = catalog?.iter().find(|c| c.model_key == model_key)?.size_bytes?;
        let tiered = V1Estimator { kv_bytes_per_ctx_token: fallback_kv_rate_for_size(size) };
        tiered
            .estimate_bytes(model_key, min_ctx, catalog)
            .map(|bytes| {
                (bytes + darkmux_gestalt::DEFAULT_TRANSIENT_MARGIN_BYTES, PotentialSource::Estimated)
            })
    }
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
        margin_percent,
        workers,
        mut messages,
    } = inputs;

    let estimator = ArchWithSizeFallback::new(arch.clone());

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
            let (potential_bytes, potential_source) =
                match estimator.estimate_with_source(&r.model_key, ctx32, Some(&catalog)) {
                    Some((bytes, source)) => (Some(bytes), Some(source)),
                    None => (None, None),
                };
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
                potential_source,
                current_bytes: None, // attribution below
                state: LedgerState::Unknown,
                over_price_bytes: None,
                shrink_hint: None,
            }
        })
        .collect();

    // Genuinely unpriceable: neither arch facts NOR a catalog size could
    // answer — `potential_bytes` itself is `None`. Distinct from an
    // ESTIMATED row (below), which DOES have a `potential_bytes`.
    let unpriced: Vec<&str> = rows
        .iter()
        .filter(|r| r.potential_bytes.is_none())
        .map(|r| r.model_key.as_str())
        .collect();
    if !unpriced.is_empty() {
        // A real degradation — the sum undercounts by an uncounted amount —
        // so `warn`, not `info` (#1821).
        messages.push(LedgerMessage::warn(format!(
            "{} resident model(s) unpriceable (no readable arch facts or catalog size): {} — machine potential UNDERCOUNTS by their commitment",
            unpriced.len(),
            unpriced.join(", ")
        )));
    }
    let unpriced_models = unpriced.len() as u32;

    // #1819: residents priced by the size-based FALLBACK rather than
    // measured arch facts. Counted separately from `unpriced` (they DO
    // contribute to `sum_potential`) and warned about separately, so the
    // two failure modes ("we have no idea" vs "we have a labeled
    // conservative guess") never read as the same thing.
    let estimated: Vec<&str> = rows
        .iter()
        .filter(|r| r.potential_source == Some(PotentialSource::Estimated))
        .map(|r| r.model_key.as_str())
        .collect();
    if !estimated.is_empty() {
        // A disclosure, not a degradation — an ESTIMATED model is priced
        // exactly as designed, permanently true on any machine with a GGUF
        // resident. `info`, so it stops lighting the WARN lamp (#1821 —
        // the whole point of this change).
        messages.push(LedgerMessage::info(format!(
            "{} resident model(s) priced by ESTIMATE, not measurement (no readable config.json — commonly a GGUF download): {} — potential assumes dense attention at a size-tiered {} KB/token. Set at or above every modern GQA architecture in its size class; it OVERSTATES hybrid-attention models (safe), and UNDER-reserves pre-GQA multi-head models such as Llama-2-13B (~819 KB/token), whose real cost no size-derived rate can predict",
            estimated.len(),
            estimated.join(", "),
            // NOTE the bare `{}` above, not `{:.1}`. This argument is a
            // pre-formatted String (it may be a RANGE, "204.8–327.7", when
            // several tiers are in play), and `{:.1}` on a String is not
            // decimal precision — it is a max-width truncation. It silently
            // rendered "204.8" as "2", understating the disclosed assumption
            // 100x in the one message whose entire job is stating that
            // assumption honestly. Shipped because no test pinned the literal
            // text; one does now.
            //
            // The rate(s) ACTUALLY applied, re-derived per row from the same
            // size that selected them — never the bare tier-1 constant. The
            // tiers made a single hardcoded figure wrong here the moment a
            // large model was estimated, which is the same class of drift
            // (stating one rate while pricing at another) that `effective_kv_
            // rate` exists to prevent on the shrink path. Decimal KB (÷1000,
            // this crate's own `fmt_bytes` convention), ONE decimal.
            {
                let mut rates: Vec<u64> = rows
                    .iter()
                    .filter(|r| r.potential_source == Some(PotentialSource::Estimated))
                    .map(effective_kv_rate)
                    .collect();
                rates.sort_unstable();
                rates.dedup();
                match (rates.first(), rates.last()) {
                    (Some(lo), Some(hi)) if lo != hi => {
                        format!("{:.1}–{:.1}", *lo as f64 / 1000.0, *hi as f64 / 1000.0)
                    }
                    (Some(one), _) => format!("{:.1}", *one as f64 / 1000.0),
                    _ => "—".to_string(),
                }
            },
        )));
    }
    let estimated_models = estimated.len() as u32;

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
        margin_percent,
        red: pressure_red(margin_percent),
    };

    // #1821: darkmux is not the machine's only tenant. `other_used` is
    // everything ELSE on the machine, right now — `pool.used_bytes` minus
    // darkmux's own attributed current. A missing `current_total`
    // (attribution unavailable) is treated as 0, which makes `other_used`
    // an OVERESTIMATE of the other tenants rather than an underestimate —
    // the safe direction when the real darkmux share is unknown.
    //
    // `projected_total` answers the question this cascade has always been
    // TRYING to answer: if darkmux's own committed potential fully
    // materializes while everything else holds what it holds now, does the
    // total exceed the limit? The prior rule compared `sum_potential`
    // (darkmux alone) against `limit` (the WHOLE machine), which is only
    // correct on a machine where darkmux is the sole tenant — never true in
    // practice. `None` when `pool.used_bytes` itself is unreadable
    // (vm_stat failed): there is then no fit guarantee to give, so the
    // cascade below falls through to `Unknown` rather than silently
    // reusing the old darkmux-only comparison.
    let other_used_bytes: Option<u64> =
        pool.and_then(|p| p.used_bytes).map(|used| used.saturating_sub(current_total.unwrap_or(0)));
    // (#1854) `potential` is the fit contract — "the most this resident will
    // ever hold". It can be WRONG, and measurably so: an idle MLX resident was
    // observed at 28.40 GiB against a priced potential of 22.88 GiB, steady to
    // the byte across repeated samples, having measured UNDER potential a day
    // earlier (see this module's own note at the ArchEstimator doc). A maximum
    // that sits below an observed value is not a policy choice, it is wrong —
    // so the projection counts `max(potential, current)` per row. Without the
    // clamp the fit verdict is optimistic by exactly the overage, in the one
    // direction that makes an operator load another model.
    //
    // Deliberately NOT a new threshold or a widened margin: widening the
    // estimator needs measurement across models and residency durations, and
    // guessing a constant here would bury the very signal that says the
    // estimate needs work. This only stops darkmux from believing a number it
    // has already disproved.
    // Flap guard: only a MATERIAL overage counts. A few MB of jitter flipping
    // this on and off every poll would teach the operator to ignore it inside
    // a day — the same "signal with no variance carries no information"
    // lesson as the always-gray lamps, arriving from the other side.
    let over_floor = |potential: u64| (potential / 100).max(256 * 1024 * 1024);
    let effective_potential: u64 = rows
        .iter()
        .map(|r| match (r.current_bytes, r.potential_bytes) {
            (Some(c), Some(p)) => c.max(p),
            (_, Some(p)) => p,
            _ => 0,
        })
        .sum();
    // The overage is stamped on the ROW as a number, not only spelled out in
    // the message: every surface that discloses this (the row's own hint, the
    // machine caption's count, the CLI) then reads ONE server-computed
    // condition instead of re-deriving `current > potential + floor` client
    // side. #1852's lesson — a figure whose definition lives in two places is
    // a figure with no definition.
    for r in rows.iter_mut() {
        r.over_price_bytes = match (r.current_bytes, r.potential_bytes) {
            (Some(c), Some(p)) if c > p && c - p > over_floor(p) => Some(c - p),
            _ => None,
        };
    }
    let over_price_models = rows.iter().filter(|r| r.over_price_bytes.is_some()).count() as u32;
    for r in rows.iter() {
        let (Some(over), Some(priced), Some(measured)) = (r.over_price_bytes, r.potential_bytes, r.current_bytes)
        else {
            continue;
        };
        // Carry BOTH numbers, deliberately. The clamp removes this defect's
        // pressure on the verdict, so the only thing keeping the real fix
        // alive afterwards is the specificity of this message — the
        // priced-vs-measured pairs ARE the corpus the estimator work needs.
        messages.push(LedgerMessage::warn(format!(
            "`{}` holds {} more than darkmux priced it (potential {}, measured {}); \
             the fit projection counts the measured size",
            r.identifier,
            fmt_bytes(over),
            fmt_bytes(priced),
            fmt_bytes(measured)
        )));
    }
    let projected_total_bytes: Option<u64> = other_used_bytes.map(|o| o + effective_potential);

    // Machine-total color per the #1286 semantics, updated by #1821: arms 3
    // and 4 now key on `projected_total`, not `sum_potential` alone.
    let mut machine_shrink: Option<String> = None;
    let machine_state = match limit_bytes {
        _ if pressure.red => LedgerState::Red,
        Some(limit) if current_total.is_some_and(|c| c > limit) => LedgerState::Red,
        Some(limit) if projected_total_bytes.is_some_and(|p| p <= limit) && unpriced_models == 0 => {
            LedgerState::Green
        }
        Some(limit) if projected_total_bytes.is_some_and(|p| p > limit) => {
            // The shrink hint's own overshoot has to be measured against
            // the SAME total the verdict was — `projected_total`, not
            // `sum_potential` — or a suggested cut could land exactly on
            // the old (wrong) target and still leave the machine over the
            // real limit once other tenants are counted.
            machine_shrink = Some(shrink_hint(
                &rows,
                projected_total_bytes.unwrap_or(effective_potential),
                limit,
                unpriced_models,
            ));
            LedgerState::Amber
        }
        // Either under the limit on the known projection but with
        // unpriceable residents (no fit guarantee, no shrink target), OR
        // `other_used`/`projected_total` themselves are unreadable
        // (`pool.used_bytes` missing) — neither case can honestly answer
        // "does this fit", so both land here rather than silently falling
        // back to the darkmux-only comparison this cascade used to make.
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
        if let Some(key) = hint_target_key(
            &rows,
            projected_total_bytes.unwrap_or(effective_potential),
            limit_bytes.unwrap_or(0),
        ) {
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
            potential_bytes: effective_potential,
            unpriced_models,
            estimated_models,
            over_price_models,
            current_bytes: current_total,
            other_used_bytes,
            projected_total_bytes,
            state: machine_state,
            shrink_hint: machine_shrink,
        },
        models: rows,
        attribution,
        attribution_note,
        messages,
    }
}

/// Red-zone pressure detection (#1286): unified memory fails silent, and the
/// kernel's own memorystatus level is the one tell that describes NOW. Swap
/// and compressor are reported as rows — see the note where the swap
/// threshold used to be for why neither is a trigger.
fn pressure_red(margin_percent: Option<u64>) -> bool {
    margin_percent.is_some_and(|p| p < MARGIN_PERCENT_RED)
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
    let total: u64 = workers.iter().map(WorkerProc::memory_bytes).sum();
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
        let mut rss: Vec<u64> = workers.iter().map(WorkerProc::memory_bytes).collect();
        rss.sort_unstable_by(|a, b| b.cmp(a));
        let mut order: Vec<usize> = (0..rows.len()).collect();
        // Rank by WEIGHTS, falling back to potential. Weights are the
        // materialised floor — the bytes a loaded model is holding no matter
        // what — whereas potential includes KV that may not exist yet, so
        // ranking by it mis-pairs whenever a small model is loaded at a huge
        // context.
        //
        // Live case that exposed this once #1821 made the worker figures
        // real: a 2.01 GiB-weights model at 120k ctx has potential 19.18 GiB
        // and an actual footprint of 2.77, while an 8.43 GiB-weights model at
        // 16k ctx has potential 12.25 and a footprint of 11.74. Ranked by
        // potential the two swap; ranked by weights every pair lands on the
        // right model. It stays a HEURISTIC either way — `lms ps` exposes no
        // pid, so nothing here is an identity, only an ordering.
        order.sort_by_key(|&i| {
            std::cmp::Reverse(rows[i].weights_bytes.or(rows[i].potential_bytes).unwrap_or(0))
        });
        for (rank, &i) in order.iter().enumerate() {
            rows[i].current_bytes = Some(rss[rank]);
        }
        return (
            Attribution::PerProcess,
            format!(
                "{} worker(s) for {} resident(s) — per-model footprint (max of rss and phys_footprint), workers rank-matched to models by weights (largest worker ↔ largest weights)",
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

/// The kv-per-token rate a shrink-hint computation should charge a row —
/// `kv_per_token_bytes` when it's a real measurement, or, for an ESTIMATED
/// row (which never gets a `kv_per_token_bytes` written back — see
/// [`ArchWithSizeFallback`]'s doc), the SAME
/// [`V1_FALLBACK_KV_BYTES_PER_CTX_TOKEN`] rate it was priced with. `0` for a
/// genuinely unpriceable row (no rate exists to charge). Without this, an
/// estimated resident could never be picked as a shrink target and — worse
/// — a machine over the limit ONLY because of an estimated resident's ctx
/// would render the false "no shrinkable context" line (#1819 review
/// finding): the context is shrinkable, the code was just declining to
/// price the saving with the rate it already used to price the commitment.
fn effective_kv_rate(row: &ModelRow) -> u64 {
    row.kv_per_token_bytes.unwrap_or_else(|| {
        if row.potential_source == Some(PotentialSource::Estimated) {
            // The SAME tier the row was priced at, re-derived from the same
            // input (its catalog weights). Reading the flat constant here
            // instead would reintroduce, one level down, exactly the defect
            // this function exists to fix: a shrink saving computed at a
            // different rate than the commitment it is shrinking. The tiers
            // are size-keyed, so the size must be the one thing consulted.
            row.weights_bytes.map(fallback_kv_rate_for_size).unwrap_or(0)
        } else {
            0
        }
    })
}


/// (#1835 + #1854) The most a ctx reduction on this row can ACTUALLY remove
/// from the machine's projected total.
///
/// The naive answer — `kv_rate × (loaded_ctx − SHRINK_CTX_FLOOR)` — assumes
/// a resident's commitment falls all the way with its price. It does not:
/// since #1854 the projection counts `max(potential, current)`, so once the
/// shrunken potential drops below what the model is ALREADY HOLDING, further
/// cutting saves nothing. You cannot shrink a resident's commitment below
/// its measured footprint; a reload at a smaller ctx does not evict the
/// weights.
///
/// Found by running the hint rather than reading it: a fixture's hint
/// promised 4.70 GB, delivered 4.12 GB, and landed the machine back in the
/// no-margin band it was supposed to escape. The rounding cushion in the
/// suggested ctx used to absorb the difference, which is why this survived
/// until a margin floor consumed the cushion.
fn achievable_ctx_saving(row: &ModelRow) -> u64 {
    let kv = effective_kv_rate(row);
    let by_ctx = kv.saturating_mul(row.loaded_ctx.saturating_sub(SHRINK_CTX_FLOOR));
    match (row.potential_bytes, row.current_bytes) {
        // The floor the projection will not go below for this row.
        (Some(pot), Some(cur)) => by_ctx.min(pot.saturating_sub(cur)),
        _ => by_ctx,
    }
}

/// Which model the amber shrink hint targets: the priceable resident whose
/// ctx reduction saves the most per token (highest effective kv rate — see
/// [`effective_kv_rate`] — with shrinkable ctx), preferring one whose full
/// reduction covers the overshoot alone.
///
/// `total_bytes` is the figure the amber verdict was actually computed
/// against — since #1821, `projected_total` (darkmux's Σ potential plus
/// everything else the machine is holding), not the raw darkmux-only sum —
/// so the overshoot this targets a cut for is the real one.
fn hint_target_key(rows: &[ModelRow], total_bytes: u64, limit: u64) -> Option<String> {
    let overshoot = total_bytes.saturating_sub(limit);
    let candidates: Vec<&ModelRow> = rows
        .iter()
        .filter(|r| effective_kv_rate(r) > 0 && r.loaded_ctx > SHRINK_CTX_FLOOR)
        .collect();
    let covering = candidates
        .iter()
        .filter(|r| achievable_ctx_saving(r) >= overshoot)
        .max_by_key(|r| effective_kv_rate(r));
    covering
        // The fallback names the biggest partial saving — but only a REAL
        // one. A candidate whose whole achievable saving is 0 (its measured
        // footprint already sits at or above its price, #1854) would make
        // the hint read "largest single saving is X at ctx 4096 (0 B)",
        // which is a no-op dressed as advice; with no positive saving
        // anywhere the honest hint is the no-shrinkable-context arm.
        .or_else(|| {
            candidates
                .iter()
                .filter(|r| achievable_ctx_saving(r) > 0)
                .max_by_key(|r| achievable_ctx_saving(r))
        })
        .map(|r| r.model_key.clone())
}

/// The amber "config shrink to green" hint (#1286): names the model + the
/// reloaded ctx that brings the total under the limit at load time, or
/// says honestly that no single-model ctx cut reaches green.
///
/// `total_bytes` — see [`hint_target_key`]'s doc: since #1821 this is
/// `projected_total`, not darkmux's Σ potential alone, so a saving computed
/// here actually closes the gap the verdict is amber about.
///
/// When `unpriced_models > 0` the promised fit is NOT green — green requires
/// zero unpriceable residents (their commitment is uncounted), so applying
/// the shrink lands the machine total at Unknown, not Green. The hint carries
/// that caveat rather than over-promising green (#1286 honesty).
fn shrink_hint(rows: &[ModelRow], total_bytes: u64, limit: u64, unpriced_models: u32) -> String {
    let overshoot = total_bytes.saturating_sub(limit);
    let base = match hint_target_key(rows, total_bytes, limit) {
        None => format!(
            "over the limit by {} with no shrinkable context — unload a resident or load a smaller quant to reach green at load time",
            fmt_bytes(overshoot)
        ),
        Some(key) => {
            let row = rows.iter().find(|r| r.model_key == key).expect("target from rows");
            let kv = effective_kv_rate(row).max(1);
            let max_saving = achievable_ctx_saving(row);
            let target_is_estimated = row.potential_source == Some(PotentialSource::Estimated);
            let saving_note = if target_is_estimated {
                // #1819: the saving itself was computed from the same
                // conservative estimate that priced the row's commitment,
                // not a measurement — say so right where the number is
                // named, not just in a machine-level footnote.
                " (estimated — computed from the same conservative KV assumption as the row's own potential)"
            } else {
                ""
            };
            if max_saving >= overshoot {
                let cut_tokens = overshoot.div_ceil(kv);
                // Round the suggested ctx DOWN to a 4 K multiple (still ≥ the
                // floor) so the hint reads as a real load config.
                let new_ctx = ((row.loaded_ctx - cut_tokens) / SHRINK_CTX_FLOOR * SHRINK_CTX_FLOOR)
                    .max(SHRINK_CTX_FLOOR);
                // The saving that will actually MATERIALIZE, not the KV
                // arithmetic: the projection floors this row at its measured
                // footprint (#1854), so report the number the operator will
                // see rather than the one the formula produces.
                let priced_off = kv * (row.loaded_ctx - new_ctx);
                let saved = match (row.potential_bytes, row.current_bytes) {
                    (Some(pot), Some(cur)) => priced_off.min(pot.saturating_sub(cur)),
                    _ => priced_off,
                };
                format!(
                    "reload {} at ctx {} (now {}) — cuts {} of KV commitment{saving_note}; Σ potential then fits the limit at load time",
                    row.model_key,
                    new_ctx,
                    row.loaded_ctx,
                    fmt_bytes(saved)
                )
            } else {
                // The ctx that REACHES the largest saving. Before #1854's cap
                // that was always the floor (the saving WAS `kv × (ctx −
                // floor)`); with the cap binding, the whole achievable saving
                // arrives long before the floor, and naming 4096 beside it
                // would tell the operator to gut a context for a saving a
                // far smaller cut already delivers.
                let at_ctx = ((row.loaded_ctx.saturating_sub(max_saving.div_ceil(kv))) / SHRINK_CTX_FLOOR
                    * SHRINK_CTX_FLOOR)
                    .max(SHRINK_CTX_FLOOR);
                format!(
                    "no single ctx reduction reaches green — largest single saving is {} at ctx {} ({}){saving_note}; shrink several contexts, unload a resident, or load a smaller quant",
                    row.model_key,
                    at_ctx,
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
    let mut messages = Vec::new();

    let ps_rows = bounded_json_rows(lms_bin, &["ps", "--json"], "ps", &mut messages);
    let ls_rows = bounded_json_rows(lms_bin, &["ls", "--json"], "ls", &mut messages);

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

    // Arch facts for each distinct resident model — resolution order
    // (#1820): a real `config.json` first (a model that ships one is read
    // directly, never reconstructed from a binary), then the GGUF header
    // reader (a measurement pulled out of the `.gguf` file's own metadata,
    // for the common config.json-less case), then — for whichever model
    // NEITHER reader could answer — `compute_ledger`'s `ArchWithSizeFallback`
    // tries the #1819 size-based estimate before giving up. So an absence
    // from this map means "estimated" for any model with a catalog size,
    // and only "genuinely unpriceable" for one without either.
    let arch_reader = ArchFactsReader::from_ls_entries(&ls_rows);
    let gguf_reader = GgufFactsReader::from_ls_entries(&ls_rows);
    let arch = resolve_arch_facts(&residents, &arch_reader, &gguf_reader);

    // Kernel counters — ONE vm_stat read feeds the pool decomposition
    // (#1821), the compressor row, AND (unchanged) the memorystatus
    // pressure tile below.
    let vm_stat = bounded_stdout("vm_stat", &[], "vm_stat", SYS_PROBE_BOUND, &mut messages);
    let (used_bytes, available_bytes, free_bytes, compressor_bytes) = match vm_stat.as_deref() {
        Some(out) => parse_pool_from_vm_stat(out),
        None => (None, None, None, None),
    };
    let capacity_bytes =
        bounded_stdout("sysctl", &["-n", "hw.memsize"], "hw.memsize", SYS_PROBE_BOUND, &mut messages)
            .and_then(|s| s.trim().parse::<u64>().ok());
    let pool = capacity_bytes
        .map(|capacity_bytes| PoolSnapshot { capacity_bytes, used_bytes, available_bytes, free_bytes });
    let swap_used_bytes = bounded_stdout(
        "sysctl",
        &["-n", "vm.swapusage"],
        "vm.swapusage",
        SYS_PROBE_BOUND,
        &mut messages,
    )
    .and_then(|s| parse_swapusage_used_bytes(&s));
    let margin_percent = bounded_stdout(
        "sysctl",
        &["-n", "kern.memorystatus_level"],
        "memorystatus",
        SYS_PROBE_BOUND,
        &mut messages,
    )
    .and_then(|s| s.trim().parse::<u64>().ok());

    // LMStudio inference workers (the llmworker node processes, #1286).
    let workers = bounded_stdout(
        "ps",
        &["-axo", "pid=,rss=,command="],
        "ps-workers",
        SYS_PROBE_BOUND,
        &mut messages,
    )
    .map(|out| enrich_with_footprint(parse_worker_rss(&out), phys_footprint));

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
            margin_percent,
            workers,
            messages,
        },
        now_ms(),
    );
    ledger.gather_ms = started.elapsed().as_millis() as u64;
    ledger
}

/// Resolves architecture facts for every distinct resident model key,
/// trying `config.json` first, then a GGUF header, in that order (#1820).
/// Pulled out of [`gather_with_bin`] as its own function so the RESOLUTION
/// ORDER is a directly-testable unit — not provable only by exercising the
/// two readers separately, which would leave the wiring itself (which one
/// gets tried first, and that a hit on the first short-circuits the second)
/// unverified. A model neither reader can answer is simply absent from the
/// returned map; `ArchWithSizeFallback` (in [`compute_ledger`]) is what
/// turns that absence into the #1819 size-tiered estimate.
fn resolve_arch_facts(
    residents: &[ResidentInput],
    arch_reader: &ArchFactsReader,
    gguf_reader: &GgufFactsReader,
) -> BTreeMap<String, ArchFacts> {
    let mut arch: BTreeMap<String, ArchFacts> = BTreeMap::new();
    for r in residents {
        if arch.contains_key(&r.model_key) {
            continue;
        }
        if let Some(raw) = arch_reader.read(&r.model_key) {
            arch.insert(r.model_key.clone(), arch_facts_v1(&raw));
            continue;
        }
        if let Some(raw) = gguf_reader.read(&r.model_key) {
            arch.insert(r.model_key.clone(), arch_facts_v1(&raw));
        }
    }
    arch
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
    messages: &mut Vec<LedgerMessage>,
) -> Option<String> {
    let mut cmd = Command::new(bin);
    cmd.args(args);
    // Messages ride /machine/resources to remote viewers, so they carry the
    // binary's BASENAME, never a full configured path — a `DARKMUX_LMS_BIN`
    // under a home dir must not leak off-machine (#1286 observer/privacy).
    // The runner's own error text (which repeats the full path) is likewise
    // reduced to the label before it lands in the payload.
    let label = bin_label(bin);
    match run_bounded(cmd, phase, Deadline(bound), StdoutMode::Capture) {
        Ok(run) if run.status.success() => Some(run.stdout),
        Ok(run) => {
            let detail = run.exit_detail().replace(bin, label);
            // #1821: a probe failure means THIS reading is untrustworthy —
            // `error`, not `warn`.
            messages.push(LedgerMessage::error(format!("`{label} {}` {detail}", args.join(" "))));
            None
        }
        Err(e) => {
            let detail = e.to_string().replace(bin, label);
            messages.push(LedgerMessage::error(format!("`{label} {}` failed: {detail}", args.join(" "))));
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
    messages: &mut Vec<LedgerMessage>,
) -> Vec<serde_json::Value> {
    let Some(out) = bounded_stdout(bin, args, phase, LMS_PROBE_BOUND, messages) else {
        return Vec::new();
    };
    match serde_json::from_str::<serde_json::Value>(&out) {
        Ok(serde_json::Value::Array(rows)) => rows,
        _ => {
            messages.push(LedgerMessage::error(format!(
                "`{bin} {}` output is not a JSON array",
                args.join(" ")
            )));
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

/// The full pool decomposition (#1821) off ONE `vm_stat` read — every term
/// is a page-count already present in that single call, so this costs
/// nothing beyond the read the gather already does. Returns
/// `(used_bytes, available_bytes, free_bytes, compressor_bytes)`.
///
/// Each figure degrades INDEPENDENTLY: a single missing label (an
/// unfamiliar `vm_stat` build, a truncated read) takes down only the
/// figures that need it, never panics, and never silently substitutes a
/// wrong number for a missing one (#1286 leniency).
///
/// - `used_bytes` — Activity-Monitor-style: `wired + compressor +
///   (active + inactive - purgeable)`.
/// - `available_bytes` — the colloquial "how much is left":
///   `free + inactive + speculative`.
/// - `free_bytes` — truly-free pages only (`Pages free × page size`).
/// - `compressor_bytes` — `Pages occupied by compressor × page size`,
///   unchanged from before #1821 (the pressure-row figure), folded in here
///   so the whole read happens through one page-size lookup.
fn parse_pool_from_vm_stat(vm_stat: &str) -> (Option<u64>, Option<u64>, Option<u64>, Option<u64>) {
    let page = parse_vm_stat_page_size(vm_stat);
    let pages = |label: &str| parse_vm_stat_pages(vm_stat, label);
    let free = pages("Pages free");
    let active = pages("Pages active");
    let inactive = pages("Pages inactive");
    let speculative = pages("Pages speculative");
    let purgeable = pages("Pages purgeable");
    let wired = pages("Pages wired down");
    let compressor = pages("Pages occupied by compressor");

    let used_bytes = match (wired, compressor, active, inactive, purgeable) {
        (Some(w), Some(c), Some(a), Some(i), Some(p)) => {
            Some((w + c + (a + i).saturating_sub(p)) * page)
        }
        _ => None,
    };
    let available_bytes = match (free, inactive, speculative) {
        (Some(f), Some(i), Some(s)) => Some((f + i + s) * page),
        _ => None,
    };
    let free_bytes = free.map(|f| f * page);
    let compressor_bytes = compressor.map(|c| c * page);

    (used_bytes, available_bytes, free_bytes, compressor_bytes)
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
            Some(WorkerProc { pid, rss_bytes: rss_kib * 1024, footprint_bytes: None })
        })
        .collect()
}

/// Fills each worker's `footprint_bytes` from `probe`.
///
/// Split out with the probe INJECTED rather than inlined at the gather so the
/// wiring is testable: a mutation that stopped filling footprint entirely
/// passed the whole suite when this lived inline, because every test built
/// `WorkerProc`s by hand and nothing exercised the path that populates them.
/// The parser above stays pure (`ps` text in, structs out); this is where the
/// syscall-per-worker happens.
fn enrich_with_footprint(mut workers: Vec<WorkerProc>, probe: impl Fn(i64) -> Option<u64>) -> Vec<WorkerProc> {
    for w in &mut workers {
        w.footprint_bytes = probe(w.pid);
    }
    workers
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
    out.push_str("machine resources — potential vs current (#1286)\n\n");
    out.push_str(&format!(
        "{:<46} {:<8} {:>8} {:>10} {:>10} {:>10} {:>10}  {}\n",
        "MODEL", "OWNER", "CTX", "WEIGHTS", "KV@CTX", "POTENTIAL", "CURRENT", "STATE"
    ));
    if ledger.models.is_empty() {
        out.push_str("  (no models loaded)\n");
    }
    for m in &ledger.models {
        let ident = truncate_ident(&m.identifier, 46);
        // #1819: an ESTIMATED row's state carries the disclosure right next
        // to the figure it qualifies — provenance visible everywhere the
        // verdict appears, not just in the warnings list at the foot. The
        // POTENTIAL figure itself gets the same `~` prefix the UI's kv line
        // uses (`modelKvLine`, `machineGauge.ts`) — the CAVEAT travels WITH
        // the number so it survives a copy-paste out of this table, not
        // just a separate column a reader could crop away.
        let is_estimated = m.potential_source == Some(PotentialSource::Estimated);
        let state_word =
            if is_estimated { format!("{} (estimated)", m.state.as_str()) } else { m.state.as_str().to_string() };
        let potential_word =
            if is_estimated { format!("~{}", fmt_opt(m.potential_bytes)) } else { fmt_opt(m.potential_bytes) };
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
            potential_word,
            fmt_opt(m.current_bytes),
            state_word,
        ));
        // #1854: footnote the row where the confusing pair actually appears —
        // this line prints `potential X · current Y` with Y > X, and the
        // machine-level message six lines down cannot repair that in place.
        if let (Some(over), Some(cur)) = (m.over_price_bytes, m.current_bytes) {
            out.push_str(&format!(
                "  ↳ holds {} more than priced — the fit projection counts the measured {}\n",
                fmt_bytes(over),
                fmt_bytes(cur)
            ));
        }
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
    // #1819: the machine-total parenthetical names BOTH gaps when both are
    // present — "unpriced" (uncounted, undercounts the sum) and "estimated"
    // (counted, but via a labeled guess) are different facts and must not
    // collapse into one word.
    //
    // Built as a clause list rather than matched combinatorially so a third
    // gap does not mean eight arms. Same rule as before: each clause names a
    // different gap and none of them collapse into one word.
    //
    // #1854 deliberately does NOT add a clause here. Its own count
    // (`over_price_models`) stays in the payload for `--json` consumers, but
    // read cold at glance a `(1 at measured)` parenthetical said nothing to
    // the operator — it only parses if you already know the ceiling-vs-floor
    // argument behind it. The condition is disclosed where it can be acted
    // on: a footnote on the row it is about, and the warning below carrying
    // both figures.
    let mut count_clauses: Vec<String> = Vec::new();
    if ledger.machine.unpriced_models > 0 {
        count_clauses.push(format!("+{} unpriced", ledger.machine.unpriced_models));
    }
    if ledger.machine.estimated_models > 0 {
        count_clauses.push(format!("{} estimated", ledger.machine.estimated_models));
    }
    let counts_paren =
        if count_clauses.is_empty() { String::new() } else { format!(" ({})", count_clauses.join(", ")) };
    out.push_str(&format!(
        "\nmachine: potential {}{} · current {} · limit {} ({}) → {}\n",
        fmt_bytes(ledger.machine.potential_bytes),
        counts_paren,
        fmt_opt(ledger.machine.current_bytes),
        fmt_opt(ledger.limit_bytes),
        limit_desc,
        ledger.machine.state.as_str(),
    ));
    // #1821: the cascade's own arithmetic, checkable from this line —
    // "other tenants" plus darkmux's Σ potential is what the verdict above
    // was actually compared against the limit with.
    out.push_str(&format!(
        "  other tenants {} · projected total {}\n",
        fmt_opt(ledger.machine.other_used_bytes),
        fmt_opt(ledger.machine.projected_total_bytes),
    ));
    if let Some(h) = &ledger.machine.shrink_hint {
        out.push_str(&format!("  ↳ {h}\n"));
    }
    out.push_str(&format!(
        "pressure: swap used {} · compressor {} · margin {}{}\n",
        fmt_opt(ledger.pressure.swap_used_bytes),
        fmt_opt(ledger.pressure.compressor_bytes),
        ledger
            .pressure
            .margin_percent
            .map(|p| format!("{p}%"))
            .unwrap_or_else(|| "—".to_string()),
        if ledger.pressure.red { "  [PRESSURE RED]" } else { "" },
    ));
    out.push_str(&format!("attribution: {}\n", ledger.attribution_note));
    out.push_str(&format!(
        "gather: {} ms (kernel counters + lms metadata only — zero model dispatches, #1286)\n",
        ledger.gather_ms
    ));
    for m in &ledger.messages {
        let tag = match m.severity {
            Severity::Info => "info",
            Severity::Warn => "warning",
            Severity::Error => "error",
        };
        out.push_str(&format!("{tag}: {}\n", m.text));
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
                // #1821: 40 GB machine-wide used, against 33 GB darkmux
                // current (the two workers below) — 7 GB of OTHER tenants,
                // which keeps every test built on this fixture's existing
                // Green/Amber expectations unchanged (7 GB is negligible
                // against a 137.4 GB limit).
                used_bytes: Some(40_000_000_000),
                available_bytes: Some(90_000_000_000),
                free_bytes: Some(3_738_599_424),
            }),
            budget_bytes: None,
            swap_used_bytes: Some(0),
            compressor_bytes: Some(2_000_000_000),
            margin_percent: Some(43),
            workers: Some(vec![
                WorkerProc { pid: 1, rss_bytes: 18_000_000_000, footprint_bytes: None },
                WorkerProc { pid: 2, rss_bytes: 15_000_000_000, footprint_bytes: None },
            ]),
            messages: Vec::new(),
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

    /// (#1854) `potential` is the fit contract — "the most this resident will
    /// ever hold". Measured live on an IDLE MLX resident: current 28.40 GiB
    /// against a potential of 22.88 GiB, steady to the byte across repeated
    /// samples, and the code's own comment records the same model measured
    /// UNDER potential a day earlier. So the estimate can be exceeded, and
    /// silently: the projection is built from `potential`, which means the
    /// machine's fit verdict was optimistic by the whole overage.
    ///
    /// A maximum that sits below an observed value is simply wrong. Clamp it
    /// — `max(potential, current)` — so the projection reflects what the
    /// machine is actually holding, and say so, because a silently-corrected
    /// estimate is one nobody ever fixes.
    #[test]
    fn a_resident_holding_more_than_its_potential_raises_the_projection_and_is_disclosed() {
        let mut inputs = base_inputs();
        // One worker far above its priced potential; the other normal.
        inputs.workers = Some(vec![
            WorkerProc { pid: 1, rss_bytes: 60_000_000_000, footprint_bytes: None },
            WorkerProc { pid: 2, rss_bytes: 5_000_000_000, footprint_bytes: None },
        ]);
        inputs.pool = Some(PoolSnapshot {
            capacity_bytes: 137_438_953_472,
            used_bytes: Some(70_000_000_000),
            available_bytes: Some(60_000_000_000),
            free_bytes: Some(3_000_000_000),
        });

        let ledger = compute_ledger(inputs, 1);
        let over: Vec<&ModelRow> = ledger
            .models
            .iter()
            .filter(|r| matches!((r.current_bytes, r.potential_bytes), (Some(c), Some(p)) if c > p))
            .collect();
        assert!(!over.is_empty(), "fixture must produce an over-potential row: {:?}", ledger.models);

        // The projection must count what is actually held, not the stale estimate.
        let effective: u64 = ledger
            .models
            .iter()
            .map(|r| match (r.current_bytes, r.potential_bytes) {
                (Some(c), Some(p)) => c.max(p),
                (_, Some(p)) => p,
                _ => 0,
            })
            .sum();
        let other = ledger.machine.other_used_bytes.expect("other_used");
        assert_eq!(
            ledger.machine.projected_total_bytes,
            Some(other + effective),
            "projection must use max(potential, current), not potential alone"
        );

        // The machine's own Σ potential is the SAME clamped sum — the
        // reconciliation identity this page prints (`projected = other +
        // potential`) has to keep holding, and a Σ maximum below Σ current
        // is wrong for exactly the reason a per-row one is.
        assert_eq!(
            ledger.machine.potential_bytes, effective,
            "machine Σ potential must be the clamped sum too"
        );
        assert_eq!(
            ledger.machine.projected_total_bytes,
            Some(other + ledger.machine.potential_bytes),
            "projected = other_used + Σ potential must reconcile exactly"
        );

        // Stamped on the ROW as a number, so every surface reads one
        // server-computed condition instead of re-deriving the floor.
        let flagged: Vec<&ModelRow> = ledger.models.iter().filter(|r| r.over_price_bytes.is_some()).collect();
        assert_eq!(flagged.len(), 1, "exactly the over-potential row carries the overage");
        let r = flagged[0];
        assert_eq!(r.over_price_bytes, Some(r.current_bytes.unwrap() - r.potential_bytes.unwrap()));
        assert_eq!(ledger.machine.over_price_models, 1);

        // …and it does NOT move the row's severity. What was falsified is the
        // estimate's ceiling, not the fit — the fit is measured. Spending the
        // state channel on an epistemic condition would mint a second meaning
        // for color.
        assert_ne!(r.state, LedgerState::Amber, "an over-price row is not thereby unhealthy");

        // And it must be SAID — a silent correction is one nobody fixes.
        assert!(
            ledger
                .messages
                .iter()
                .any(|m| m.severity == Severity::Warn && m.text.contains("more than darkmux priced it")),
            "an over-potential resident must be disclosed: {:?}",
            ledger.messages
        );
    }

    /// (#1854, flap guard) A few MB of jitter above potential must NOT light
    /// the condition. A signal that flickers every poll teaches the operator
    /// to ignore it inside a day — the same "no variance, no information"
    /// lesson as the always-gray lamps, arriving from the other side. Floor is
    /// `max(1% of potential, 256 MiB)`.
    ///
    /// The clamp itself still applies below the floor: the projection always
    /// counts what is held. Only the DISCLOSURE is gated, because that is the
    /// part with an attention cost.
    #[test]
    fn a_trivial_overage_clamps_the_projection_but_stays_silent() {
        let mut inputs = base_inputs();
        // Nudge one worker a hair over its potential — well under the floor.
        let over_by = 10 * 1024 * 1024; // 10 MiB
        let priced = compute_ledger(base_inputs(), 1).models[0].potential_bytes.expect("priced");
        inputs.workers = Some(vec![
            WorkerProc { pid: 1, rss_bytes: priced + over_by, footprint_bytes: None },
            WorkerProc { pid: 2, rss_bytes: 1_000_000_000, footprint_bytes: None },
        ]);
        let ledger = compute_ledger(inputs, 1);
        assert!(
            !ledger.messages.iter().any(|m| m.text.contains("more than darkmux priced it")),
            "a sub-floor overage must not be announced: {:?}",
            ledger.messages
        );
        // Silent at BOTH disclosure altitudes — the row hint and the machine
        // caption's count read this one field, so gating it here gates both.
        assert!(ledger.models.iter().all(|r| r.over_price_bytes.is_none()));
        assert_eq!(ledger.machine.over_price_models, 0);
        // …but the arithmetic still counts the measured size. The clamp is
        // not what has an attention cost; the disclosure is.
        assert_eq!(
            ledger.machine.potential_bytes,
            ledger
                .models
                .iter()
                .map(|r| match (r.current_bytes, r.potential_bytes) {
                    (Some(c), Some(p)) => c.max(p),
                    (_, Some(p)) => p,
                    _ => 0,
                })
                .sum::<u64>(),
            "the clamp applies below the floor even though nothing is said"
        );
        assert!(
            ledger.machine.potential_bytes > priced + 1_000_000_000,
            "sanity: the clamped sum reflects the nudged worker"
        );
    }

    /// The clamp must not fire on the ordinary case, or every machine grows a
    /// permanent warning and the signal is worthless.
    #[test]
    fn a_resident_within_its_potential_is_untouched_and_unremarked() {
        let ledger = compute_ledger(base_inputs(), 1);
        for r in &ledger.models {
            if let (Some(c), Some(p)) = (r.current_bytes, r.potential_bytes) {
                assert!(c <= p, "fixture should be well-priced: {} cur={c} pot={p}", r.identifier);
            }
        }
        assert!(
            !ledger.messages.iter().any(|m| m.text.contains("more than darkmux priced it")),
            "no over-potential message on a well-priced machine: {:?}",
            ledger.messages
        );
        assert!(ledger.models.iter().all(|r| r.over_price_bytes.is_none()));
        assert_eq!(ledger.machine.over_price_models, 0);
        // A CEILING-backed total: no resident was counted at anything other
        // than the price it declared. This is the green the verdict has
        // always claimed to be, and the count is what distinguishes it from
        // the floor-backed one.
        assert_eq!(
            ledger.machine.potential_bytes,
            ledger.models.iter().filter_map(|r| r.potential_bytes).sum::<u64>(),
            "with nothing over price, the clamped sum IS the priced sum"
        );
    }

    /// (#1835 + #1854) The shrink hint may not promise a saving that cannot
    /// materialize. Cutting a resident's ctx lowers its PRICE, but the
    /// projection floors every row at its measured footprint — so once the
    /// shrunken potential falls under what the model is already holding,
    /// further cutting reclaims nothing.
    ///
    /// Found by RUNNING the hint, not reading it: the fixture's hint claimed
    /// 4.70 GB, delivered 4.12 GB, and left the machine in the very band it
    /// was supposed to escape. Reading the formula could not have shown this
    /// — the shortfall only exists once the recomputed ledger re-prices the
    /// row and #1854's clamp catches it.
    #[test]
    fn a_ctx_shrink_cannot_reclaim_below_what_the_resident_is_already_holding() {
        let row = ModelRow {
            identifier: "m".into(),
            model_key: "m".into(),
            owner: Owner::Darkmux,
            loaded_ctx: 32_768,
            weights_bytes: Some(13_000_000_000),
            kv_per_token_bytes: Some(163_840),
            kv_bytes_at_ctx: Some(163_840 * 32_768),
            potential_bytes: Some(19_118_709_120),
            potential_source: Some(PotentialSource::Arch),
            current_bytes: Some(15_000_000_000),
            state: LedgerState::Green,
            over_price_bytes: None,
            shrink_hint: None,
        };
        // Naive KV arithmetic: 163_840 x (32_768 - 4_096) = 4.70 GB.
        let naive = 163_840u64 * (32_768 - 4_096);
        assert_eq!(naive, 4_697_620_480);
        // What can actually be reclaimed: down to the measured footprint.
        assert_eq!(achievable_ctx_saving(&row), 19_118_709_120 - 15_000_000_000);
        assert!(achievable_ctx_saving(&row) < naive, "the cap must bind here");

        // The inverted case: a resident holding far less than its price is
        // bounded by ctx arithmetic as before, and the cap must not bite.
        let lazy = ModelRow { current_bytes: Some(1_000_000_000), ..row.clone() };
        assert_eq!(achievable_ctx_saving(&lazy), naive);

        // And with no measured footprint at all there is nothing to floor
        // against — never a fabricated cap.
        let unattributed = ModelRow { current_bytes: None, ..row };
        assert_eq!(achievable_ctx_saving(&unattributed), naive);
    }


    #[test]
    fn sum_potential_equal_to_limit_is_green_inclusive() {
        // projected_total == limit: the green arm's `≤` is inclusive at
        // equality. `used_bytes` pinned to Σ current so `other_used` is
        // exactly 0 (#1821) — projected_total then equals Σ potential
        // exactly, isolating the equality-boundary claim this test makes
        // from the separate other-tenants arithmetic.
        let mut inputs = base_inputs();
        inputs.pool = inputs.pool.map(|p| PoolSnapshot { used_bytes: Some(33_000_000_000), ..p });
        inputs.budget_bytes = Some(JUDGE_POTENTIAL + DEVSTRAL_POTENTIAL);
        let ledger = compute_ledger(inputs, 1);
        assert_eq!(ledger.machine.state, LedgerState::Green);
        assert!(ledger.machine.shrink_hint.is_none());
    }

    /// (#1854) The CLI is the viewer's twin, so it discloses in the same two
    /// places: a footnote on the ROW (which is where the confusing
    /// `potential X · current Y` pair with Y > X actually prints, and the
    /// machine-level warning several lines below cannot repair it in place)
    /// and the warning itself. NOT on the machine-total line.
    #[test]
    fn render_human_footnotes_the_over_price_row_but_keeps_it_off_the_machine_line() {
        let mut inputs = base_inputs();
        inputs.workers = Some(vec![
            WorkerProc { pid: 1, rss_bytes: 60_000_000_000, footprint_bytes: None },
            WorkerProc { pid: 2, rss_bytes: 5_000_000_000, footprint_bytes: None },
        ]);
        inputs.pool = Some(PoolSnapshot {
            capacity_bytes: 137_438_953_472,
            used_bytes: Some(70_000_000_000),
            available_bytes: Some(60_000_000_000),
            free_bytes: Some(3_000_000_000),
        });
        let text = render_human(&compute_ledger(inputs, 1));
        assert!(text.contains("more than priced"), "the row carries its own footnote: {text}");
        // …and NOT at glance. `(1 at measured)` read as noise to the operator
        // cold — it only parses for a reader who already knows the argument
        // behind it — so the machine line stays about fit.
        assert!(!text.contains("at measured"), "no glance-layer jargon: {text}");
    }

    /// The inverted case for the parenthetical's rebuild from a 2x2 match to
    /// a clause list: a well-priced machine's machine-total line must gain no
    /// parenthetical at all, and the existing clauses must not have changed
    /// shape in the rewrite.
    #[test]
    fn render_human_leaves_an_ordinary_machine_line_unparenthesized() {
        let text = render_human(&compute_ledger(base_inputs(), 1));
        assert!(!text.contains("at measured"), "nothing over price ⇒ no clause: {text}");
        assert!(!text.contains("more than priced"), "and no row footnote: {text}");
        // Scoped to the COUNTS parenthetical: the limit-source description
        // later on the same line legitimately carries its own parentheses.
        let machine_line = text.lines().find(|l| l.starts_with("machine: ")).expect("machine line");
        let potential_clause = machine_line.split(" · ").next().expect("potential clause");
        assert!(!potential_clause.contains('('), "no counts parenthetical at all: {machine_line}");
    }

    /// Contract 5 (schema leniency) for the #1854 additions — a 2.0 payload
    /// has neither key. Same guarantee `estimated_models_defaults_to_zero_on_
    /// a_pre_1819_payload_missing_the_field` pins for #1819's pair, and the
    /// same real path: a heterogeneous fleet where one machine is a release
    /// behind (`main.rs::cmd_machine_resources` parses a peer's ledger).
    /// Without `serde(default)` the whole table silently degrades to a raw
    /// JSON dump.
    #[test]
    fn the_over_price_fields_default_to_absent_on_a_pre_1854_payload() {
        let mut v = serde_json::to_value(compute_ledger(base_inputs(), 1)).expect("serialize");
        v["machine"].as_object_mut().unwrap().remove("over_price_models");
        for m in v["models"].as_array_mut().unwrap() {
            m.as_object_mut().unwrap().remove("over_price_bytes");
        }
        let back: ModelLedger = serde_json::from_value(v).expect("a 2.0 payload must still parse");
        assert_eq!(back.machine.over_price_models, 0);
        assert!(back.models.iter().all(|r| r.over_price_bytes.is_none()));
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

    /// #1821 — the cascade must key on `projected_total` (darkmux's Σ
    /// potential PLUS what everything else on the machine is holding), not
    /// on darkmux's Σ potential alone. Same residents/potential as
    /// `green_when_sum_potential_fits_the_limit` above (Σ potential ≈ 38.39
    /// GB, well under the 137.44 GB limit) — the ONLY thing this test
    /// changes is `pool.used_bytes`, driven high enough that OTHER tenants
    /// push the projected total over the limit. Under the pre-#1821 rule
    /// (`sum_potential <= limit`) this machine would have reported GREEN;
    /// it must not.
    /// (#1854) The hint's PRINTED saving is capped the same way the
    /// projection is. The suggested ctx rounds down to a 4 K multiple, so
    /// the KV arithmetic for the rounded cut can exceed the overshoot by a
    /// wide margin — and here it also exceeds what the resident can give
    /// back at all: devstral is priced at 19.12 GB and already holding
    /// 15.00 GB, so no ctx cut reclaims more than 4.12 GB, however the
    /// formula reads. Before this cap the hint promised the formula's
    /// 4.70 GB. Red-provable: swap `saved` for `priced_off` and the string
    /// says "cuts 4.70 GB".
    #[test]
    fn the_shrink_hint_prints_the_saving_that_can_materialize_not_the_kv_formula() {
        let mut inputs = base_inputs();
        // Pin `used_bytes` to Σ current so `other_used` is 0 and the budget
        // arithmetic below is exact.
        inputs.pool = inputs.pool.map(|p| PoolSnapshot { used_bytes: Some(33_000_000_000), ..p });
        // Overshoot chosen so devstral (the KV hog) COVERS it — but only just
        // under its 4.12 GB ceiling — while the rounded-down ctx (4096) prices
        // off the full 4.70 GB.
        let projected = JUDGE_POTENTIAL + DEVSTRAL_POTENTIAL;
        let overshoot = 4_100_000_000u64;
        inputs.budget_bytes = Some(projected - overshoot);
        let ledger = compute_ledger(inputs, 1);
        assert_eq!(ledger.machine.state, LedgerState::Amber);
        let hint = ledger.machine.shrink_hint.clone().expect("amber names a shrink");
        assert!(hint.contains("reload devstral at ctx 4096"), "hint: {hint}");
        // devstral's potential minus what it holds: 19,118,709,120 − 15,000,000,000.
        assert!(hint.contains("cuts 4.12 GB"), "hint: {hint}");
        assert!(!hint.contains("4.70 GB"), "the KV formula's figure must not print: {hint}");
    }

    /// (#1854) When every shrinkable resident is already holding at least
    /// its price, no ctx cut reclaims anything — and the hint must say so
    /// rather than name one of them with a 0 B saving.
    ///
    /// Amber needs `Σ current ≤ limit < projected`, and a clamped row's
    /// projection IS its current — so with both rows clamped the lift past
    /// the limit has to come from OTHER tenants (#1821), which is exactly
    /// the case where a ctx cut helps nothing: darkmux is not the one
    /// holding the difference.
    #[test]
    fn a_hint_with_no_positive_saving_anywhere_falls_to_the_no_shrinkable_context_arm() {
        let mut inputs = base_inputs();
        // Both workers above their rows' prices (judge ~19.27 GB, devstral
        // ~19.12 GB): both rows clamp to 20 GB, both achievable savings are 0.
        inputs.workers = Some(vec![
            WorkerProc { pid: 1, rss_bytes: 20_000_000_000, footprint_bytes: None },
            WorkerProc { pid: 2, rss_bytes: 20_000_000_000, footprint_bytes: None },
        ]);
        // Σ current 40 GB ≤ budget 44 GB < projected 47 GB (7 GB of others).
        inputs.pool = inputs.pool.map(|p| PoolSnapshot { used_bytes: Some(47_000_000_000), ..p });
        inputs.budget_bytes = Some(44_000_000_000);
        let ledger = compute_ledger(inputs, 1);
        assert_eq!(ledger.machine.over_price_models, 2, "{:?}", ledger.models);
        assert_eq!(ledger.machine.state, LedgerState::Amber, "{:?}", ledger.machine);
        let hint = ledger.machine.shrink_hint.clone().expect("amber names a hint");
        assert!(hint.contains("no shrinkable context"), "hint: {hint}");
        assert!(!hint.contains("0 B"), "hint: {hint}");
    }

    /// (#1854 review) The fallback hint's "largest single saving is X at
    /// ctx N (bytes)" pairs a byte figure with the ctx that REACHES it. Before
    /// the cap the two were one formula (`kv × (ctx − floor)`, so N was always
    /// the floor). With the cap binding, the whole achievable saving is
    /// reached long before the floor — printing 4096 beside it tells the
    /// operator to gut a context for a saving a far smaller cut delivers.
    #[test]
    fn the_fallback_hint_names_the_ctx_that_reaches_the_capped_saving_not_the_floor() {
        // Workers rank-match by weights, so judge (17.18 GB weights) takes the
        // larger RSS. judge priced at 19,272,177,280 and holding 18.95 GB:
        // 322,177,280 B reclaimable at most, which its 20,480 B/token rate
        // reaches 15,732 tokens in — 65536 − 15732 = 49,804, floored to a 4K
        // multiple = 49152. devstral holds 18.90 GB against 19.12 GB: 0.22 GB.
        // Overshoot 0.39 GB exceeds both, so the fallback branch fires and
        // names the larger (judge).
        let mut inputs = base_inputs();
        inputs.workers = Some(vec![
            WorkerProc { pid: 1, rss_bytes: 18_950_000_000, footprint_bytes: None },
            WorkerProc { pid: 2, rss_bytes: 18_900_000_000, footprint_bytes: None },
        ]);
        inputs.pool = inputs.pool.map(|p| PoolSnapshot { used_bytes: Some(37_850_000_000), ..p });
        inputs.budget_bytes = Some(38_000_000_000);
        let ledger = compute_ledger(inputs, 1);
        assert_eq!(ledger.machine.state, LedgerState::Amber, "{:?}", ledger.machine);
        let hint = ledger.machine.shrink_hint.clone().expect("amber names a hint");
        assert!(hint.contains("no single ctx reduction reaches green"), "hint: {hint}");
        assert!(hint.contains("largest single saving is judge at ctx 49152 (322 MB)"), "hint: {hint}");
    }

    /// (#1854 review) The materiality floor is a strict `>`: an overage of
    /// EXACTLY the floor is not disclosed (still clamped, still silent), one
    /// byte more is.
    #[test]
    fn the_over_price_floor_is_exclusive_at_the_boundary() {
        // devstral's floor is max(1% of its 19.12 GB price, 256 MiB) = 256 MiB.
        // judge is given a larger RSS (30 GB) so rank-pairing sends the
        // boundary figure to devstral; judge is over its own price and
        // disclosed regardless — this test reads the devstral row only.
        let floor = 256u64 * 1024 * 1024;
        let at_floor = DEVSTRAL_POTENTIAL + floor;
        for (rss, expect_disclosed) in [(at_floor, false), (at_floor + 1, true)] {
            let mut inputs = base_inputs();
            inputs.workers = Some(vec![
                WorkerProc { pid: 1, rss_bytes: 30_000_000_000, footprint_bytes: None },
                WorkerProc { pid: 2, rss_bytes: rss, footprint_bytes: None },
            ]);
            let ledger = compute_ledger(inputs, 1);
            let dev = ledger.models.iter().find(|r| r.model_key == "devstral").unwrap();
            assert_eq!(dev.current_bytes, Some(rss), "fixture pairing");
            assert_eq!(dev.over_price_bytes.is_some(), expect_disclosed, "rss={rss} {dev:?}");
            // Clamped either way: the projection never reads below measured.
            assert_eq!(ledger.machine.potential_bytes, 30_000_000_000 + rss);
        }
    }

    /// (#1854 review) The machine line's counts parenthetical, every arm:
    /// unpriced alone, estimated alone (pinned elsewhere), and both together
    /// in this order with this separator.
    #[test]
    fn render_human_machine_line_counts_join_unpriced_then_estimated() {
        let mut inputs = base_inputs();
        // estimated: a GGUF resident with a catalog size but no arch facts.
        inputs.catalog.push(CatalogFact { model_key: "phi-4-gguf".into(), size_bytes: Some(9_053_136_497) });
        inputs.residents.push(resident("microsoft/phi-4", "phi-4-gguf", 8_192));
        // unpriced: no arch facts AND no catalog entry.
        inputs.residents.push(resident("mystery", "mystery", 8_192));
        let ledger = compute_ledger(inputs.clone(), 1);
        assert_eq!((ledger.machine.unpriced_models, ledger.machine.estimated_models), (1, 1));
        let text = render_human(&ledger);
        assert!(text.contains(" (+1 unpriced, 1 estimated) · current"), "{text}");

        // unpriced alone
        inputs.residents.retain(|r| r.model_key != "phi-4-gguf");
        let text = render_human(&compute_ledger(inputs, 1));
        assert!(text.contains(" (+1 unpriced) · current"), "{text}");
    }

    #[test]
    fn amber_verdict_and_shrink_hint_key_on_projected_total_not_darkmux_alone() {
        let mut inputs = base_inputs();
        // Σ current (the two workers) is 33 GB; pinning `used_bytes` to 135
        // GB makes `other_used` = 135 - 33 = 102 GB — on its own already
        // most of the 137.44 GB limit, before darkmux's own ~38.39 GB is
        // even added.
        inputs.pool = inputs.pool.map(|p| PoolSnapshot { used_bytes: Some(135_000_000_000), ..p });
        let ledger = compute_ledger(inputs, 1);

        let sum_potential = JUDGE_POTENTIAL + DEVSTRAL_POTENTIAL;
        assert!(
            sum_potential <= 137_438_953_472,
            "the darkmux-alone sum must still fit — that's the whole point of this test"
        );
        assert_eq!(
            ledger.machine.other_used_bytes,
            Some(102_000_000_000),
            "other_used = pool.used_bytes - darkmux current, emitted so the arithmetic is checkable"
        );
        assert_eq!(
            ledger.machine.projected_total_bytes,
            Some(102_000_000_000 + sum_potential),
            "projected_total = other_used + Σ potential"
        );
        assert_eq!(
            ledger.machine.state,
            LedgerState::Amber,
            "other tenants push the PROJECTED total over the limit even though darkmux's own sum fits — \
             a cascade that ignores other_used would report Green here"
        );
        // The shrink hint's own arithmetic must be honest about which total
        // it is targeting — see `hint_target_key`/`shrink_hint`'s doc.
        assert!(ledger.machine.shrink_hint.is_some());
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
        // Budget between Σ current (33 GB) and Σ potential (~38.4 GB):
        // running under the limit only because lazy allocation hasn't
        // materialized — amber, with the config shrink named.
        //
        // `used_bytes` pinned to Σ current so `other_used` is exactly 0 (#1821) — this
        // test is about the shrink-hint TARGET, not the other-tenants
        // arithmetic, which `amber_verdict_and_shrink_hint_key_on_
        // projected_total_not_darkmux_alone` below covers directly.
        let mut inputs = base_inputs();
        inputs.pool = inputs.pool.map(|p| PoolSnapshot { used_bytes: Some(33_000_000_000), ..p });
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
            WorkerProc { pid: 1, rss_bytes: JUDGE_POTENTIAL + 1_000_000, footprint_bytes: None },
            WorkerProc { pid: 2, rss_bytes: 10_000_000_000, footprint_bytes: None },
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
    fn swap_alone_is_a_row_not_a_red_trigger() {
        // Swap-in-use is a monotonic high-water mark macOS never reclaims, so
        // it tracks UPTIME rather than distress: 6.96 GB observed at 94%
        // memorystatus free on a healthy box with 45 days up. It reports; it
        // does not alarm. Same call as `compressor_alone_is_a_row_...`.
        let mut inputs = base_inputs();
        inputs.swap_used_bytes = Some(64 << 30); // 64 GiB — absurd, still not a trigger
        let ledger = compute_ledger(inputs, 1);
        assert!(!ledger.pressure.red);
        assert_eq!(ledger.machine.state, LedgerState::Green);
        // Still REPORTED — demoting the trigger must not drop the row.
        assert_eq!(ledger.pressure.swap_used_bytes, Some(64 << 30));
    }

    #[test]
    fn red_on_memory_pressure_free_percent_signal() {
        let mut inputs = base_inputs();
        inputs.margin_percent = Some(MARGIN_PERCENT_RED - 1);
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
        inputs.workers = Some(vec![WorkerProc { pid: 1, rss_bytes: 30_000_000_000, footprint_bytes: None }]);
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
        assert!(ledger.messages.iter().any(|m| m.severity == Severity::Warn && m.text.contains("mystery-model")));
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
        inputs.margin_percent = Some(MARGIN_PERCENT_RED - 1);
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

    // A FULL vm_stat sample (#1821) — every label the pool decomposition
    // reads, chosen as round page counts so the expected byte math is
    // hand-checkable rather than trusted. Deliberately a SEPARATE const
    // from `VM_STAT` above: that one's own test pins the "label absent"
    // case (`Pages purgeable` → `None`), which adding a line here would
    // quietly break.
    const VM_STAT_FULL: &str = "Mach Virtual Memory Statistics: (page size of 16384 bytes)\n\
        Pages free:                              1000.\n\
        Pages active:                            2000.\n\
        Pages inactive:                          1500.\n\
        Pages speculative:                        300.\n\
        Pages purgeable:                          200.\n\
        Pages wired down:                         900.\n\
        Pages occupied by compressor:             700.\n";

    /// `used_bytes` is Activity-Monitor-style — `wired + compressor +
    /// (active + inactive - purgeable)` — NOT the cruder implied
    /// `capacity - free` this page used to lean on (#1821 finding: that
    /// implied figure swung 1.8→61 GiB in one session). Page math:
    /// `(900 + 700 + (2000 + 1500 - 200)) * 16384 = 4900 * 16384 =
    /// 80,281,600`. `capacity - free`, by contrast, depends on a capacity
    /// this function never receives — the two are structurally different
    /// computations, and this test pins the ONE this function actually
    /// performs.
    #[test]
    fn pool_used_bytes_is_activity_monitor_style() {
        let (used, _available, _free, _compressor) = parse_pool_from_vm_stat(VM_STAT_FULL);
        assert_eq!(used, Some(80_281_600));
    }

    /// `available_bytes` is the COLLOQUIAL figure — `free + inactive +
    /// speculative` — not truly-free pages alone. Page math:
    /// `(1000 + 1500 + 300) * 16384 = 2800 * 16384 = 45,875,200`, which is
    /// larger than `free_bytes` alone (`1000 * 16384 = 16,384,000`) by
    /// exactly the reclaimable inactive+speculative pages neither the old
    /// `available_bytes` nor `kern.memorystatus_level` counted (issue
    /// finding 7).
    #[test]
    fn pool_available_bytes_is_the_colloquial_figure_not_truly_free() {
        let (_used, available, free, _compressor) = parse_pool_from_vm_stat(VM_STAT_FULL);
        assert_eq!(available, Some(45_875_200));
        assert_eq!(free, Some(16_384_000));
        assert_ne!(available, free, "available and free must be DIFFERENT figures, not the same one twice");
    }

    /// The compressor figure folds into the same one-`vm_stat`-read helper
    /// (#1821) without changing what it reports.
    #[test]
    fn pool_compressor_bytes_is_unchanged_by_the_fold_into_one_helper() {
        let (_used, _available, _free, compressor) = parse_pool_from_vm_stat(VM_STAT_FULL);
        assert_eq!(compressor, Some(11_468_800));
    }

    /// Each of the four figures degrades INDEPENDENTLY on a missing label —
    /// never panics, never silently substitutes a wrong figure for a
    /// missing one, and a gap in ONE figure's inputs does not take the
    /// others down with it (#1286 leniency, applied to the new decomposition).
    #[test]
    fn pool_decomposition_degrades_each_figure_independently_on_a_missing_label() {
        // No "Pages purgeable" line: `used` (which needs it) goes `None`;
        // `available`/`free`/`compressor` (which don't) stay populated.
        let (used, available, free, compressor) = parse_pool_from_vm_stat(VM_STAT);
        assert_eq!(used, None, "used needs purgeable, which VM_STAT lacks");
        assert_eq!(available, None, "available needs speculative, which VM_STAT also lacks");
        assert_eq!(free, Some(228_186 * 16_384));
        assert_eq!(compressor, Some(131_072 * 16_384));
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

    /// Rank-pairing must use WEIGHTS, not potential. The live case from
    /// #1821: a small model at a huge context outranks a bigger model on
    /// potential while holding a fraction of its memory, so ranking by
    /// potential hands each the other's figure.
    #[test]
    fn workers_pair_by_weights_so_a_huge_ctx_does_not_outrank_real_weights() {
        let g = |x: f64| (x * (1u64 << 30) as f64) as u64;
        let mut inputs = base_inputs();
        inputs.residents.clear();
        inputs.catalog.clear();

        // 2.01 GiB of weights at a huge ctx -> potential 19.18, footprint 2.77
        inputs.catalog.push(CatalogFact { model_key: "small-huge-ctx".into(), size_bytes: Some(g(2.01)) });
        inputs.residents.push(resident("darkmux:small", "small-huge-ctx", 120_000));
        // 8.43 GiB of weights at a small ctx -> potential 12.25, footprint 11.74
        inputs.catalog.push(CatalogFact { model_key: "big-small-ctx".into(), size_bytes: Some(g(8.43)) });
        inputs.residents.push(resident("user/big", "big-small-ctx", 16_384));

        inputs.workers = Some(vec![
            WorkerProc { pid: 1, rss_bytes: g(11.74), footprint_bytes: Some(g(3.25)) },
            WorkerProc { pid: 2, rss_bytes: g(0.25), footprint_bytes: Some(g(2.77)) },
        ]);

        let ledger = compute_ledger(inputs, 1);
        let small = ledger.models.iter().find(|m| m.model_key == "small-huge-ctx").unwrap();
        let big = ledger.models.iter().find(|m| m.model_key == "big-small-ctx").unwrap();

        // The big-weights model gets the big footprint, despite the small one
        // having the larger POTENTIAL.
        assert_eq!(big.current_bytes, Some(g(11.74)));
        assert_eq!(small.current_bytes, Some(g(2.77)));
        assert!(small.potential_bytes.unwrap() > big.potential_bytes.unwrap(), "the trap this guards");
    }

    /// The gather WIRING, with the probe injected. Without this, a mutation
    /// that stopped filling footprint passed all 157 tests.
    #[test]
    fn enrichment_fills_every_worker_from_the_probe() {
        let ws = vec![
            WorkerProc { pid: 7, rss_bytes: 100, footprint_bytes: None },
            WorkerProc { pid: 9, rss_bytes: 200, footprint_bytes: None },
        ];
        let out = enrich_with_footprint(ws, |pid| Some((pid as u64) * 1_000));
        assert_eq!(out[0].footprint_bytes, Some(7_000));
        assert_eq!(out[1].footprint_bytes, Some(9_000));
        // ...and the resolved figure follows it, not the RSS it replaced.
        assert_eq!(out[0].memory_bytes(), 7_000);
    }

    /// A probe that cannot answer leaves `None`, and the worker falls back to
    /// RSS rather than to zero.
    #[test]
    fn enrichment_leaves_none_when_the_probe_declines() {
        let ws = vec![WorkerProc { pid: 7, rss_bytes: 4_242, footprint_bytes: None }];
        let out = enrich_with_footprint(ws, |_| None);
        assert_eq!(out[0].footprint_bytes, None);
        assert_eq!(out[0].memory_bytes(), 4_242);
    }

    /// The real FFI, against THIS process: `rusage_info_v0`'s layout is a
    /// frozen ABI, and a wrong field offset would return garbage rather than
    /// fail loudly. Asserting a plausible range is what catches a shifted
    /// struct — a mis-parsed field reads as either 0 or an absurd value.
    #[cfg(target_os = "macos")]
    #[test]
    fn phys_footprint_reads_a_plausible_figure_for_the_test_process_itself() {
        let me = std::process::id() as i64;
        let fp = phys_footprint(me).expect("own footprint is readable");
        assert!(fp > 1_000_000, "implausibly small: {fp} bytes — struct offset likely wrong");
        assert!(fp < 100 * (1u64 << 30), "implausibly large: {fp} bytes — struct offset likely wrong");
    }

    /// A pid that cannot exist reports `None`, never a fabricated figure.
    #[cfg(target_os = "macos")]
    #[test]
    fn phys_footprint_declines_for_a_dead_pid() {
        assert_eq!(phys_footprint(i64::from(i32::MAX)), None);
    }

    /// `memory_bytes` is `max(rss, footprint)`, and these are the two REAL
    /// backend shapes measured live on 2026-08-15 — the reason neither
    /// counter can be used alone.
    #[test]
    fn worker_memory_takes_whichever_counter_the_backend_actually_populates() {
        let g = |x: f64| (x * (1u64 << 30) as f64) as u64;

        // MLX: weights live in Metal/IOAccelerator buffers. RSS sees ~nothing.
        let mlx_35b = WorkerProc { pid: 1, rss_bytes: g(0.14), footprint_bytes: Some(g(22.27)) };
        assert_eq!(mlx_35b.memory_bytes(), g(22.27));

        // GGUF: weights are mmapped CLEAN file-backed pages. Footprint
        // deliberately excludes them (evictable without swapping) — true,
        // and exactly wrong for "is this occupying RAM right now".
        let gguf_phi4 = WorkerProc { pid: 2, rss_bytes: g(11.74), footprint_bytes: Some(g(3.25)) };
        assert_eq!(gguf_phi4.memory_bytes(), g(11.74));

        // Reading footprint alone — the naive "fix" — would have reported the
        // GGUF model at 3.25 instead of 11.7: a different wrong number.
        assert!(gguf_phi4.footprint_bytes.unwrap() < gguf_phi4.rss_bytes);
    }

    /// Degrades to RSS when no footprint is readable (non-macOS, or a worker
    /// that exited between enumeration and probe) — never to zero, which
    /// would silently erase a live model from the machine total.
    #[test]
    fn worker_memory_falls_back_to_rss_when_footprint_is_unavailable() {
        let w = WorkerProc { pid: 3, rss_bytes: 5_000_000_000, footprint_bytes: None };
        assert_eq!(w.memory_bytes(), 5_000_000_000);
    }

    /// The union is APPROXIMATED, never summed: dirty anonymous pages appear
    /// in both counters, so adding them double-counts.
    #[test]
    fn worker_memory_never_sums_the_two_counters() {
        let w = WorkerProc { pid: 4, rss_bytes: 3_000_000_000, footprint_bytes: Some(2_000_000_000) };
        assert_eq!(w.memory_bytes(), 3_000_000_000);
        assert_ne!(w.memory_bytes(), 5_000_000_000);
    }

    /// The end-to-end consequence, and the whole point of #1821: a machine
    /// whose workers report near-zero RSS but real footprints must total the
    /// footprints, not the RSS. Understating this is what put 271 MiB on a
    /// dial next to 25 GiB of actually-resident models.
    #[test]
    fn machine_current_totals_the_larger_counter_not_rss() {
        let g = |x: f64| (x * (1u64 << 30) as f64) as u64;
        let mut inputs = base_inputs();
        inputs.workers = Some(vec![
            WorkerProc { pid: 1, rss_bytes: g(0.14), footprint_bytes: Some(g(22.27)) },
            WorkerProc { pid: 2, rss_bytes: g(0.25), footprint_bytes: Some(g(2.77)) },
        ]);
        let ledger = compute_ledger(inputs, 1);
        let total = ledger.machine.current_bytes.expect("attributed");
        assert_eq!(total, g(22.27) + g(2.77));
        // ...and emphatically not the RSS sum, which is what shipped.
        assert!(total > g(20.0));
    }

    #[test]
    fn worker_rss_filters_llmworker_rows_and_scales_kib() {
        let ps = "  735  18432000 /Applications/LM Studio.app/Contents/Resources/app/.webpack/main/llmworker.js --stdio\n\
             812      2048 /usr/libexec/somethingelse\n\
             990    512000 node /opt/lmstudio/llmworker.js\n";
        let workers = parse_worker_rss(ps);
        assert_eq!(workers.len(), 2);
        assert_eq!(workers[0], WorkerProc { pid: 735, rss_bytes: 18_432_000 * 1024, footprint_bytes: None });
        assert_eq!(workers[1], WorkerProc { pid: 990, rss_bytes: 512_000 * 1024, footprint_bytes: None });
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
    /// to an `error`-severity message (#1821: a probe failure means the
    /// reading itself is untrustworthy), the operator's real LMStudio is
    /// never touched (no ls entries ⇒ the arch reader opens no files), and
    /// the observer-cost stamp is populated. Kernel-counter probes run for
    /// real (read-only), same as the MacProbe tests.
    #[test]
    fn gather_with_missing_lms_degrades_loud_and_stamps_cost() {
        let ledger = gather_with_bin("/nonexistent/darkmux-test-lms-bin");
        assert!(ledger.models.is_empty());
        assert!(
            ledger.messages.iter().any(|m| m.severity == Severity::Error && m.text.contains("ps")),
            "lms ps failure surfaces as an error-severity message: {:?}",
            ledger.messages
        );
        assert_eq!(ledger.schema_version, LEDGER_SCHEMA_VERSION);
        assert!(ledger.generated_at_ms > 1_700_000_000_000);
        // gather_ms is stamped (may legitimately be 0 ms on a fast box, so
        // just assert the render path carries it).
        assert!(render_human(&ledger).contains("gather:"));
    }

    // ── #1820: config.json → GGUF header → (absent) resolution order ────
    //
    // These tests exercise `resolve_arch_facts` — the WIRING between the
    // two readers, not the byte-level GGUF parser itself (that gets its own
    // exhaustive coverage in `gestalt_host::gguf_facts`). Real filesystem
    // fixtures in a temp dir, never the operator's real `~/.lmstudio`.

    /// Minimal valid synthetic GGUF bytes carrying just the scalar fields
    /// `resolve_arch_facts` needs (no tokenizer arrays — the array-skip path
    /// is `gguf_facts`'s own concern, already covered there).
    fn tiny_gguf_bytes(block_count: u32, head_count: u32, head_count_kv: u32, embedding_length: u32) -> Vec<u8> {
        fn write_string(buf: &mut Vec<u8>, s: &str) {
            buf.extend_from_slice(&(s.len() as u64).to_le_bytes());
            buf.extend_from_slice(s.as_bytes());
        }
        fn write_u32_kv(buf: &mut Vec<u8>, key: &str, v: u32) {
            write_string(buf, key);
            buf.extend_from_slice(&4u32.to_le_bytes()); // GGUF T_UINT32
            buf.extend_from_slice(&v.to_le_bytes());
        }
        let mut buf = Vec::new();
        buf.extend_from_slice(b"GGUF");
        buf.extend_from_slice(&3u32.to_le_bytes());
        buf.extend_from_slice(&0u64.to_le_bytes()); // tensor_count
        buf.extend_from_slice(&5u64.to_le_bytes()); // kv_count
        write_string(&mut buf, "general.architecture");
        buf.extend_from_slice(&8u32.to_le_bytes()); // GGUF T_STRING
        write_string(&mut buf, "testarch");
        write_u32_kv(&mut buf, "testarch.block_count", block_count);
        write_u32_kv(&mut buf, "testarch.attention.head_count", head_count);
        write_u32_kv(&mut buf, "testarch.attention.head_count_kv", head_count_kv);
        write_u32_kv(&mut buf, "testarch.embedding_length", embedding_length);
        buf
    }

    fn write_fixture_config_json(dir: &std::path::Path, layers: u64, kv_heads: u64, head_dim: u64) {
        std::fs::create_dir_all(dir).unwrap();
        let body = serde_json::json!({
            "num_hidden_layers": layers,
            "num_key_value_heads": kv_heads,
            "head_dim": head_dim,
            "quantization": { "bits": 4 }
        });
        std::fs::write(dir.join("config.json"), serde_json::to_vec(&body).unwrap()).unwrap();
    }

    /// A fresh, uniquely-named temp root per test, removed on drop — real
    /// filesystem fixtures without leaving anything behind and without a
    /// name collision across parallel test threads.
    struct TempTestRoot(std::path::PathBuf);
    impl TempTestRoot {
        fn new(label: &str) -> Self {
            use std::sync::atomic::{AtomicU64, Ordering};
            static COUNTER: AtomicU64 = AtomicU64::new(0);
            let n = COUNTER.fetch_add(1, Ordering::Relaxed);
            let dir = std::env::temp_dir()
                .join(format!("darkmux-ledger-arch-resolve-test-{}-{label}-{n}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            TempTestRoot(dir)
        }
    }
    impl Drop for TempTestRoot {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn resolve_arch_facts_prefers_config_json_over_gguf_when_both_present() {
        let root = TempTestRoot::new("precedence");
        let dir = root.0.join("dual-format-model");
        write_fixture_config_json(&dir, 8, 8, 64); // config.json: 8/8/64
        std::fs::write(dir.join("weights.gguf"), tiny_gguf_bytes(40, 40, 10, 5120)).unwrap(); // gguf: 40/10/128

        let residents =
            vec![ResidentInput { identifier: "darkmux:dual".into(), model_key: "dual-format-model".into(), loaded_ctx: 4096 }];
        let arch_reader = ArchFactsReader::with_root(&root.0);
        let gguf_reader = GgufFactsReader::with_root(&root.0);
        let arch = resolve_arch_facts(&residents, &arch_reader, &gguf_reader);
        let facts = arch.get("dual-format-model").expect("resolved");
        assert_eq!(facts.total_layers, 8, "config.json must win — GGUF never overrides an existing config.json");
        assert_eq!(facts.kv_heads, 8);
        assert_eq!(facts.head_dim, 64);
    }

    #[test]
    fn resolve_arch_facts_falls_to_gguf_header_when_no_config_json() {
        let root = TempTestRoot::new("gguf-fallback");
        let dir = root.0.join("gguf-only-model");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("weights.gguf"), tiny_gguf_bytes(40, 40, 10, 5120)).unwrap();

        let residents = vec![ResidentInput {
            identifier: "darkmux:gguf-only".into(),
            model_key: "gguf-only-model".into(),
            loaded_ctx: 4096,
        }];
        let arch_reader = ArchFactsReader::with_root(&root.0);
        let gguf_reader = GgufFactsReader::with_root(&root.0);
        let arch = resolve_arch_facts(&residents, &arch_reader, &gguf_reader);
        let facts = arch.get("gguf-only-model").expect("resolved via GGUF header");
        assert_eq!(facts.total_layers, 40);
        assert_eq!(facts.kv_heads, 10);
        assert_eq!(facts.head_dim, 128);
        assert_eq!(facts.full_attention_layers, 40, "dense default — no layer_types-equivalent in GGUF");
        assert_eq!(facts.kv_bytes_per_element, KV_BYTES_PER_ELEMENT_V1);
    }

    #[test]
    fn resolve_arch_facts_leaves_model_absent_when_neither_readable() {
        let root = TempTestRoot::new("neither");
        let residents = vec![ResidentInput {
            identifier: "darkmux:nowhere".into(),
            model_key: "no-such-model".into(),
            loaded_ctx: 4096,
        }];
        let arch_reader = ArchFactsReader::with_root(&root.0);
        let gguf_reader = GgufFactsReader::with_root(&root.0);
        let arch = resolve_arch_facts(&residents, &arch_reader, &gguf_reader);
        assert!(
            !arch.contains_key("no-such-model"),
            "neither reader has anything for this model — must stay absent, never guessed"
        );
    }

    /// The end-to-end product-level guarantee #1820 promises, proven through
    /// the SAME `compute_ledger` entry point the live gather uses (not just
    /// the isolated `resolve_arch_facts` helper): a GGUF-only resident's
    /// `potential_bytes` prices via `PotentialSource::Arch` — a
    /// MEASUREMENT — never `PotentialSource::Estimated`.
    #[test]
    fn gguf_only_resident_prices_as_arch_not_estimated_through_compute_ledger() {
        let root = TempTestRoot::new("e2e");
        let dir = root.0.join("gguf-e2e-model");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("weights.gguf"), tiny_gguf_bytes(40, 40, 10, 5120)).unwrap();

        let residents = vec![ResidentInput {
            identifier: "darkmux:gguf-e2e".into(),
            model_key: "gguf-e2e-model".into(),
            loaded_ctx: 4096,
        }];
        let arch_reader = ArchFactsReader::with_root(&root.0);
        let gguf_reader = GgufFactsReader::with_root(&root.0);
        let arch = resolve_arch_facts(&residents, &arch_reader, &gguf_reader);

        let mut inputs = base_inputs();
        inputs.residents = residents;
        inputs.arch = arch;
        inputs.catalog =
            vec![CatalogFact { model_key: "gguf-e2e-model".into(), size_bytes: Some(9_000_000_000) }];
        let ledger = compute_ledger(inputs, 1);
        let row = ledger.models.iter().find(|m| m.model_key == "gguf-e2e-model").expect("row present");
        assert_eq!(
            row.potential_source,
            Some(PotentialSource::Arch),
            "GGUF-derived facts must price as a MEASUREMENT, not an estimate: {row:?}"
        );
        assert!(row.potential_bytes.is_some());
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
    fn probe_messages_use_basename_not_home_path() {
        // A configured absolute lms path must not leak off-machine through the
        // served message (#1286): only the basename is embedded.
        let mut messages = Vec::new();
        let got = bounded_stdout(
            "/Users/someone/private/bin/lms-does-not-exist",
            &["ps", "--json"],
            "ps",
            SYS_PROBE_BOUND,
            &mut messages,
        );
        assert!(got.is_none());
        assert_eq!(messages.len(), 1);
        let m = &messages[0];
        assert_eq!(m.severity, Severity::Error, "a probe failure is error severity (#1821)");
        assert!(!m.text.contains("/Users/"), "no home path in the served message: {}", m.text);
        assert!(m.text.contains("lms-does-not-exist"), "basename still names the binary: {}", m.text);
        assert_eq!(bin_label("/Users/x/lms"), "lms");
        assert_eq!(bin_label("vm_stat"), "vm_stat");
    }


    #[test]
    fn memory_free_exactly_at_threshold_is_not_red() {
        // margin_percent == 15: the test is strict `<`, so equality is
        // NOT red.
        let mut inputs = base_inputs();
        inputs.margin_percent = Some(MARGIN_PERCENT_RED);
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
            WorkerProc { pid: 1, rss_bytes: JUDGE_POTENTIAL, footprint_bytes: None }, // cur == pot
            WorkerProc { pid: 2, rss_bytes: 10_000_000_000, footprint_bytes: None },
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
                WorkerProc { pid: 1, rss_bytes: 12_000_000_000, footprint_bytes: None },
                WorkerProc { pid: 2, rss_bytes: 8_000_000_000, footprint_bytes: None },
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
        // would fail here. `used_bytes` pinned to Σ current so `other_used`
        // is exactly 0 (#1821) — this property is about the ROUNDING
        // direction, not the other-tenants arithmetic.
        let mut inputs = base_inputs();
        inputs.pool = inputs.pool.map(|p| PoolSnapshot { used_bytes: Some(33_000_000_000), ..p });
        inputs.budget_bytes = Some(35_000_000_000);
        let ledger = compute_ledger(inputs, 1);
        assert_eq!(ledger.machine.state, LedgerState::Amber);
        let hint = ledger.machine.shrink_hint.as_deref().expect("amber names a shrink");
        let new_ctx = parse_hint_ctx(hint).expect("covering hint names a ctx");

        let mut applied = base_inputs();
        applied.pool = applied.pool.map(|p| PoolSnapshot { used_bytes: Some(33_000_000_000), ..p });
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

    // ── #1819 estimate-fallback tests ───────────────────────────────────

    /// Ties the fallback constant to the arithmetic it claims to derive
    /// from: `microsoft/phi-4`'s OWN published architecture (fetched from
    /// `huggingface.co/microsoft/phi-4/raw/main/config.json`, 2026-08-15 —
    /// `num_hidden_layers: 40, num_attention_heads: 40,
    /// num_key_value_heads: 10, hidden_size: 5120` → `head_dim = 5120/40 =
    /// 128`, a homogeneous dense decoder, no hybrid/sliding-window layers).
    /// If either drifts, this fails — the derivation can never go silently
    /// stale.
    #[test]
    fn fallback_kv_constant_matches_phi_4s_own_published_architecture() {
        let phi4_arch = ArchFacts {
            total_layers: 40,
            full_attention_layers: 40, // dense: every layer is full-attention
            kv_heads: 10,
            head_dim: 128,
            kv_bytes_per_element: 2, // fp16, the v1 default
        };
        assert_eq!(V1_FALLBACK_KV_BYTES_PER_CTX_TOKEN, phi4_arch.kv_per_token());
        assert_eq!(V1_FALLBACK_KV_BYTES_PER_CTX_TOKEN, 204_800);
    }

    /// Each tier must sit at or ABOVE the true KV rate of every modern GQA
    /// architecture that lands in it. These are the published configs the
    /// #1819 merge gate worked through — the same table in
    /// `FALLBACK_KV_TIERS`'s doc — asserted against the crate's own formula
    /// so a tier can never drift below the models it is supposed to cover.
    #[test]
    fn every_tier_covers_the_real_architectures_that_land_in_it() {
        // (name, size_bytes, layers, kv_heads, head_dim)
        let published: [(&str, u64, u32, u32, u32); 4] = [
            ("microsoft/phi-4", 9_053_136_497, 40, 10, 128),
            ("Qwen3-32B", 20 * 1024 * 1024 * 1024, 64, 8, 128),
            ("Llama-3.3-70B", 40 * 1024 * 1024 * 1024, 80, 8, 128),
            ("Mistral-Large-2-123B", 70 * 1024 * 1024 * 1024, 88, 8, 128),
        ];
        for (name, size, layers, kv_heads, head_dim) in published {
            let truth = ArchFacts {
                total_layers: layers,
                full_attention_layers: layers,
                kv_heads,
                head_dim,
                kv_bytes_per_element: 2,
            }
            .kv_per_token();
            let assumed = fallback_kv_rate_for_size(size);
            assert!(
                assumed >= truth,
                "{name}: fallback assumes {assumed} B/token but the real architecture needs {truth} — \
                 under-reserving is the one direction this estimate must never take",
            );
        }
    }

    /// The regression the #1819 merge gate caught: a 70B-class GGUF must NOT
    /// be priced at phi-4's rate. Before the tiers, this returned 204_800 —
    /// short by 122_880 B/token, which at 32K ctx is ~4 GB of reserve the
    /// machine silently did not have, while the cascade promised GREEN.
    #[test]
    fn a_large_dense_gguf_is_not_priced_at_a_small_models_kv_rate() {
        let seventy_b = 40 * 1024 * 1024 * 1024;
        assert_eq!(fallback_kv_rate_for_size(seventy_b), 327_680);
        assert!(fallback_kv_rate_for_size(seventy_b) > V1_FALLBACK_KV_BYTES_PER_CTX_TOKEN);
    }

    /// Total and monotonic: every size maps to a rate, and a bigger model
    /// never assumes a smaller one. The boundaries are inclusive-below.
    #[test]
    fn the_tier_table_is_total_and_monotonic() {
        let gib = 1024 * 1024 * 1024;
        assert_eq!(fallback_kv_rate_for_size(0), 204_800);
        assert_eq!(fallback_kv_rate_for_size(15 * gib), 204_800); // inclusive
        assert_eq!(fallback_kv_rate_for_size(15 * gib + 1), 327_680);
        assert_eq!(fallback_kv_rate_for_size(45 * gib), 327_680); // inclusive
        assert_eq!(fallback_kv_rate_for_size(45 * gib + 1), 409_600);
        assert_eq!(fallback_kv_rate_for_size(u64::MAX), 409_600);

        let mut prev = 0;
        for step in 0..200u64 {
            let rate = fallback_kv_rate_for_size(step * gib);
            assert!(rate >= prev, "rate fell from {prev} to {rate} at {step} GiB");
            prev = rate;
        }
    }

    /// A resident with a catalog size but NO arch facts (the live #1819
    /// trace: a GGUF download, no sidecar `config.json`) is priced by the
    /// fallback, not left `None` — and the row says so via
    /// `potential_source`. The fallback estimate carries the SAME
    /// `DEFAULT_TRANSIENT_MARGIN_BYTES` post-load margin an arch-priced row
    /// gets (#1819 review finding 1) — without it, an estimated row would
    /// be priced on a cheaper basis than a measured one, letting Green come
    /// easier for a guess than for a measurement.
    #[test]
    fn gguf_style_resident_falls_back_to_the_size_based_estimate() {
        let mut inputs = base_inputs();
        inputs
            .catalog
            .push(CatalogFact { model_key: "phi-4-gguf".into(), size_bytes: Some(9_053_136_497) });
        inputs.residents.push(resident("microsoft/phi-4", "phi-4-gguf", 8_192));
        // Deliberately NOT added to `inputs.arch` — this is the whole point.

        let ledger = compute_ledger(inputs, 1);
        let phi4 = ledger.models.iter().find(|m| m.model_key == "phi-4-gguf").unwrap();
        let expected = 9_053_136_497
            + V1_FALLBACK_KV_BYTES_PER_CTX_TOKEN * 8_192
            + darkmux_gestalt::DEFAULT_TRANSIENT_MARGIN_BYTES;
        assert_eq!(phi4.potential_bytes, Some(expected));
        assert_eq!(phi4.potential_source, Some(PotentialSource::Estimated));
        // The row's own arch-derived fields stay honestly `None` — only the
        // AGGREGATE potential was estimated, never a fabricated per-token
        // breakdown.
        assert!(phi4.kv_per_token_bytes.is_none());
        assert!(phi4.kv_bytes_at_ctx.is_none());
    }

    /// The end-to-end half of the tier fix: `compute_ledger` must PRICE a
    /// large unpriceable resident at its tier's rate, not merely be able to
    /// look that rate up. Mutating `estimate_with_source` back to the flat
    /// constant passed every tier test until this existed — the selector was
    /// covered, its USE was not.
    #[test]
    fn a_large_estimated_resident_is_priced_at_its_tier_rate_end_to_end() {
        let seventy_b: u64 = 40 * 1024 * 1024 * 1024; // lands in the ≤45 GiB tier
        let ctx: u64 = 32_768;
        let mut inputs = base_inputs();
        inputs
            .catalog
            .push(CatalogFact { model_key: "llama-70b-gguf".into(), size_bytes: Some(seventy_b) });
        inputs.residents.push(resident("meta/llama-70b", "llama-70b-gguf", ctx));

        let ledger = compute_ledger(inputs, 1);
        let row = ledger.models.iter().find(|m| m.model_key == "llama-70b-gguf").unwrap();
        let expected = seventy_b + 327_680 * ctx + darkmux_gestalt::DEFAULT_TRANSIENT_MARGIN_BYTES;
        assert_eq!(row.potential_bytes, Some(expected));
        // ...and materially MORE than the tier-1 rate would have reserved:
        // this difference is the ~4 GB the merge gate found missing.
        let flat = seventy_b + V1_FALLBACK_KV_BYTES_PER_CTX_TOKEN * ctx + darkmux_gestalt::DEFAULT_TRANSIENT_MARGIN_BYTES;
        assert!(row.potential_bytes.unwrap() > flat);
        assert_eq!(row.potential_bytes.unwrap() - flat, (327_680 - 204_800) * ctx);
    }

    /// The shrink path must charge an estimated row the rate it was PRICED
    /// at, including its tier. Reverting `effective_kv_rate` to the flat
    /// constant also passed everything until this existed — the earlier
    /// shrink test used a small model, where flat and tier-1 coincide.
    #[test]
    fn the_shrink_path_charges_a_large_estimated_row_its_own_tier_rate() {
        let seventy_b: u64 = 40 * 1024 * 1024 * 1024;
        let row = ModelRow {
            identifier: "meta/llama-70b".into(),
            model_key: "llama-70b-gguf".into(),
            owner: Owner::User,
            loaded_ctx: 32_768,
            weights_bytes: Some(seventy_b),
            kv_per_token_bytes: None, // estimated rows carry no per-token fact
            kv_bytes_at_ctx: None,
            potential_bytes: Some(1),
            current_bytes: None,
            state: LedgerState::Unknown,
            potential_source: Some(PotentialSource::Estimated),
            over_price_bytes: None,
            shrink_hint: None,
        };
        assert_eq!(effective_kv_rate(&row), 327_680);
        assert_ne!(effective_kv_rate(&row), V1_FALLBACK_KV_BYTES_PER_CTX_TOKEN);
    }

    /// #1819 review finding 1's own invariant, pinned directly: an
    /// ESTIMATED row and an ARCH-priced row with IDENTICAL weights/ctx and
    /// an identical kv rate must price identically up to that rate — i.e.
    /// the two estimators sit on the same accounting basis (weights + kv +
    /// the SAME transient margin), never a cheaper one for the guess.
    #[test]
    fn an_estimated_row_and_an_arch_priced_row_price_the_same_weights_and_ctx_identically_up_to_the_kv_rate(
    ) {
        let ctx = 8_192u64;
        let size = 9_053_136_497u64;
        // An arch-priced row whose kv_per_token happens to equal the
        // fallback's own rate — so if the two estimators share a basis,
        // their `potential_bytes` must be byte-for-byte identical.
        let matching_arch = ArchFacts {
            total_layers: 1,
            full_attention_layers: 1,
            kv_heads: 1,
            head_dim: (V1_FALLBACK_KV_BYTES_PER_CTX_TOKEN / (2 * 2)) as u32, // 2×heads×dim×elem = rate
            kv_bytes_per_element: 2,
        };
        assert_eq!(matching_arch.kv_per_token(), V1_FALLBACK_KV_BYTES_PER_CTX_TOKEN, "fixture sanity");

        let mut arch_inputs = base_inputs();
        arch_inputs.residents = vec![resident("a", "same-key", ctx)];
        arch_inputs.catalog = vec![CatalogFact { model_key: "same-key".into(), size_bytes: Some(size) }];
        arch_inputs.arch = BTreeMap::from([("same-key".to_string(), matching_arch)]);
        let arch_ledger = compute_ledger(arch_inputs, 1);
        let arch_row = &arch_ledger.models[0];
        assert_eq!(arch_row.potential_source, Some(PotentialSource::Arch));

        let mut estimated_inputs = base_inputs();
        estimated_inputs.residents = vec![resident("b", "same-key", ctx)];
        estimated_inputs.catalog = vec![CatalogFact { model_key: "same-key".into(), size_bytes: Some(size) }];
        // No arch entry ⇒ falls to the estimate.
        let estimated_ledger = compute_ledger(estimated_inputs, 1);
        let estimated_row = &estimated_ledger.models[0];
        assert_eq!(estimated_row.potential_source, Some(PotentialSource::Estimated));

        assert_eq!(
            arch_row.potential_bytes, estimated_row.potential_bytes,
            "same weights/ctx/kv-rate must price identically regardless of which estimator answered"
        );
    }

    /// The two normally-priced rows in `base_inputs()` both came from real
    /// arch facts — `potential_source` says `Arch`, not `Estimated`.
    #[test]
    fn arch_priced_rows_report_the_arch_source() {
        let ledger = compute_ledger(base_inputs(), 1);
        for m in &ledger.models {
            assert_eq!(
                m.potential_source,
                Some(PotentialSource::Arch),
                "{} should be arch-priced, not estimated",
                m.model_key
            );
        }
        // Pin the WIRE string too (not just the Rust enum) — the UI's
        // `MachineResourcesModel.potential_source` union names `"arch"`
        // literally, and a variant rename here would slip past an
        // enum-only assertion.
        let v = serde_json::to_value(&ledger).expect("serializes");
        assert_eq!(v["models"][0]["potential_source"], "arch");
    }

    /// The estimate message must state the ACTUAL rate, and this pins the
    /// literal text because nothing did — which is why it shipped wrong.
    ///
    /// The argument is a pre-formatted `String` (it may be a range when
    /// several tiers apply). `{:.1}` on a `String` is a max-width truncation,
    /// not decimal precision, so "204.8" rendered as "2" — a 100x understated
    /// disclosure in the one message that exists to disclose the assumption.
    #[test]
    fn the_estimate_message_states_the_whole_rate_not_a_truncated_one() {
        let mut inputs = base_inputs();
        inputs
            .catalog
            .push(CatalogFact { model_key: "phi-4-gguf".into(), size_bytes: Some(9_053_136_497) });
        inputs.residents.push(resident("microsoft/phi-4", "phi-4-gguf", 8_192));

        let ledger = compute_ledger(inputs, 1);
        let msg = ledger
            .messages
            .iter()
            .find(|m| m.text.contains("priced by ESTIMATE"))
            .expect("estimate message");

        assert!(msg.text.contains("204.8 KB/token"), "rate was mangled: {}", msg.text);
        // The exact shape of the bug: a single leading digit where the figure
        // should be.
        assert!(!msg.text.contains("a size-tiered 2 KB/token"), "truncated to one character: {}", msg.text);
    }

    /// #1819 decision 2: `estimated_models` counts separately from
    /// `unpriced_models` — an estimated row IS priced (it contributes to
    /// `sum_potential`), so it must never inflate the undercount counter.
    /// A dedicated message names the estimated resident(s) too — `info`
    /// severity (#1821): the estimate is a disclosure, not a degradation.
    #[test]
    fn estimated_models_counted_separately_from_unpriced_with_its_own_message() {
        let mut inputs = base_inputs();
        inputs
            .catalog
            .push(CatalogFact { model_key: "phi-4-gguf".into(), size_bytes: Some(9_053_136_497) });
        inputs.residents.push(resident("microsoft/phi-4", "phi-4-gguf", 8_192));

        let ledger = compute_ledger(inputs, 1);
        assert_eq!(ledger.machine.estimated_models, 1);
        assert_eq!(ledger.machine.unpriced_models, 0);
        assert!(
            ledger
                .messages
                .iter()
                .any(|m| m.severity == Severity::Info && m.text.contains("ESTIMATE") && m.text.contains("phi-4-gguf")),
            "a dedicated info-severity estimate message names the resident: {:?}",
            ledger.messages
        );
        // The undercount message (a DIFFERENT fact, `warn` severity) must
        // not fire for an estimated resident — it has a potential, just not
        // a measured one.
        assert!(
            !ledger.messages.iter().any(|m| m.text.contains("undercount") && m.text.contains("phi-4-gguf")),
            "an estimated resident is not an undercounted one: {:?}",
            ledger.messages
        );
    }

    /// #1819 decision 1: an estimated resident MAY produce a decided GREEN
    /// verdict — the whole point of the fallback. Per-model tint for the
    /// estimated row follows the ordinary machine-state rule (it is NOT
    /// forced to `Unknown` the way a genuinely unpriceable row is).
    #[test]
    fn green_is_reachable_with_an_estimated_resident_present() {
        let mut inputs = base_inputs();
        inputs
            .catalog
            .push(CatalogFact { model_key: "phi-4-gguf".into(), size_bytes: Some(9_053_136_497) });
        inputs.residents.push(resident("microsoft/phi-4", "phi-4-gguf", 8_192));
        // No budget_bytes set: falls back to the 128 GiB physical pool,
        // comfortably fitting judge + devstral + the phi-4 estimate (~48 GB
        // total).

        let ledger = compute_ledger(inputs, 1);
        assert_eq!(ledger.machine.state, LedgerState::Green);
        assert_eq!(ledger.machine.estimated_models, 1);
        assert_eq!(ledger.machine.unpriced_models, 0);
        let phi4 = ledger.models.iter().find(|m| m.model_key == "phi-4-gguf").unwrap();
        assert_eq!(phi4.state, LedgerState::Green, "an estimated row follows the machine verdict like any priced row");
    }

    /// #1819 review finding 5: an amber machine whose ONLY resident is an
    /// ESTIMATED one, with real shrinkable ctx, must NOT render the false
    /// "no shrinkable context" line — `effective_kv_rate()` has to let the
    /// shrink-hint search charge the estimated row its own fallback rate
    /// rather than treating it as `kv_per_token_bytes == 0` (un-shrinkable).
    #[test]
    fn amber_shrink_hint_can_target_an_estimated_row_not_just_arch_priced_ones() {
        let inputs = LedgerInputs {
            residents: vec![resident("microsoft/phi-4", "phi-4-gguf", 131_072)],
            catalog: vec![CatalogFact { model_key: "phi-4-gguf".into(), size_bytes: Some(9_053_136_497) }],
            // No arch entry ⇒ estimated. `used_bytes: Some(0)` keeps
            // `other_used` (and so `projected_total`) at exactly
            // `sum_potential` — this test is about the shrink-hint TARGET,
            // not the #1821 other-tenants arithmetic, and a `None` here
            // would land the machine at Unknown instead of the Amber this
            // test needs.
            pool: Some(PoolSnapshot {
                capacity_bytes: 137_438_953_472,
                used_bytes: Some(0),
                available_bytes: Some(1),
                free_bytes: Some(1),
            }),
            budget_bytes: Some(34_646_682_097), // potential − 2 GB ⇒ amber
            workers: Some(Vec::new()),
            ..Default::default()
        };
        let ledger = compute_ledger(inputs, 1);
        assert_eq!(ledger.machine.state, LedgerState::Amber);
        assert_eq!(ledger.machine.estimated_models, 1);
        let hint = ledger.machine.shrink_hint.as_deref().expect("amber names a shrink");
        assert!(
            !hint.to_lowercase().contains("no shrinkable context"),
            "an estimated row's ctx IS shrinkable — the false claim the review caught: {hint}"
        );
        assert!(hint.contains("phi-4-gguf") && hint.contains("reload"), "names the estimated row as the target: {hint}");
        // The saving figure is itself flagged as computed from the estimate.
        assert!(
            hint.to_lowercase().contains("estimated"),
            "the shrink hint discloses that ITS OWN saving figure rests on the same estimate: {hint}"
        );
    }

    /// The inverted case of the test above: a GENUINELY unpriceable
    /// resident (no catalog size either) must still force `Unknown`, even
    /// when an estimated resident is ALSO present and would otherwise have
    /// let the machine land Green. `unpriced_models == 0` is the only gate
    /// on Green — an estimate never substitutes for it.
    #[test]
    fn unpriceable_resident_still_blocks_green_even_alongside_an_estimated_one() {
        let mut inputs = base_inputs();
        inputs
            .catalog
            .push(CatalogFact { model_key: "phi-4-gguf".into(), size_bytes: Some(9_053_136_497) });
        inputs.residents.push(resident("microsoft/phi-4", "phi-4-gguf", 8_192));
        // Genuinely unpriceable: no catalog entry AND no arch entry.
        inputs.residents.push(resident("mystery", "mystery-model", 8_192));

        let ledger = compute_ledger(inputs, 1);
        assert_eq!(ledger.machine.estimated_models, 1);
        assert_eq!(ledger.machine.unpriced_models, 1);
        assert_eq!(
            ledger.machine.state,
            LedgerState::Unknown,
            "a genuinely unpriceable resident blocks Green regardless of any estimated one"
        );
        let mystery = ledger.models.iter().find(|m| m.model_key == "mystery-model").unwrap();
        assert_eq!(mystery.state, LedgerState::Unknown);
        assert!(mystery.potential_source.is_none());
    }

    /// Provenance travels through both surfaces `render_human` composes:
    /// the row's STATE column and the machine-total parenthetical.
    #[test]
    fn render_human_discloses_the_estimated_count_on_the_row_and_the_machine_line() {
        let mut inputs = base_inputs();
        inputs
            .catalog
            .push(CatalogFact { model_key: "phi-4-gguf".into(), size_bytes: Some(9_053_136_497) });
        inputs.residents.push(resident("microsoft/phi-4", "phi-4-gguf", 8_192));
        let ledger = compute_ledger(inputs, 1);
        let text = render_human(&ledger);
        assert!(text.contains("(estimated)"), "row STATE column discloses the estimate: {text}");
        assert!(
            text.contains("1 estimated"),
            "machine-total line names the estimated count: {text}"
        );
        // #1819 review nitpick: the CAVEAT travels WITH the potential figure
        // itself (a `~` prefix), not only in a separate column — a reader
        // copy-pasting just that cell still carries the qualifier.
        let phi4 = ledger.models.iter().find(|m| m.model_key == "phi-4-gguf").unwrap();
        let expected_potential = fmt_bytes(phi4.potential_bytes.unwrap());
        assert!(
            text.contains(&format!("~{expected_potential}")),
            "the estimated row's POTENTIAL cell carries a `~` prefix: {text}"
        );
    }

    /// `potential_source` is OMITTED from the JSON entirely for a row with
    /// no potential at all (never a spurious `null`), and present verbatim
    /// as `"estimated"` for the fallback-priced row — the wire contract
    /// `MachineResourcesModel` (the UI's TypeScript type) depends on.
    #[test]
    fn json_potential_source_is_present_for_estimated_and_absent_for_unpriceable() {
        let mut inputs = base_inputs();
        inputs
            .catalog
            .push(CatalogFact { model_key: "phi-4-gguf".into(), size_bytes: Some(9_053_136_497) });
        inputs.residents.push(resident("microsoft/phi-4", "phi-4-gguf", 8_192));
        inputs.residents.push(resident("mystery", "mystery-model", 8_192));
        let ledger = compute_ledger(inputs, 1);
        let v = serde_json::to_value(&ledger).expect("serializes");
        let models = v["models"].as_array().unwrap();
        let phi4 = models.iter().find(|m| m["model_key"] == "phi-4-gguf").unwrap();
        assert_eq!(phi4["potential_source"], "estimated");
        let mystery = models.iter().find(|m| m["model_key"] == "mystery-model").unwrap();
        assert!(
            mystery.get("potential_source").is_none(),
            "no potential at all ⇒ the field is OMITTED, not null: {mystery}"
        );
        // A LITERAL, deliberately — the sibling assertion elsewhere compares
        // against the constant and so can never fail. This one is the pin
        // that makes a schema bump a conscious edit; #1854 took it 2.0 → 2.1
        //.
        assert_eq!(v["schema_version"], "2.1");
        assert_eq!(v["machine"]["estimated_models"], 1);
    }

    /// #1819 review finding 3: a 1.1 binary must tolerate a 1.0 peer's
    /// ledger (a real cross-fleet path — `darkmux machine resources
    /// <machine>` deserializes a remote payload, `src/main.rs::
    /// cmd_machine_resources`) that predates `estimated_models` entirely.
    /// Without `#[serde(default)]` this is a hard deserialization error
    /// (unlike `Option<T>` fields, which serde already treats as
    /// implicitly optional-on-read — `potential_source` needs no such
    /// annotation, only this newly-added non-`Option` field does).
    #[test]
    fn estimated_models_defaults_to_zero_on_a_pre_1819_payload_missing_the_field() {
        // A hand-built 1.0-shaped `machine` object — literally what a
        // pre-#1819 daemon would have emitted, with no `estimated_models`
        // key at all.
        let pre_1819_machine = serde_json::json!({
            "potential_bytes": 24565385183u64,
            "unpriced_models": 0,
            "current_bytes": 19506757632u64,
            "state": "green"
        });
        let totals: MachineTotals =
            serde_json::from_value(pre_1819_machine).expect("a 1.0 payload missing estimated_models must still parse");
        assert_eq!(totals.estimated_models, 0, "absent field defaults to zero, never an error");
    }

}
