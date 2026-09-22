//! (#2855) Derived run metrics — computed once, in the lab, with the
//! reconciliation checks that say whether each number may be quoted.
//!
//! A run's artifacts already carry everything: `metrics.json` has the raw
//! counters, `trajectory.jsonl` has per-turn events, and the flow stream has
//! host telemetry. What did not exist was the DERIVATION. Every analysis
//! wrote its own, outside the lab, and every one of them got some of it
//! wrong — not by arithmetic slips, but because the naive reading of these
//! artifacts is wrong in ways nothing in the data warns you about.
//!
//! The traps this module exists to close, each one a number that shipped:
//!
//! 1. **There are TWO gates.** `dispatch.gate.observation` (+ `.abort`) is
//!    the STREAMING gate — it judges mid-call and can end the call
//!    client-side. `dispatch.checkpoint` is the later per-call-cap gate.
//!    Reading one and not the other reported zero degeneracy on runs that
//!    were demonstrably cut. They are separate fields here
//!    ([`StreamGate`] / [`CheckpointGate`]) so a consumer cannot conflate
//!    them by accident.
//! 2. **Usage frames ACCUMULATE per seq.** One turn emits several
//!    `model.completed` frames, because every checkpoint continuation
//!    reports its own. Assigning instead of summing undercounted one run 15x
//!    (4,553 against 68,553) and turned 173 tok/s into 11.5.
//! 3. **A cut stream is never billed.** `usage.completion_tokens` is NULL
//!    when the gate ended the call, so those tokens are absent while their
//!    SECONDS are present. Throughput therefore runs over billed seconds
//!    against billed tokens, and [`RunStats::billed_gen_fraction`] says what
//!    sample size the rate rests on.
//! 4. **`reasoning_chars` is not output chars.** `model.reasoning` is the
//!    thinking channel; `model.partial.cumulative_chars` is content. They
//!    differ by more than 10x, so they are counted separately and neither is
//!    ever called "output".
//! 5. **Rest is the thermal governor, not harness overhead.** It is charged
//!    PER TURN, so an engine taking more turns pays more rest at identical
//!    thermals — which is why `wall_ms` is the wrong cross-engine number and
//!    `active_ms` is reported beside it (#2848). `ratchet_factor` doubles
//!    the delay after a serious episode, so the DISTINCT delays are reported:
//!    anything but one value means the ratchet fired.
//! 6. **A tail ratio needs six decimals.** 0.2499837 rounds to `0.25` at 4dp
//!    and then reads as sitting ON the threshold; the comparison is a
//!    strict `<`, so that digit is the whole verdict.
//! 7. **Power must be busy-only, with a duty cycle.** Averaging across idle
//!    understates load by whatever fraction of the run was tool calls and
//!    rest. Energy per token uses the same billed subset the tokens came
//!    from; mixing all-generation seconds with billed-only tokens inflated
//!    one figure 6.9x.
//! 8. **Flow files are named by UTC date.** Every file is read, never
//!    "today's".
//!
//! **Nothing here needs a producer change.** The capture is complete; this
//! is the reading. That also means it applies retroactively to runs already
//! on disk, and a fix to a derivation fixes every past run at once — which
//! is the argument for deriving on demand rather than freezing a computed
//! number into the artifact at run time.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

/// Data-shape semver for [`RunStats`], per the repo's additive-minor rule.
pub const RUN_STATS_SCHEMA_VERSION: &str = "1.0.0";

/// Uniqueness ratio below which a slice is degenerate.
///
/// **Mirrors `runtime/src/reasoning_loop.rs`'s `DEGENERATE_TAIL_RATIO`**, and
/// cannot import it — the runtime crate is not a workspace member. A copied
/// constant drifts silently, so it is never trusted on its own: the
/// [`RunChecks::verdict_matches_ratio`] check re-derives the degenerate set
/// from this threshold and compares it against the runtime's OWN recorded
/// judgment. False means this constant no longer matches the build that
/// produced the run, and the derived set is the one to distrust.
pub const DEGENERATE_TAIL_RATIO: f64 = 0.25;

/// GPU utilization at or above which a telemetry sample counts as BUSY.
///
/// Measured separation on this hardware is wide and clean: busy samples read
/// 96-97% at 28-46 W against idle at 0-2% and 0-43 mW. Anything in the middle
/// would be an artifact of a sample landing on a boundary, not a real state.
pub const BUSY_GPU_PCT: u64 = 20;

/// Decimal places a renderer must keep when printing a tail ratio.
///
/// Four is not enough: 0.2499837 rounds to `0.25` at 4dp and then reads as
/// sitting ON [`DEGENERATE_TAIL_RATIO`] rather than under it, while the
/// comparison the runtime made is a strict `<`. The ratios themselves are
/// carried here at full measured precision — rounding is a display decision,
/// and a consumer can round but cannot unround.
pub const TAIL_RATIO_DISPLAY_DP: usize = 6;

/// Plausible band for reasoning chars per reasoning token. Outside it, one of
/// the two counters is measuring something other than what its name says, and
/// the turn is listed rather than silently averaged in.
pub const PLAUSIBLE_CHARS_PER_TOKEN: (f64, f64) = (2.0, 8.0);

/// Event names a run that produced any turns must carry. Their absence means
/// this module is reading a stream that does not use its vocabulary — a typo
/// here, not a quiet zero in the data.
const REQUIRED_EVENTS: [&str; 3] = [
    "model.streaming.start",
    "model.streaming.end",
    "model.completed",
];

fn round(v: f64, dp: i32) -> f64 {
    let f = 10f64.powi(dp);
    (v * f).round() / f
}

fn mean(xs: impl Iterator<Item = f64>) -> Option<f64> {
    let (sum, n) = xs.fold((0.0, 0usize), |(s, n), x| (s + x, n + 1));
    if n == 0 { None } else { Some(sum / n as f64) }
}

// ---------------------------------------------------------------------------
// Output shape
// ---------------------------------------------------------------------------

/// The STREAMING gate (`dispatch.gate.observation` / `dispatch.gate.abort`) —
/// it samples output mid-call and can end the call client-side.
///
/// `degenerate_turns` comes from each record's OWN `degenerate` flag, which
/// is the gate's judgment and is recorded regardless of whether the policy
/// let it act. Under `observe` a degenerate finding therefore appears here
/// with `aborts: 0`, which is exactly the shape that proves the policy was
/// in effect.
#[derive(Debug, Clone, Default, Serialize)]
pub struct StreamGate {
    /// How many times output was sampled. A zero degenerate count against a
    /// large observation count is a far stronger statement than an absence
    /// of records, which is what a one-gate reader reports instead.
    pub observations: usize,
    pub degenerate_turns: Vec<u64>,
    pub aborts: usize,
    /// At full measured precision — see [`TAIL_RATIO_DISPLAY_DP`].
    pub min_tail_ratio: Option<f64>,
}

/// The per-call-cap gate (`dispatch.checkpoint`) — it judges the accumulated
/// slice when a call hits `max_tokens_per_call`.
#[derive(Debug, Clone, Default, Serialize)]
pub struct CheckpointGate {
    pub observations: usize,
    /// Turns the runtime JUDGED degenerate (`would_conclude`), whatever the
    /// policy then did about it.
    pub degenerate_turns: Vec<u64>,
    /// Turns the runtime actually CUT (`verdict: conclude`). Under `observe`
    /// this is empty while `degenerate_turns` is not; that difference is the
    /// whole point of the policy and must not be collapsed.
    pub concluded_turns: Vec<u64>,
    /// Re-derived from [`DEGENERATE_TAIL_RATIO`], for the cross-check only.
    pub degenerate_turns_by_ratio: Vec<u64>,
    pub min_tail_ratio: Option<f64>,
    /// The resolved detection policy the records themselves report.
    pub policy: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct Gates {
    pub stream: StreamGate,
    pub checkpoint: CheckpointGate,
}

#[derive(Debug, Clone, Serialize)]
pub struct SuspectTurn {
    pub seq: u64,
    pub reasoning_chars_per_token: f64,
}

/// Every check that has to hold before a number beside it may be quoted.
///
/// **A metric that will not reconcile is not reported as fact.** Each of
/// these would have caught a specific wrong claim made before this module
/// existed; [`RunStats::unreconciled`] turns the failures into the caveats a
/// renderer prints next to the figures.
#[derive(Debug, Clone, Default, Serialize)]
pub struct RunChecks {
    /// Per-turn tokens sum to the run total. This is what caught the 15x
    /// undercount; it fails the moment frames are assigned instead of summed.
    pub tokens_reconcile: Option<bool>,
    pub metrics_totals_zeroed: bool,
    pub rest_matches_trajectory: Option<bool>,
    pub turns_match_trajectory: Option<bool>,
    pub telemetry_covers_run: bool,
    /// Rest cannot exceed wall. A violation means one of the two counters is
    /// measuring a different window than the other.
    pub rest_within_wall: bool,
    /// Host telemetry landed inside the run window at all. False = there is
    /// no power arm for this run, not that it drew no power.
    pub have_telemetry_samples: bool,
    pub have_flow_records: bool,
    /// Event names keyed on here that the trajectory never uses. Non-empty
    /// means this module is reading a vocabulary the producer does not speak.
    pub missing_required_events: Vec<String>,
    pub checkpoint_events_seen: bool,
    /// False = checkpoint records are demonstrably in the trajectory and the
    /// parse counted none. The contradiction gets its own boolean rather
    /// than being left for a reader to spot across two fields; a mutation
    /// that repoints the parse at a wrong name flips exactly this.
    pub checkpoint_parse_consistent: bool,
    /// The runtime's own `would_conclude` verdicts and the threshold
    /// comparison name the same turns. False = [`DEGENERATE_TAIL_RATIO`] here
    /// does not match the build that produced the run.
    pub verdict_matches_ratio: bool,
    /// Every stream returned a usage frame. False = `tok_per_s` is a rate
    /// over the BILLED subset only, and must be quoted with
    /// [`RunStats::billed_gen_fraction`].
    pub all_streams_billed: bool,
    /// Completion frames and streams arrived 1:1. Their pairing is what
    /// identifies which SPAN was unbilled; when it fails, unbilled spans are
    /// identified from gate-abort records alone and the timing split is
    /// weaker.
    pub frames_match_streams: bool,
    /// Every stream that started also ended. An unterminated span is a run
    /// that died mid-call; its generation seconds are unknown, not zero.
    pub streams_terminated: bool,
}

/// Derived metrics for one lab run.
///
/// Durations are milliseconds because that is the unit the artifacts use;
/// converting to seconds is the renderer's job, and doing it here would
/// round away differences the comparison depends on.
#[derive(Debug, Clone, Default, Serialize)]
pub struct RunStats {
    pub schema_version: &'static str,
    pub run: String,
    pub model: Option<String>,
    pub result: Option<String>,
    /// The fixture's verify outcome, from the run manifest. The denominator
    /// for any cost-per-successful-outcome figure, which reversed a headline
    /// once: the faster engine cost 1.85x the GPU seconds per FINISHED run,
    /// because a failed run consumes the GPU too.
    pub verify: Option<String>,
    pub ok: Option<bool>,

    // --- time -------------------------------------------------------------
    pub wall_ms: u64,
    pub rest_ms: u64,
    /// `wall_ms - rest_ms`. The cross-engine time figure; wall carries a
    /// thermal penalty proportional to turn count (#2848).
    pub active_ms: u64,
    pub rest_events: usize,
    /// DISTINCT rest delays. Anything other than a single value means the
    /// thermal ratchet fired mid-run, which would otherwise read as a
    /// within-block slowdown.
    pub rest_delays_ms: Vec<u64>,
    pub rest_reasons: Vec<String>,
    pub rest_states: Vec<String>,
    pub thermal_ratchet_fired: bool,
    pub rest_ms_per_turn: Option<f64>,

    // --- work -------------------------------------------------------------
    pub turns: u64,
    pub compactions: u64,
    pub reasoning_chars: u64,
    pub content_chars: u64,
    pub completion_tokens: u64,
    pub reasoning_tokens: u64,
    pub suspect_turns: Vec<SuspectTurn>,
    pub tool_calls: BTreeMap<String, usize>,
    pub tool_calls_total: usize,
    pub tool_calls_failed: usize,

    // --- throughput -------------------------------------------------------
    pub streams: usize,
    pub streams_unbilled: usize,
    pub streams_unterminated: usize,
    pub turns_with_stream_abort: Vec<u64>,
    pub gen_ms_billed: u64,
    pub gen_ms_all: u64,
    pub unbilled_gen_ms: u64,
    /// What fraction of generation time `tok_per_s` is actually measured
    /// over. Low means the rate rests on a small billed subset — quote it.
    pub billed_gen_fraction: Option<f64>,
    /// Billed tokens over billed generation seconds. Generation time comes
    /// from the stream bookends, never from wall clock: wall includes tool
    /// execution and rest, and dividing by it understates the engine by
    /// however tool-heavy the task happened to be.
    pub tok_per_s: Option<f64>,
    /// The THINKING channel's rate, over every stream. Immune to the billing
    /// gap, and not a measure of answer output.
    pub reasoning_chars_per_s: Option<f64>,

    // --- detection --------------------------------------------------------
    pub gates: Gates,
    /// `dispatch start.bounds` — the resolved caps with their provenance, so
    /// an arm's settings are read from the run rather than assumed.
    pub bounds: BTreeMap<String, serde_json::Value>,

    // --- host -------------------------------------------------------------
    pub gpu_w_busy: Option<f64>,
    pub cpu_w_busy: Option<f64>,
    pub pkg_w_busy: Option<f64>,
    pub gpu_duty_pct: Option<f64>,
    pub samples_busy: usize,
    pub samples_idle: usize,
    /// Wall time spent busy, from the duty cycle. With `pkg_w_busy` this is
    /// the energy a run cost regardless of whether it finished.
    pub busy_ms: Option<u64>,
    pub pkg_j_busy: Option<f64>,
    /// Billed seconds against billed tokens — mixing all-generation seconds
    /// with billed-only tokens inflates this by exactly the unbilled fraction.
    pub pkg_j_per_1k_tokens: Option<f64>,
    pub pkg_j_per_1k_reasoning_chars: Option<f64>,
    pub thermal_states_busy: BTreeMap<String, usize>,
    /// The number that says work was actually SLOWED. A `fair` thermal state
    /// is a transient tolerance, not throttling.
    pub cpu_speed_limit_min: Option<u64>,
    pub throttled_samples: usize,
    /// Machine-wide, so a PRESSURE reading rather than this run's footprint.
    pub mem_pct_busy_max: Option<u64>,
    pub telemetry_max_gap_ms: Option<u64>,

    /// This session's flow records inside the scanned window. A provenance
    /// count, bounded by the same window as everything else here.
    pub flow_records_in_window: usize,
    pub flow_scan: FlowScan,
    pub checks: RunChecks,
}

impl RunStats {
    /// The checks that failed, as short reasons a renderer can print beside
    /// the figures. Empty means every number here reconciles.
    pub fn unreconciled(&self) -> Vec<&'static str> {
        let c = &self.checks;
        let mut out = Vec::new();
        if c.metrics_totals_zeroed {
            out.push(
                "metrics.json totals were zeroed by the runtime's error path; turns and rest \
                 come from the trajectory, and tokens cannot be reconciled",
            );
        }
        if c.tokens_reconcile == Some(false) {
            out.push("per-turn tokens do not sum to the run total");
        }
        if c.turns_match_trajectory == Some(false) {
            out.push("metrics.json turns and the trajectory's turns disagree");
        }
        if c.rest_matches_trajectory == Some(false) {
            out.push("metrics.json rest and the trajectory's rests disagree");
        }
        if !c.rest_within_wall {
            out.push("rest exceeds wall");
        }
        if !c.missing_required_events.is_empty() {
            out.push("the trajectory is missing events this reading keys on");
        }
        if !c.checkpoint_parse_consistent {
            out.push("checkpoint records present but none were parsed");
        }
        if !c.verdict_matches_ratio {
            out.push("the recorded verdicts and the tail-ratio threshold disagree");
        }
        if !c.all_streams_billed {
            out.push("tok/s and energy per token cover only the billed streams");
        }
        if !c.frames_match_streams {
            out.push("usage frames and streams did not pair 1:1");
        }
        if !c.streams_terminated {
            out.push("a stream never ended");
        }
        if !c.have_telemetry_samples {
            out.push("no host telemetry in the run window");
        } else if !c.telemetry_covers_run {
            out.push("host telemetry does not cover the whole run; busy time and energy are withheld");
        }
        if !c.have_flow_records {
            out.push("no flow records for this session in the window; its dispatch bounds are unknown");
        }
        if !self.suspect_turns.is_empty() {
            out.push("a turn's reasoning chars per token is implausible; its reasoning counts may not measure what they say");
        }
        out
    }
}

// ---------------------------------------------------------------------------
// Parsed inputs
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default, Deserialize)]
pub(crate) struct RuntimeMetrics {
    model: Option<String>,
    result: Option<String>,
    started_at_unix_ms: Option<u64>,
    wall_ms: Option<u64>,
    rest_ms: Option<u64>,
    turns: Option<u64>,
    compactions: Option<u64>,
    total_completion_tokens: Option<u64>,
}

#[derive(Debug, Clone, Default)]
struct Span {
    seq: Option<u64>,
    t0: Option<u64>,
    t1: Option<u64>,
    /// Content chars this stream produced. `model.partial.cumulative_chars`
    /// restarts with every stream, so a retried turn's content is the SUM
    /// over its streams, never the per-turn maximum.
    content: u64,
}

impl Span {
    fn ms(&self) -> u64 {
        match (self.t0, self.t1) {
            (Some(a), Some(b)) if b >= a => b - a,
            _ => 0,
        }
    }
}

#[derive(Debug, Clone, Default)]
struct Turn {
    completion_tokens: u64,
    reasoning_tokens: u64,
    reasoning_chars: u64,
    content_chars: u64,
}

#[derive(Debug, Clone, Default)]
struct Checkpoint {
    tail_ratio: Option<f64>,
    concluded: bool,
    would_conclude: bool,
}

#[derive(Debug, Clone, Default)]
struct Rest {
    ms: u64,
    reason: String,
    state: String,
}

/// Everything one pass over `trajectory.jsonl` yields.
#[derive(Debug, Clone, Default)]
pub(crate) struct Trajectory {
    spans: Vec<Span>,
    /// One entry per `model.completed`, IN ORDER. `None` is an unbilled
    /// stream (the gate ended the call before the endpoint's final usage
    /// frame), not a zero.
    frames: Vec<Option<u64>>,
    turns: BTreeMap<u64, Turn>,
    stream_observations: usize,
    stream_degenerate: BTreeSet<u64>,
    stream_aborts: BTreeSet<u64>,
    /// Abort RECORDS, not aborted turns: a retry can be aborted too, and the
    /// seq set above collapses the two.
    stream_abort_events: usize,
    stream_min_ratio: Option<f64>,
    checkpoints: BTreeMap<u64, Vec<Checkpoint>>,
    policy: Option<String>,
    rests: Vec<Rest>,
    tools: Vec<(String, bool)>,
    seen_types: BTreeSet<String>,
}

fn as_u64(v: Option<&serde_json::Value>) -> Option<u64> {
    v.and_then(|v| v.as_u64())
}

fn as_f64(v: Option<&serde_json::Value>) -> Option<f64> {
    v.and_then(|v| v.as_f64())
}

/// One pass over a trajectory. Lenient by construction: an unparsable line is
/// skipped rather than failing the read, because a run killed mid-write ends
/// in a partial line and its earlier events are still good data.
pub(crate) fn parse_trajectory(raw: &str) -> Trajectory {
    let mut t = Trajectory::default();
    for line in raw.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(ev) = serde_json::from_str::<serde_json::Value>(line) else { continue };
        let Some(kind) = ev.get("type").and_then(|v| v.as_str()) else { continue };
        t.seen_types.insert(kind.to_string());
        // Trajectory records are flat; the same events ride the flow stream
        // wrapped in a `payload`. Read either, so this module works against
        // both without a second parser.
        let body = ev.get("payload").unwrap_or(&ev);
        let seq = as_u64(body.get("seq")).or_else(|| as_u64(body.get("turn_seq")));

        match kind {
            "tool.completed" => {
                let name = body
                    .get("tool_name")
                    .and_then(|v| v.as_str())
                    .unwrap_or("?")
                    .to_string();
                let ok = body.get("ok").and_then(|v| v.as_bool()).unwrap_or(true);
                t.tools.push((name, ok));
            }
            "runtime.rest" => t.rests.push(Rest {
                ms: as_u64(body.get("ms")).unwrap_or(0),
                reason: body.get("reason").and_then(|v| v.as_str()).unwrap_or("?").to_string(),
                state: body.get("state").and_then(|v| v.as_str()).unwrap_or("?").to_string(),
            }),
            // The STREAMING gate. `degenerate` is the gate's own verdict and
            // is recorded whether or not the policy let it act.
            "dispatch.gate.observation" => {
                t.stream_observations += 1;
                if body.get("degenerate").and_then(|v| v.as_bool()) == Some(true) {
                    if let Some(s) = seq {
                        t.stream_degenerate.insert(s);
                    }
                }
                if let Some(r) = as_f64(body.get("tail_ratio")) {
                    t.stream_min_ratio = Some(t.stream_min_ratio.map_or(r, |m: f64| m.min(r)));
                }
            }
            "dispatch.gate.abort" => {
                t.stream_abort_events += 1;
                if let Some(s) = seq {
                    t.stream_aborts.insert(s);
                }
            }
            // The per-call-cap gate.
            "dispatch.checkpoint" => {
                if let Some(p) = body.get("policy").and_then(|v| v.as_str()) {
                    t.policy = Some(p.to_string());
                }
                let verdict = body.get("verdict").and_then(|v| v.as_str()).unwrap_or("");
                let concluded = verdict == "conclude";
                t.checkpoints.entry(seq.unwrap_or(0)).or_default().push(Checkpoint {
                    tail_ratio: as_f64(body.get("tail_ratio")),
                    concluded,
                    // Under `observe` the runtime records a degenerate
                    // finding as `would_conclude` while the verdict stays
                    // `continue`. Reading the VERDICT as the judgment makes
                    // every observing run look clean; reading
                    // `would_conclude` is what separates the finding from
                    // the action. Older records carry no such field, so a
                    // conclusion still counts as one.
                    would_conclude: body
                        .get("would_conclude")
                        .and_then(|v| v.as_bool())
                        .unwrap_or(concluded),
                });
            }
            "model.streaming.start" => t.spans.push(Span { seq, t0: as_u64(body.get("ts")), t1: None, content: 0 }),
            "model.streaming.end" => {
                // Close the most recent OPEN span on this seq. A seq can
                // carry more than one stream — an aborted turn is retried
                // under the same seq — so keying spans by seq collapses the
                // pair and mis-times both (measured: an 87.1s abort and its
                // 8.8s retry, both seq 2).
                if let Some(sp) = t
                    .spans
                    .iter_mut()
                    .rev()
                    .find(|sp| sp.seq == seq && sp.t1.is_none())
                {
                    sp.t1 = as_u64(body.get("ts"));
                }
            }
            "model.completed" => {
                let usage = body.get("usage");
                let ct = usage.and_then(|u| u.get("completion_tokens")).and_then(|v| v.as_u64());
                t.frames.push(ct);
                if let Some(s) = seq {
                    let e = t.turns.entry(s).or_default();
                    // ACCUMULATE. Every checkpoint continuation is the same
                    // logical turn resuming and reports its own frame.
                    e.completion_tokens += ct.unwrap_or(0);
                    e.reasoning_tokens += usage
                        .and_then(|u| u.get("reasoning_tokens"))
                        .and_then(|v| v.as_u64())
                        .unwrap_or(0);
                }
            }
            "model.reasoning" => {
                if let Some(s) = seq {
                    t.turns.entry(s).or_default().reasoning_chars +=
                        as_u64(body.get("reasoning_chars")).unwrap_or(0);
                }
            }
            "model.partial" => {
                // The CONTENT channel only; reasoning is not in it (measured:
                // 6,743 streamed chars against 43,263 reasoning chars on one
                // turn). Cumulative within ONE stream, so it is attributed to
                // the latest stream on this seq and summed across streams.
                let n = as_u64(body.get("cumulative_chars")).unwrap_or(0);
                if let Some(sp) = t.spans.iter_mut().rev().find(|sp| sp.seq == seq) {
                    sp.content = sp.content.max(n);
                } else if let Some(s) = seq {
                    let e = t.turns.entry(s).or_default();
                    e.content_chars = e.content_chars.max(n);
                }
            }
            _ => {}
        }
    }
    t
}

/// One host telemetry sample, already narrowed to the run's window.
#[derive(Debug, Clone, Default)]
pub(crate) struct Sample {
    ts: u64,
    /// The sampler's own stamp: milliseconds since its previous sample
    /// (measured equal to the actual gap). The time this sample stands for.
    interval_ms: Option<u64>,
    gpu_pct: u64,
    w_gpu: f64,
    w_cpu: f64,
    w_total: f64,
    thermal_state: Option<String>,
    cpu_speed_limit_pct: u64,
    mem_pct: u64,
}

impl Sample {
    fn busy(&self) -> bool {
        self.gpu_pct >= BUSY_GPU_PCT
    }
}

/// How far outside a run's own window a record that belongs to it can land.
///
/// The host emits `dispatch start` before the runtime stamps its own
/// `started_at_unix_ms`, and the terminal records land after `wall_ms` has
/// elapsed. Telemetry is NOT widened by this: power is averaged over samples
/// strictly inside the run, and only the file-selection and early-stop bounds
/// use the slack.
pub const WINDOW_SLACK_MS: u64 = 5 * 60 * 1000;

/// What reading the flow stream cost, stamped into the output so "the
/// observer was negligible" is a claim the data can check rather than an
/// assumption. The point of the bound: `files_read` and `lines_scanned` track
/// the size of the RUN, not the size of the archive.
#[derive(Debug, Clone, Default, Serialize)]
pub struct FlowScan {
    pub files_total: usize,
    /// Last written before the run began, so they cannot hold any of it.
    /// Never opened.
    pub files_skipped: usize,
    pub files_read: usize,
    /// Could not be opened. `files_total` always equals skipped + read +
    /// unreadable, so no file leaves the count silently.
    pub files_unreadable: usize,
    /// Files whose own clock passed the end of the window, so the rest of
    /// the file was not read.
    pub files_stopped_early: usize,
    pub lines_scanned: usize,
    pub gather_ms: u64,
}

/// What one pass over the flow stream yields for a run.
#[derive(Debug, Clone, Default)]
pub(crate) struct FlowFacts {
    samples: Vec<Sample>,
    records_for_session: usize,
    bounds: BTreeMap<String, serde_json::Value>,
    scan: FlowScan,
}

/// One pass over one flow file's lines. Collects telemetry inside
/// `[run_from, run_to]` and this session's records, and STOPS once the file's
/// own clock passes `run_to + WINDOW_SLACK_MS`.
///
/// Stopping is safe because flow files are append-only: a record is written
/// when it happens, so the order of lines is the order of time. The clock
/// read is `payload.sampled_at_ms`, which the telemetry sampler writes every
/// few seconds while the daemon runs. If there is no sampler, there is no
/// clock, and the file is read to the end — slower, never wrong.
///
/// Returns `(lines_scanned, stopped_early)`.
pub(crate) fn scan_flow_lines(
    mut reader: impl std::io::BufRead,
    run_from: u64,
    run_to: u64,
    session_id: Option<&str>,
    facts: &mut FlowFacts,
) -> (usize, bool) {
    let stop_after = run_to.saturating_add(WINDOW_SLACK_MS);
    let mut scanned = 0usize;
    let mut buf = Vec::new();
    loop {
        buf.clear();
        // Read line by line, so stopping early stops the READ, not just the
        // parse; and decode lossily, so one bad byte costs one line instead of
        // making the whole day file vanish.
        match reader.read_until(b'\n', &mut buf) {
            Ok(0) | Err(_) => break,
            Ok(_) => {}
        }
        scanned += 1;
        let text = String::from_utf8_lossy(&buf);
        let line = text.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(r) = serde_json::from_str::<serde_json::Value>(line) else { continue };
        let p = r.get("payload");
        let clock = p.and_then(|p| as_u64(p.get("sampled_at_ms")));
        if clock.is_some_and(|ts| ts > stop_after) {
            return (scanned, true);
        }

        if let Some(sid) = session_id {
            if r.get("session_id").and_then(|v| v.as_str()) == Some(sid) {
                facts.records_for_session += 1;
                if r.get("action").and_then(|v| v.as_str()) == Some("dispatch start") {
                    if let Some(b) = p.and_then(|p| p.get("bounds")).and_then(|b| b.as_object()) {
                        for (k, v) in b {
                            facts.bounds.entry(k.clone()).or_insert_with(|| v.clone());
                        }
                    }
                }
            }
        }

        if r.get("action").and_then(|v| v.as_str()) != Some("machine.telemetry") {
            continue;
        }
        let (Some(p), Some(ts)) = (p, clock) else { continue };
        if ts < run_from || ts > run_to {
            continue;
        }
        let Some(pw) = p.get("power_mw") else { continue };
        let thermal = p.get("thermal");
        facts.samples.push(Sample {
            ts,
            interval_ms: as_u64(p.get("interval_ms")),
            gpu_pct: as_u64(p.get("gpu_pct")).unwrap_or(0),
            w_gpu: as_f64(pw.get("gpu")).unwrap_or(0.0) / 1000.0,
            w_cpu: as_f64(pw.get("cpu")).unwrap_or(0.0) / 1000.0,
            w_total: as_f64(pw.get("total")).unwrap_or(0.0) / 1000.0,
            thermal_state: thermal
                .and_then(|t| t.get("state"))
                .and_then(|v| v.as_str())
                .map(|s| s.to_string()),
            cpu_speed_limit_pct: thermal
                .and_then(|t| as_u64(t.get("cpu_speed_limit_pct")))
                .unwrap_or(100),
            mem_pct: as_u64(p.get("mem_pct")).unwrap_or(0),
        });
    }
    (scanned, false)
}

fn mtime_ms(path: &Path) -> Option<u64> {
    let t = std::fs::metadata(path).ok()?.modified().ok()?;
    Some(t.duration_since(std::time::UNIX_EPOCH).ok()?.as_millis() as u64)
}

/// Read the part of the flow stream that can describe one run.
///
/// **The work is bounded by the run, not by the archive.** A flow directory
/// grows forever (289 MB across 125 files measured on one machine); reading
/// all of it to describe a run that lasted minutes is the same shape as a
/// query with no LIMIT. Two bounds keep it proportional:
///
/// 1. **A file last written before the run started cannot contain it**, so
///    it is never opened. This is decided by modification time, NOT by the
///    date in the file's name: names are UTC dates, and a reader that picks
///    files by name misses a run that crossed midnight. Modification time has
///    no such edge — a record inside the window forces its file's mtime past
///    the window's start.
/// 2. **A file is abandoned once its own clock passes the window's end**
///    (see [`scan_flow_lines`]).
fn read_flows(flows_dir: &Path, session_id: Option<&str>, run_from: u64, run_to: u64) -> FlowFacts {
    let started = std::time::Instant::now();
    let mut facts = FlowFacts::default();
    let Ok(entries) = std::fs::read_dir(flows_dir) else { return facts };
    let mut paths: Vec<_> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().and_then(|e| e.to_str()) == Some("jsonl"))
        .collect();
    paths.sort();
    let open_from = run_from.saturating_sub(WINDOW_SLACK_MS);
    for path in paths {
        facts.scan.files_total += 1;
        // An unreadable mtime is not evidence the file is old: read it.
        if mtime_ms(&path).is_some_and(|m| m < open_from) {
            facts.scan.files_skipped += 1;
            continue;
        }
        let Ok(file) = std::fs::File::open(&path) else {
            facts.scan.files_unreadable += 1;
            continue;
        };
        facts.scan.files_read += 1;
        let (lines, stopped) = scan_flow_lines(
            std::io::BufReader::new(file),
            run_from,
            run_to,
            session_id,
            &mut facts,
        );
        facts.scan.lines_scanned += lines;
        if stopped {
            facts.scan.files_stopped_early += 1;
        }
    }
    facts.scan.gather_ms = started.elapsed().as_millis() as u64;
    facts
}

/// How well the telemetry samples cover a run window.
struct Coverage {
    /// Time the BUSY samples stand for, and all samples, in ms.
    weight_busy: u64,
    weight_all: u64,
    /// Largest stretch of the window no sample stands for.
    max_gap_ms: Option<u64>,
    covers_run: bool,
}

/// The smallest uncovered gap that always counts as a hole, whatever the
/// sampler's cadence: a sample may land up to one interval late, but not
/// this far with nothing at all.
const TELEMETRY_GAP_FLOOR_MS: u64 = 10_000;

/// Each sample stands for the interval it closes: `[ts - interval_ms, ts]`,
/// clamped to the window. Where a sample carries no `interval_ms`, it stands
/// for the time back to the previous sample. A gap no sample stands for is
/// uncovered, and a run whose largest uncovered gap exceeds twice the
/// sampler's typical interval (at least [`TELEMETRY_GAP_FLOOR_MS`]) is not
/// covered: its duty cycle and energy would be extrapolated from a part of
/// the run that may not resemble the rest.
fn telemetry_coverage(samples: &[Sample], from: u64, to: u64) -> Coverage {
    let mut sorted: Vec<&Sample> = samples.iter().collect();
    sorted.sort_by_key(|s| s.ts);
    let (mut weight_busy, mut weight_all) = (0u64, 0u64);
    let mut covered_to = from;
    let mut max_gap = 0u64;
    let mut intervals: Vec<u64> = Vec::new();
    for s in &sorted {
        let back = s.interval_ms.unwrap_or(s.ts.saturating_sub(covered_to));
        if let Some(i) = s.interval_ms {
            intervals.push(i);
        }
        let start = s.ts.saturating_sub(back).max(from);
        max_gap = max_gap.max(start.saturating_sub(covered_to));
        let w = s.ts.saturating_sub(start.max(covered_to.min(s.ts)));
        weight_all += w;
        if s.busy() {
            weight_busy += w;
        }
        covered_to = covered_to.max(s.ts);
    }
    if sorted.is_empty() {
        return Coverage { weight_busy: 0, weight_all: 0, max_gap_ms: None, covers_run: false };
    }
    max_gap = max_gap.max(to.saturating_sub(covered_to));
    intervals.sort_unstable();
    let typical = intervals.get(intervals.len() / 2).copied().unwrap_or(0);
    let allowed = (2 * typical).max(TELEMETRY_GAP_FLOOR_MS);
    Coverage { weight_busy, weight_all, max_gap_ms: Some(max_gap), covers_run: max_gap <= allowed }
}

// ---------------------------------------------------------------------------
// Derivation
// ---------------------------------------------------------------------------

/// Derive metrics for a run named by id or path, reading the operator's
/// configured flow directory for host telemetry.
///
/// Resolution is [`crate::lab::inspect::resolve_run_dir`] — the same one
/// `lab run inspect` uses, so `stats` and `inspect` can never disagree about
/// which run they are describing.
pub fn run_stats(run: &str) -> Result<RunStats> {
    let dir = crate::lab::inspect::resolve_run_dir(run);
    compute_from_dir(&dir, &darkmux_types::config_access::flows_dir())
}

/// Derive the metrics for a run directory.
///
/// `flows_dir` is read for host telemetry and dispatch bounds; a missing or
/// empty one is not an error — it means the power arm has no data, which the
/// checks say plainly rather than reporting zero watts.
pub fn compute_from_dir(run_dir: &Path, flows_dir: &Path) -> Result<RunStats> {
    if !run_dir.is_dir() {
        anyhow::bail!(
            "no run directory at {}; pass a path, or a run id under {}",
            run_dir.display(),
            darkmux_types::config_access::lab_dir().display()
        );
    }
    let metrics_path = run_dir.join("metrics.json");
    let raw = std::fs::read_to_string(&metrics_path).with_context(|| {
        format!(
            "reading {}: a run without runtime metrics has no derivable numbers",
            metrics_path.display()
        )
    })?;
    let m: RuntimeMetrics = serde_json::from_str(&raw)
        .with_context(|| format!("parsing {}", metrics_path.display()))?;

    let traj_raw = std::fs::read_to_string(run_dir.join("trajectory.jsonl")).unwrap_or_default();
    let t = parse_trajectory(&traj_raw);

    let side = |name: &str, key: &str| -> Option<serde_json::Value> {
        let raw = std::fs::read_to_string(run_dir.join(name)).ok()?;
        let v: serde_json::Value = serde_json::from_str(&raw).ok()?;
        v.get(key).cloned()
    };
    let session_id = side("lifecycle.json", "session_id")
        .or_else(|| side("manifest.json", "session_id"))
        .and_then(|v| v.as_str().map(|s| s.to_string()));
    // The manifest records verify as `{passed, details}`; older runs wrote a
    // bare string. Normalized to one word here so a caller comparing runs
    // across that change is not comparing two shapes.
    let verify = side("manifest.json", "verify").and_then(|v| match &v {
        serde_json::Value::String(s) => Some(s.clone()),
        _ => v
            .get("passed")
            .and_then(|p| p.as_bool())
            .map(|p| if p { "pass" } else { "fail" }.to_string()),
    });
    let ok = side("manifest.json", "ok").and_then(|v| v.as_bool());

    let wall_ms = m.wall_ms.unwrap_or(0);
    let started = m.started_at_unix_ms.unwrap_or(0);
    let flows = read_flows(flows_dir, session_id.as_deref(), started, started + wall_ms);

    Ok(derive_stats(
        run_dir
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("")
            .to_string(),
        m,
        t,
        flows,
        verify,
        ok,
    ))
}

pub(crate) fn derive_stats(
    run: String,
    m: RuntimeMetrics,
    t: Trajectory,
    flows: FlowFacts,
    verify: Option<String>,
    ok: Option<bool>,
) -> RunStats {
    let wall_ms = m.wall_ms.unwrap_or(0);
    let run_from = m.started_at_unix_ms.unwrap_or(0);
    let run_to = run_from + wall_ms;

    // --- turn and rest counters -------------------------------------------
    // `metrics.json` is the runtime's own count and normally matches the
    // trajectory exactly (verified on every completed run measured). But the
    // runtime's ERROR path writes it with turns and rest hardcoded to zero,
    // and says in its own comment that the real totals live only in the
    // trajectory. Reading metrics there printed "rest 0s, 0 turns" beside
    // three rests and 234k tokens, and overstated active time by the hidden
    // rest. So: metrics where they are real, the trajectory where they are
    // known not to be, and a check wherever both exist.
    let traj_turns = t
        .spans
        .iter()
        .filter_map(|s| s.seq)
        .chain(t.turns.keys().copied())
        .collect::<BTreeSet<u64>>()
        .len() as u64;
    let traj_rest: u64 = t.rests.iter().map(|r| r.ms).sum();
    let has_trajectory = !t.seen_types.is_empty();
    let zeroed = has_trajectory && m.result.as_deref() == Some("error");
    let (turns, rest_ms) = if zeroed {
        (traj_turns, traj_rest)
    } else {
        (m.turns.unwrap_or(0), m.rest_ms.unwrap_or(0))
    };
    let turns_match = (!zeroed && traj_turns > 0).then(|| traj_turns == m.turns.unwrap_or(0));
    let rest_match = (!zeroed && has_trajectory).then(|| traj_rest == m.rest_ms.unwrap_or(0));

    // --- which streams were billed ----------------------------------------
    // Frames and spans arrive 1:1 in the same order, so span i is unbilled
    // iff frame i is null. That pairing is only safe while the counts match,
    // which `frames_match_streams` asserts; when it does not, fall back to
    // the gate's own abort records rather than mapping by a broken index.
    let frames_match = t.frames.len() == t.spans.len();
    let mut unbilled: BTreeSet<usize> = BTreeSet::new();
    if frames_match {
        for (i, f) in t.frames.iter().enumerate() {
            if f.is_none() {
                unbilled.insert(i);
            }
        }
    }
    // Only when the null frames could NOT identify the unbilled streams:
    // guess that the aborted span on an aborted seq is the LONGEST one. That
    // holds on every abort measured so far, but nothing in the producer
    // guarantees it — a gate firing at its first observation leaves a short
    // abort before a long billed retry — so it never overrides an exact
    // pairing. Running it anyway marked a billed 60 s retry as unbilled and
    // reported 700 tok/s against a true 100.
    for seq in t.stream_aborts.iter().filter(|_| !frames_match) {
        if let Some((i, _)) = t
            .spans
            .iter()
            .enumerate()
            .filter(|(_, sp)| sp.seq.as_ref() == Some(seq))
            .max_by_key(|(_, sp)| sp.ms())
        {
            unbilled.insert(i);
        }
    }

    let gen_ms_all: u64 = t.spans.iter().map(|s| s.ms()).sum();
    let gen_ms_billed: u64 = t
        .spans
        .iter()
        .enumerate()
        .filter(|(i, _)| !unbilled.contains(i))
        .map(|(_, s)| s.ms())
        .sum();
    let unterminated = t.spans.iter().filter(|s| s.t1.is_none()).count();

    let completion_tokens: u64 = t.turns.values().map(|e| e.completion_tokens).sum();
    let reasoning_tokens: u64 = t.turns.values().map(|e| e.reasoning_tokens).sum();
    let reasoning_chars: u64 = t.turns.values().map(|e| e.reasoning_chars).sum();
    let content_chars: u64 = t.spans.iter().map(|s| s.content).sum::<u64>()
        + t.turns.values().map(|e| e.content_chars).sum::<u64>();

    let tok_per_s = if gen_ms_billed > 0 && completion_tokens > 0 {
        Some(round(completion_tokens as f64 / (gen_ms_billed as f64 / 1000.0), 1))
    } else {
        None
    };
    let reasoning_chars_per_s = if gen_ms_all > 0 && reasoning_chars > 0 {
        Some(round(reasoning_chars as f64 / (gen_ms_all as f64 / 1000.0), 1))
    } else {
        None
    };
    let billed_gen_fraction = if gen_ms_all > 0 {
        Some(round(gen_ms_billed as f64 / gen_ms_all as f64, 2))
    } else {
        None
    };

    // Compare reasoning CHARS against reasoning TOKENS. An earlier reading
    // divided by TOTAL completion tokens, which made any turn with large
    // tool-call arguments and little prose look implausible — three healthy
    // turns flagged as suspect.
    let suspect_turns: Vec<SuspectTurn> = t
        .turns
        .iter()
        .filter_map(|(seq, e)| {
            if e.reasoning_tokens == 0 || e.reasoning_chars == 0 {
                return None;
            }
            let cpt = e.reasoning_chars as f64 / e.reasoning_tokens as f64;
            (cpt < PLAUSIBLE_CHARS_PER_TOKEN.0 || cpt > PLAUSIBLE_CHARS_PER_TOKEN.1).then(|| SuspectTurn {
                seq: *seq,
                reasoning_chars_per_token: round(cpt, 1),
            })
        })
        .collect();

    // --- the two gates ----------------------------------------------------
    let mut cp_min: BTreeMap<u64, f64> = BTreeMap::new();
    let mut concluded: Vec<u64> = Vec::new();
    let mut judged: Vec<u64> = Vec::new();
    let mut cp_observations = 0usize;
    for (seq, obs) in &t.checkpoints {
        cp_observations += obs.len();
        for o in obs {
            // A checkpoint with no ratio carries no evidence either way; it
            // must not count as a perfectly unique 1.0.
            if let Some(r) = o.tail_ratio {
                let e = cp_min.entry(*seq).or_insert(r);
                *e = e.min(r);
            }
        }
        if obs.iter().any(|o| o.concluded) {
            concluded.push(*seq);
        }
        if obs.iter().any(|o| o.would_conclude) {
            judged.push(*seq);
        }
    }
    let by_ratio: Vec<u64> = cp_min
        .iter()
        .filter(|(_, r)| **r < DEGENERATE_TAIL_RATIO)
        .map(|(s, _)| *s)
        .collect();

    let gates = Gates {
        stream: StreamGate {
            observations: t.stream_observations,
            degenerate_turns: t.stream_degenerate.iter().copied().collect(),
            aborts: t.stream_abort_events,
            min_tail_ratio: t.stream_min_ratio,
        },
        checkpoint: CheckpointGate {
            observations: cp_observations,
            degenerate_turns: judged.clone(),
            concluded_turns: concluded,
            degenerate_turns_by_ratio: by_ratio.clone(),
            min_tail_ratio: cp_min
                .values()
                .copied()
                .fold(None, |a: Option<f64>, r| Some(a.map_or(r, |m| m.min(r)))),
            policy: t.policy.clone(),
        },
    };

    // --- host -------------------------------------------------------------
    let busy: Vec<&Sample> = flows.samples.iter().filter(|s| s.busy()).collect();
    let idle = flows.samples.len() - busy.len();
    let pkg_w = mean(busy.iter().map(|s| s.w_total));
    let cover = telemetry_coverage(&flows.samples, run_from, run_to);
    // Duty weighs each sample by the time it stands for. The sampler runs at
    // ~5 s while busy and ~51 s while idle, so a COUNT of samples
    // over-weights busy time by up to 10x.
    let duty = (cover.weight_all > 0).then(|| round(100.0 * cover.weight_busy as f64 / cover.weight_all as f64, 1));
    // Busy time and the energy built on it extrapolate from the samples to
    // the whole run, so they are only reported when the samples cover it.
    // Two samples in the last ten seconds of a ten-minute run extrapolated
    // to 600 s busy and 27 kJ, with no caveat.
    let busy_ms = duty
        .filter(|_| cover.covers_run)
        .map(|d| (wall_ms as f64 * d / 100.0).round() as u64);
    let mut thermal_states_busy: BTreeMap<String, usize> = BTreeMap::new();
    for s in &busy {
        if let Some(st) = &s.thermal_state {
            *thermal_states_busy.entry(st.clone()).or_insert(0) += 1;
        }
    }

    let mut tool_calls: BTreeMap<String, usize> = BTreeMap::new();
    for (name, _) in &t.tools {
        *tool_calls.entry(name.clone()).or_insert(0) += 1;
    }

    let rest_delays: Vec<u64> = t.rests.iter().map(|r| r.ms).collect::<BTreeSet<_>>().into_iter().collect();
    let missing_required: Vec<String> = if t.seen_types.is_empty() {
        Vec::new()
    } else {
        REQUIRED_EVENTS
            .iter()
            .filter(|e| !t.seen_types.contains(**e))
            .map(|e| e.to_string())
            .collect()
    };

    RunStats {
        schema_version: RUN_STATS_SCHEMA_VERSION,
        run,
        model: m.model.clone(),
        result: m.result.clone(),
        verify,
        ok,
        wall_ms,
        rest_ms,
        active_ms: wall_ms.saturating_sub(rest_ms),
        rest_events: t.rests.len(),
        thermal_ratchet_fired: rest_delays.len() > 1,
        rest_delays_ms: rest_delays,
        rest_reasons: t.rests.iter().map(|r| r.reason.clone()).collect::<BTreeSet<_>>().into_iter().collect(),
        rest_states: t.rests.iter().map(|r| r.state.clone()).collect::<BTreeSet<_>>().into_iter().collect(),
        rest_ms_per_turn: (turns > 0).then(|| round(rest_ms as f64 / turns as f64, 1)),
        turns,
        compactions: m.compactions.unwrap_or(0),
        reasoning_chars,
        content_chars,
        completion_tokens,
        reasoning_tokens,
        suspect_turns,
        tool_calls,
        tool_calls_total: t.tools.len(),
        tool_calls_failed: t.tools.iter().filter(|(_, ok)| !ok).count(),
        streams: t.spans.len(),
        streams_unbilled: unbilled.len(),
        streams_unterminated: unterminated,
        turns_with_stream_abort: t.stream_aborts.iter().copied().collect(),
        gen_ms_billed,
        gen_ms_all,
        unbilled_gen_ms: gen_ms_all.saturating_sub(gen_ms_billed),
        billed_gen_fraction,
        tok_per_s,
        reasoning_chars_per_s,
        gates,
        bounds: flows.bounds.clone(),
        gpu_w_busy: mean(busy.iter().map(|s| s.w_gpu)).map(|v| round(v, 1)),
        cpu_w_busy: mean(busy.iter().map(|s| s.w_cpu)).map(|v| round(v, 1)),
        pkg_w_busy: pkg_w.map(|v| round(v, 1)),
        gpu_duty_pct: duty,
        samples_busy: busy.len(),
        samples_idle: idle,
        busy_ms,
        pkg_j_busy: match (pkg_w, busy_ms) {
            (Some(w), Some(ms)) => Some(round(w * ms as f64 / 1000.0, 1)),
            _ => None,
        },
        pkg_j_per_1k_tokens: match pkg_w {
            Some(w) if gen_ms_billed > 0 && completion_tokens > 0 => Some(round(
                w * (gen_ms_billed as f64 / 1000.0) / (completion_tokens as f64 / 1000.0),
                1,
            )),
            _ => None,
        },
        pkg_j_per_1k_reasoning_chars: match pkg_w {
            Some(w) if gen_ms_all > 0 && reasoning_chars > 0 => Some(round(
                w * (gen_ms_all as f64 / 1000.0) / (reasoning_chars as f64 / 1000.0),
                1,
            )),
            _ => None,
        },
        thermal_states_busy,
        cpu_speed_limit_min: busy.iter().map(|s| s.cpu_speed_limit_pct).min(),
        throttled_samples: busy.iter().filter(|s| s.cpu_speed_limit_pct < 100).count(),
        mem_pct_busy_max: busy.iter().map(|s| s.mem_pct).max(),
        telemetry_max_gap_ms: cover.max_gap_ms,
        flow_records_in_window: flows.records_for_session,
        flow_scan: flows.scan.clone(),
        checks: RunChecks {
            // Nothing to reconcile against when the runtime zeroed its own
            // totals: `None` says "not checkable", which is not a pass.
            tokens_reconcile: (!zeroed)
                .then(|| completion_tokens == m.total_completion_tokens.unwrap_or(0)),
            metrics_totals_zeroed: zeroed,
            rest_matches_trajectory: rest_match,
            turns_match_trajectory: turns_match,
            telemetry_covers_run: cover.covers_run,
            rest_within_wall: rest_ms <= wall_ms,
            have_telemetry_samples: !flows.samples.is_empty(),
            have_flow_records: flows.records_for_session > 0,
            missing_required_events: missing_required,
            checkpoint_events_seen: t.seen_types.contains("dispatch.checkpoint"),
            // The mutation that proves this guard: repoint the parse at a
            // name that does not exist and `checkpoint_events_seen` stays
            // true while the count drops to zero. That CONTRADICTION is the
            // signal.
            checkpoint_parse_consistent: !(t.seen_types.contains("dispatch.checkpoint")
                && cp_observations == 0),
            verdict_matches_ratio: judged == by_ratio,
            all_streams_billed: unbilled.is_empty(),
            frames_match_streams: frames_match,
            streams_terminated: unterminated == 0,
        },
    }
}

#[cfg(test)]
#[path = "stats_tests.rs"]
mod tests;
