//! What `darkmux dispatch --json` prints on stdout: one envelope.
//!
//! From this release the envelope is a semver contract, like every `--json`
//! output. There are two shapes, by where the role execution ran:
//!
//! - [`DispatchEnvelope`]: a role execution that ran in the container. The
//!   runtime prints [`RuntimeEnvelope`] (its result, its last message, where its
//!   trajectory is) and the host adds what only the host sees: the counts folded
//!   from the trajectory, the detector firings, the resolved bounds, host
//!   pressure and the rest.
//! - [`DirectDispatchEnvelope`]: a role execution staffed by a hosted endpoint.
//!   It is one call with no container and no trajectory, so it has no host
//!   blocks and its `metrics` name the endpoint.
//!
//! Three fields carry a JSON value rather than a named type, because the same
//! value is a flow-record payload today and the flow payloads are typed
//! separately: `detections`, `bounds` and `host_window`. Each is documented
//! below with the flow record it shares.

use darkmux_trajectory::{RuntimeEnvelope, TokenSum, TrajectoryFold, UsageCounts};
use schemars::JsonSchema;
use serde::Serialize;

use crate::host_probe::{HostExtras, MwStats, PowerWindow, ThermalWindow};
use crate::telemetry_sampler::{HostStats, MetricStats};

/// The envelope of a role execution that ran in the container.
///
/// The runtime's keys come first, then the host's, in the order below. A block
/// that was not measured is absent, never zeroed: an absent `host` says "the
/// sampler never ran", a zeroed one would read as "measured, and idle".
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct DispatchEnvelope {
    #[serde(flatten)]
    pub runtime: RuntimeEnvelope,
    /// This execution's counts, folded from its trajectory.
    pub metrics: EnvelopeMetrics,
    /// Every detector firing, verbatim as the `dispatch.telemetry` flow record
    /// carries it. Always present: an empty list is a positive statement that
    /// nothing fired, and an absent field would be ambiguous.
    pub detections: Vec<serde_json::Value>,
    /// The resolved request bounds and where each came from: the same value the
    /// `dispatch.start` flow record carries.
    pub bounds: serde_json::Value,
    /// Host pressure over the execution.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub host: Option<EnvelopeHost>,
    /// The flatter host-pressure summary the `dispatch.complete` flow record
    /// carries.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub host_window: Option<serde_json::Value>,
    /// What a crawl recorded; absent when the findings channel was never used.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub findings: Option<FindingsSummary>,
    /// Degeneracy findings surfaced as warnings under the `warn` policy;
    /// absent when there were none.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub degeneracy_warnings: Option<u32>,
    /// The reasoning checkpoints, reduced; absent when none ran.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub checkpoints: Option<CheckpointSummary>,
}

/// One container execution's counts, under the names `--json` callers, the lab
/// benches and the crawl read. `wall_ms` is the runtime's own clock (the span
/// its events cover, for a killed run).
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct EnvelopeMetrics {
    pub model: String,
    pub wall_ms: Option<u64>,
    pub turns: u32,
    pub compactions: u32,
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    pub total_tokens: u64,
    pub reasoning_tokens: Option<u64>,
    pub cached_tokens: Option<u64>,
    pub rest_ms: u64,
    pub rests: u32,
    pub turn_delay_effective_ms: Option<u64>,
}

impl EnvelopeMetrics {
    pub fn of(fold: &TrajectoryFold, model: &str) -> Self {
        let TokenSum { prompt, completion, total, reasoning, cached } = fold.tokens;
        EnvelopeMetrics {
            model: model.to_string(),
            wall_ms: fold.wall_ms(),
            turns: fold.turns(),
            compactions: fold.compactions(),
            prompt_tokens: prompt,
            completion_tokens: completion,
            total_tokens: total,
            reasoning_tokens: reasoning,
            cached_tokens: cached,
            rest_ms: fold.rest_ms(),
            rests: fold.rest_count(),
            turn_delay_effective_ms: fold.complete.as_ref().and_then(|c| c.turn_delay_effective_ms),
        }
    }
}

/// Host pressure over the execution, reduced. `cpu`, `mem` and `gpu` each carry
/// the full peak/mean/p95/duty reduction: a peak alone answers "did this ever
/// spike" but not how hard the host was driven on average. `power`, `thermal`
/// and `energy_mwh` are present only when the host probe could read that
/// source.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct EnvelopeHost {
    pub cpu: MetricStats,
    pub mem: MetricStats,
    pub gpu: MetricStats,
    pub samples: u32,
    pub sample_interval_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub power: Option<EnvelopePower>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thermal: Option<EnvelopeThermal>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub energy_mwh: Option<f64>,
}

impl EnvelopeHost {
    /// `None` when the sampler never took a sample.
    pub fn of(stats: &HostStats, extras: &HostExtras) -> Option<Self> {
        if stats.samples == 0 {
            return None;
        }
        Some(EnvelopeHost {
            cpu: stats.cpu,
            mem: stats.mem,
            gpu: stats.gpu,
            samples: stats.samples,
            sample_interval_ms: stats.sample_interval_ms,
            power: extras.power.as_ref().map(EnvelopePower::from),
            thermal: extras.thermal.as_ref().map(EnvelopeThermal::from),
            energy_mwh: extras.energy_mwh,
        })
    }
}

/// The three power rails.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct EnvelopePower {
    pub cpu: PowerRail,
    pub gpu: PowerRail,
    pub total: PowerRail,
}

/// One power rail over the window, in milliwatts. In this envelope a number's
/// unit is part of its name.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct PowerRail {
    pub mean_mw: Option<f64>,
    pub peak_mw: Option<u64>,
}

impl From<&MwStats> for PowerRail {
    fn from(m: &MwStats) -> Self {
        PowerRail { mean_mw: m.mean_mw, peak_mw: m.max_mw }
    }
}

impl From<&PowerWindow> for EnvelopePower {
    fn from(p: &PowerWindow) -> Self {
        EnvelopePower { cpu: (&p.cpu).into(), gpu: (&p.gpu).into(), total: (&p.total).into() }
    }
}

/// The window's thermal summary.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct EnvelopeThermal {
    /// The most severe state observed in the window.
    pub worst_state: String,
    /// Wall-clock spent in a state other than `nominal`.
    pub above_nominal_ms: u64,
    /// The lowest CPU speed cap seen; 100 means the kernel never capped.
    pub min_cpu_speed_limit_pct: u64,
}

impl From<&ThermalWindow> for EnvelopeThermal {
    fn from(t: &ThermalWindow) -> Self {
        EnvelopeThermal {
            worst_state: t.worst_state.clone(),
            above_nominal_ms: t.above_nominal_ms,
            min_cpu_speed_limit_pct: t.min_cpu_speed_limit_pct,
        }
    }
}

/// How many findings a crawl recorded and where they are: a count plus a path
/// rather than the findings themselves, because the envelope is a decision
/// surface. The path is host-shaped, one the caller can open.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct FindingsSummary {
    pub count: usize,
    pub path: String,
}

/// The reasoning checkpoints, reduced: the caller's next action turns on "did
/// it converge", not on each ruling.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct CheckpointSummary {
    pub total: usize,
    pub concluded: u32,
    pub min_tail_ratio: Option<f64>,
    pub mean_tail_ratio: Option<f64>,
}

impl CheckpointSummary {
    /// `None` when no checkpoint ran.
    pub fn of(fold: &TrajectoryFold) -> Option<Self> {
        if fold.checkpoints.is_empty() {
            return None;
        }
        let (min_tail_ratio, mean_tail_ratio) = fold.checkpoint_tail_ratios();
        Some(CheckpointSummary {
            total: fold.checkpoints.len(),
            concluded: fold.checkpoints_concluded(),
            min_tail_ratio,
            mean_tail_ratio,
        })
    }
}

/// The envelope of a role execution staffed by a hosted endpoint: one call, no
/// container, so no trajectory and no host blocks.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct DirectDispatchEnvelope {
    /// Always `stop`: a call that failed is an error, not an envelope.
    pub result: String,
    pub final_assistant: String,
    pub metrics: DirectMetrics,
}

/// The one call's counts.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct DirectMetrics {
    pub model: String,
    /// The endpoint label the call went to.
    pub endpoint: String,
    pub wall_ms: u64,
    /// Always 1: one call is one turn.
    pub turns: u32,
    #[serde(flatten)]
    pub tokens: DirectTokens,
}

/// The call's own token counts, the ones its usage record carries. An
/// unreported count is `null`, never a fabricated 0, and a reply with no usage
/// block reports nothing, details included. The key set is the parity contract
/// both hosted single-shot producers are held to (`DIRECT_TOKEN_KEYS`).
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct DirectTokens {
    pub prompt_tokens: Option<u64>,
    pub completion_tokens: Option<u64>,
    pub total_tokens: Option<u64>,
    pub reasoning_tokens: Option<u64>,
    pub cached_tokens: Option<u64>,
}

impl DirectTokens {
    pub fn of(counts: &UsageCounts) -> Self {
        if !counts.reported() {
            return DirectTokens {
                prompt_tokens: None,
                completion_tokens: None,
                total_tokens: None,
                reasoning_tokens: None,
                cached_tokens: None,
            };
        }
        DirectTokens {
            prompt_tokens: counts.prompt,
            completion_tokens: counts.completion,
            total_tokens: counts.total_tokens(),
            reasoning_tokens: counts.reasoning,
            cached_tokens: counts.cached,
        }
    }
}
