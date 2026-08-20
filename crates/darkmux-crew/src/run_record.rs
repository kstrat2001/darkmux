//! Per-step and per-seat execution records, plus the resolved-staffing
//! snapshot — the run-record substrate #1877 (item 2) extracts out of
//! `darkmux_lab::lab::review`, the one mission that had built it for
//! itself. None of these types know anything about flags, rulings, or
//! judging; they describe what ANY mission run did (which steps ran, how
//! long, how many items in/out), what it staffed (which model filled which
//! seat, at what context/token limits, chosen how), and how much of that
//! it actually accounts for in tokens/wall-clock.
//!
//! **Why this matters (#1877's own framing):** CLAUDE.md states the knob-
//! snapshot doctrine ("the resolved knob config snapshotted into the
//! artifact so a run is self-describing") as binding on every measurement-
//! grade run. Before this extraction, exactly one mission satisfied it —
//! not because the others decided host telemetry or a staffing snapshot
//! didn't matter, but because [`StepRecord`], [`MemberRecord`], and
//! [`StaffingSnapshot`] were defined once, privately, inside `review.rs`,
//! and so were literally unreachable from anywhere else in the tree. This
//! module is where a second mission (`coder-phase`, #1877's own acceptance
//! test) will reach for the same substrate instead of copying it.
//!
//! `review.rs` remains the first (and, as of this PR, only) consumer —
//! it re-exports these types from its own module path so every existing
//! external reference (`darkmux_lab::lab::review::MemberRecord`, etc.)
//! keeps resolving unchanged. See that module's own doc for how the
//! [`StepRecord`]s/[`MemberRecord`]s it constructs feed `ReviewEnvelope`.
//!
//! **#1877 item 3 (this module's own follow-up, now shipped):**
//! `crate::scheduler::run_step_graph` now times and emits a [`StepRecord`]
//! per step itself — see `apply_step_terminal` and the job closure in
//! `scheduler.rs`. A mission gets these BY CONSTRUCTION, with no opt-in and
//! no cooperation required from the `StepKind` that ran: `step_id`/`kind`/
//! `wall_ms` are things the scheduler genuinely observes (which step ran,
//! what kind it was registered under, how long its own `run_streaming` call
//! took — timed per-step, on that step's own worker thread, never a
//! wave-wide clock). `items_in`/`items_out` are NOT scheduler-observable —
//! they are per-kind business semantics (review's dedup step knows it took
//! N deduped flags and produced M; the scheduler only knows a step ran) —
//! so [`StepRecord::items_in`]/[`StepRecord::items_out`] are `Option<usize>`
//! and the scheduler always leaves them `None`. `None` is the honest
//! "the producer does not know this," never a lying `Some(0)`: this is the
//! same discipline this arc has applied everywhere else a run's substrate
//! cannot claim more than it actually observed.
//!
//! `darkmux_lab::lab::review`'s two drivers still hand-build their OWN
//! `StepRecord`s (`finish_review`/`run_judge_only`'s five call sites, which
//! know real `items_in`/`items_out` counts the scheduler cannot) and this is
//! a deliberate, reconciled choice, not an oversight: review's sequential
//! `run_judge_only` path never calls `run_step_graph` at all (no scheduler
//! involved), and the scheduler's new `SchedulerReport::step_records` is not
//! merged into `ReviewEnvelope::steps` on the graph path either (confirmed
//! by `review_tests.rs`'s own comment: no graph step kind writes
//! `env.steps`, so it stays empty end-to-end there, unchanged by this PR).
//! The two producers are structurally disjoint today — nothing combines
//! them, so there is no double count to reconcile. Wiring the graph path's
//! `env.steps` from `SchedulerReport::step_records` (recovering
//! `items_in`/`items_out` from wherever a mission's own business logic
//! knows them) is left for a later PR — likely the same one that gives
//! `coder-phase` its own typed envelope home, since that is the arc's next
//! item and the natural place to decide how a mission backfills the
//! scheduler's `None`s.

use crate::resourcing::{ResolvedSeatStaffing, StaffingProvenance};
use darkmux_profiles::swap;
use darkmux_types::{BundleSelector, ModelEndpoint, ProfileModel};
use serde::{Deserialize, Serialize};

// ─── per-run execution records ───────────────────────────────────────────

/// Per-model resource accounting — one row per seat a run staffed (a
/// review's probe staffings plus its judge seat; any future mission's own
/// per-seat dispatch accounting).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct MemberRecord {
    pub model: String,
    pub seat: String,
    pub draws: u32,
    pub wall_ms: u64,
    pub total_tokens: u64,
    /// (#1260/#1186) `true` when this seat dispatched to a remote endpoint —
    /// its `total_tokens` are CLOUD tokens, which downstream savings
    /// surfaces must exclude (remote work is never "off the meter").
    /// Skipped when `false` so local-only envelopes serialize unchanged.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub remote: bool,
    /// (#1260) Endpoint HOST only (e.g. `myorg.cognitiveservices.azure.com`)
    /// — never credentials, never the full deployment path.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub endpoint: Option<String>,
    /// (#1300) The model the endpoint's response body actually reported it
    /// served (`SingleShotReply.model`, the OpenAI-compatible completion's
    /// top-level `model` field) — DISTINCT from `model` above, which is the
    /// requested/declared identifier (a deployment name can alias to a
    /// different underlying model; for local seats `lms ps` is ground truth
    /// and this is always `None`). `None` when the response omitted the
    /// field, not when they match — provenance surfaces compare the two and
    /// only call out a mismatch, never assume aliasing from absence.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub served_model: Option<String>,
}

/// One pipeline step's in/out counts + wall time — the issue #1230 bridge:
/// a future flow-record consumer can render a run as a step timeline
/// without re-deriving it from a nested-array envelope. Realized by the
/// `step result` flow record (#1247 Part 1) — the live-run counterpart of
/// this end-of-run summary. See this module's doc for who produces these
/// today: `crate::scheduler::run_step_graph` (every step, every mission,
/// `items_in`/`items_out` always `None`) and review's own two drivers
/// (`items_in`/`items_out` always `Some`, since they know the counts).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StepRecord {
    /// Mission-defined step identity (review's steps are `bundle` |
    /// `probe` | `dedup` | `judge-pass1` | `judge-pass2`). A
    /// scheduler-produced record uses the step's own `Step::id`.
    pub step_id: String,
    /// Review's hand-built records use the coarse `procedural` (no
    /// dispatch) | `dispatch` (LMStudio calls) convention. A
    /// scheduler-produced record uses the step's own `Step::kind` — the
    /// full step-kind-registry id (e.g. `"dispatch.internal"`,
    /// `"procedural.shell"`) — since that is the genuine, precise value
    /// the scheduler has in hand; it does not know which registry kinds
    /// count as "dispatch" in review's coarser sense.
    pub kind: String,
    /// How many items this step consumed, when the producer knows —
    /// per-kind business semantics the scheduler cannot observe from
    /// outside a `StepKind::run_streaming` call. `None` means "not known
    /// to this producer," never a lying `Some(0)`. Skipped from JSON
    /// entirely when `None`, so an old reader parsing a scheduler-produced
    /// record never mistakes absence for a real zero-item step.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub items_in: Option<usize>,
    /// See [`Self::items_in`] — same honesty contract, output side.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub items_out: Option<usize>,
    pub wall_ms: u64,
}

// ─── resolved staffing snapshot ──────────────────────────────────────────

/// (#1260) The dispatch identity for one seat. LOCAL seats use the
/// darkmux-namespaced LMStudio identifier (`swap::namespaced_identifier`);
/// REMOTE seats keep the profile's bare model id — nothing is loaded into
/// LMStudio, so no `darkmux:` namespace entry is ever minted for them (the
/// namespace marks darkmux-owned LOCAL residency, and a remote seat has
/// none).
pub fn seat_identifier(pm: &ProfileModel) -> String {
    if pm.is_remote() {
        pm.id.clone()
    } else {
        swap::namespaced_identifier(pm)
    }
}

/// (#1260) The remote endpoint HOST for provenance records — host only,
/// NEVER credentials and never the full deployment path (an Azure
/// deployment URL embeds the deployment name; the host is the boundary
/// operators reason about). `None` for local seats.
pub fn seat_endpoint_host(pm: &ProfileModel) -> Option<String> {
    let ep = pm.endpoint.as_ref().filter(|e| e.is_remote())?;
    let url = ep.base_url();
    let authority = url.split("://").nth(1).and_then(|s| s.split('/').next()).unwrap_or("remote");
    // (#1530) Strip URL userinfo before the `@`. This function's contract is
    // HOST ONLY, NEVER CREDENTIALS — and its output is stamped into step
    // config, `MemberRecord.endpoint`, and a run's envelope, which CI
    // uploads as a public artifact. The sanctioned way to carry a key is
    // `EndpointAuth` (a Keychain item name or an env-var name, never a
    // value), but nothing stops an operator pasting `https://tok@proxy/v1`
    // into a profile's `url` — some LiteLLM/proxy setups document exactly
    // that form. Without this, that token would ride to a public surface.
    Some(authority.rsplit('@').next().unwrap_or(authority).to_string())
}

/// (#1260) The endpoint a seat's chat calls should route through — `Some`
/// only when the staffing's resolved model declares a remote endpoint.
pub fn seat_endpoint(pm: &ProfileModel) -> Option<&ModelEndpoint> {
    pm.endpoint.as_ref().filter(|e| e.is_remote())
}

/// One seat staffing's resolved config, snapshotted as ACTUALLY used — see
/// [`StaffingSnapshot`], and `darkmux_lab::lab::review::ReviewEnvelope::
/// staffing` for the first consumer's own envelope field.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SeatStaffingSnapshot {
    pub name: String,
    /// (#1475 packet 2) The task ROLE this seat was staffed for (review's
    /// `review-probe-high`/`-mid`/`-low`, `review-judge`, `review-verify`)
    /// when the role→profile flip produced it — so the snapshot names WHICH
    /// role bound this seat's profile+model, the truthful record of the
    /// task→role→profile→model rollup (operator sovereignty #44). `Option` +
    /// `#[serde(default)]`: absent on the legacy roster-scoring staffings and
    /// on pre-#1475 snapshots, the module's standard schema-lenience.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub role_id: Option<String>,
    /// The darkmux-namespaced LMStudio identifier for a LOCAL seat; the
    /// profile's bare model id for a REMOTE one — the same form
    /// [`MemberRecord::model`] records, so the two line up at a glance.
    pub model: String,
    /// (#1260) `true` when the staffing's model declares a remote endpoint.
    /// Skipped when `false` so pre-#1260 snapshots round-trip unchanged.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub remote: bool,
    /// (#1260) Endpoint HOST only — never credentials, never the full
    /// deployment path.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub endpoint: Option<String>,
    pub k: u32,
    /// (#1266) The judge seat's resolved consensus depth (`passes` — 1
    /// single / 2 double-confirm / N unanimous), snapshotted so every run is
    /// self-describing. Present on every seat's snapshot the same way `k` is
    /// (the judge is the consumer; other seats carry it inertly). Defaults to
    /// 2 on read so a pre-1.3 snapshot (this field didn't exist) deserializes
    /// as today's double-confirm rather than a hard parse failure — the
    /// module's standard schema-lenience.
    #[serde(default = "default_snapshot_passes")]
    pub passes: u32,
    /// The resolved `ProfileModel`'s DECLARED context length — settings
    /// provenance per run, so "what context was this model loaded at" is
    /// never a forensic question (a sibling concern to the config-vs-
    /// measured-context mismatch class of bug #1135 shipped). `Option` +
    /// `#[serde(default)]` so a pre-#1256 snapshot (staffing existed, this
    /// field didn't) deserializes as `None` rather than a hard parse failure
    /// — the same schema-lenience discipline every field in this module
    /// follows. (#1282: `n_ctx` is optional on `ProfileModel` itself now —
    /// resolution requires it for LOCAL seats, so their snapshots always
    /// carry a value; a REMOTE seat, #1260, has no local context and stays
    /// `None` here.)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub n_ctx: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub selector: Option<BundleSelector>,
    /// (#1426 ship-2 / #44) HOW this seat's model was chosen — `scored`
    /// (capability scoring against the roster profile, with what it scored
    /// against) or `pinned` (which launch param pinned it). The snapshot
    /// already recorded WHAT resolved; this records WHY, so the operator
    /// never wonders where the staffing decision came from. `Option` +
    /// `#[serde(default)]` — a pre-ship-2 snapshot (field absent)
    /// deserializes as `None`, the module's standard schema-lenience.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provenance: Option<StaffingProvenance>,
}

/// Per-seat resolved staffing snapshot — review's `review-probe` (one or
/// more staffings) + `review-judge` (exactly one) + the optional
/// `review-verify` seat (#1260) is the shape this was built against; any
/// mission with a probe/judge/verify-shaped seat set reuses it as-is. See
/// `darkmux_lab::lab::review::ReviewEnvelope::staffing` for the first
/// consumer's own envelope field.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct StaffingSnapshot {
    pub probes: Vec<SeatStaffingSnapshot>,
    pub judge: Option<SeatStaffingSnapshot>,
    /// (#1260) Present iff the crew declares the `review-verify` seat —
    /// absent (and never serialized) otherwise, so pre-#1260 snapshots
    /// round-trip unchanged.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub verify: Option<SeatStaffingSnapshot>,
    /// (#1302) The crew's resolved `request_changes` flag — snapshotted so the
    /// render path reads the run's own blocking-vs-advisory choice from its
    /// self-describing artifact, and a serialized envelope re-rendered later
    /// picks the same review event. Defaults to `false` on read (skipped when
    /// `false`) so pre-#1302 snapshots round-trip unchanged as the non-blocking
    /// default.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub request_changes: bool,
}

/// (#1266) Snapshot default for `SeatStaffingSnapshot::passes` — 2 (double-
/// confirm), so a pre-1.3 envelope missing the field reads as today's judge.
fn default_snapshot_passes() -> u32 {
    2
}

pub fn staffing_snapshot(
    probes: &[ResolvedSeatStaffing],
    judge: &ResolvedSeatStaffing,
    verify: Option<&ResolvedSeatStaffing>,
    request_changes: bool,
) -> StaffingSnapshot {
    fn one(s: &ResolvedSeatStaffing) -> SeatStaffingSnapshot {
        SeatStaffingSnapshot {
            name: s.name.clone(),
            // (#1475 packet 2) Carry the seat's bound role verbatim so the
            // snapshot records role → profile → model, not just profile+model.
            role_id: s.role_id.clone(),
            model: seat_identifier(&s.pm),
            remote: s.pm.is_remote(),
            endpoint: seat_endpoint_host(&s.pm),
            k: s.k,
            passes: s.passes,
            n_ctx: s.pm.n_ctx,
            max_tokens: s.max_tokens,
            selector: s.selector.clone(),
            // (#44) Scored-vs-pinned, carried verbatim from the resolver.
            provenance: s.provenance.clone(),
        }
    }
    StaffingSnapshot {
        probes: probes.iter().map(one).collect(),
        judge: Some(one(judge)),
        verify: verify.map(one),
        // (#1302) The run's blocking-vs-advisory choice, snapshotted for the
        // render path (see `StaffingSnapshot::request_changes`).
        request_changes,
    }
}

// ─── #1877 item 2 conformance: the moved types serialize unchanged ───────

#[cfg(test)]
mod tests {
    use super::*;
    use darkmux_types::ProfileModel;

    /// Golden JSON for [`MemberRecord`] — pins both the emitted fields AND
    /// the `skip_serializing_if` behavior (`remote`/`endpoint`/
    /// `served_model` never appear when they're the "nothing to say"
    /// default) exactly as `darkmux_lab::lab::review`'s own copy serialized
    /// before this type moved. **Proved failing first**: commenting out
    /// `#[serde(default, skip_serializing_if = "std::ops::Not::not")]` on
    /// `remote` (leaving the field un-skippable) made this test fail with
    /// an unexpected `"remote":false` key in the local-seat case — restored
    /// before committing.
    #[test]
    fn member_record_serializes_with_the_pre_move_shape() {
        let local = MemberRecord {
            model: "darkmux:probe-a".into(),
            seat: "review-probe".into(),
            draws: 2,
            wall_ms: 1500,
            total_tokens: 4200,
            ..Default::default()
        };
        assert_eq!(
            serde_json::to_value(&local).unwrap(),
            serde_json::json!({
                "model": "darkmux:probe-a",
                "seat": "review-probe",
                "draws": 2,
                "wall_ms": 1500,
                "total_tokens": 4200
            }),
            "a local (non-remote, no served_model) seat must not serialize remote/endpoint/served_model at all"
        );

        let remote = MemberRecord {
            model: "gpt-4o".into(),
            seat: "review-judge".into(),
            remote: true,
            endpoint: Some("myorg.cognitiveservices.azure.com".into()),
            served_model: Some("gpt-4o-2024-08-06".into()),
            ..Default::default()
        };
        assert_eq!(
            serde_json::to_value(&remote).unwrap(),
            serde_json::json!({
                "model": "gpt-4o",
                "seat": "review-judge",
                "draws": 0,
                "wall_ms": 0,
                "total_tokens": 0,
                "remote": true,
                "endpoint": "myorg.cognitiveservices.azure.com",
                "served_model": "gpt-4o-2024-08-06"
            })
        );
    }

    /// Golden JSON for [`StepRecord`] when the producer KNOWS its item
    /// counts (review's own hand-built records, both before and after
    /// #1877 item 3's `Option` change) — pins that `Some(n)` still
    /// serializes as the bare number, byte-identical to the pre-`Option`
    /// shape review.rs has always emitted. **Proved failing first**: before
    /// wrapping `items_in`/`items_out` in `Some(..)`, this test failed to
    /// compile (`expected Option<usize>, found integer`) — the type change
    /// forces every existing call site to say explicitly whether it knows
    /// the count.
    #[test]
    fn step_record_with_known_items_serializes_with_the_pre_move_shape() {
        let step = StepRecord {
            step_id: "judge-pass1".into(),
            kind: "dispatch".into(),
            items_in: Some(134),
            items_out: Some(123),
            wall_ms: 87_000,
        };
        assert_eq!(
            serde_json::to_value(&step).unwrap(),
            serde_json::json!({
                "step_id": "judge-pass1",
                "kind": "dispatch",
                "items_in": 134,
                "items_out": 123,
                "wall_ms": 87000
            })
        );
    }

    /// (#1877 item 3) The scheduler's own honesty contract: when a producer
    /// genuinely does not know a step's item counts (every
    /// `crate::scheduler::run_step_graph`-produced record), the fields must
    /// be OMITTED, never written as `0`. A reader that sees `items_in`
    /// absent knows "not measured here"; a reader that saw `"items_in":0`
    /// would read it as "measured — zero items," which is false. **Proved
    /// failing first**: temporarily changing `items_in`/`items_out` back to
    /// `#[serde(default)]` without `skip_serializing_if` made this test fail
    /// with `"items_in":null` present in the JSON (still not `0`, but not
    /// absent either) — restored before committing. This test also locks
    /// the decision in place: a later change cannot quietly turn `None`
    /// into a serialized `0` without this test catching it.
    #[test]
    fn step_record_omits_item_counts_when_unknown_never_a_lying_zero() {
        let step = StepRecord {
            step_id: "dispatch.internal-step".into(),
            kind: "dispatch.internal".into(),
            items_in: None,
            items_out: None,
            wall_ms: 4_200,
        };
        let value = serde_json::to_value(&step).unwrap();
        assert_eq!(
            value,
            serde_json::json!({
                "step_id": "dispatch.internal-step",
                "kind": "dispatch.internal",
                "wall_ms": 4200
            }),
            "an unknown item count must be ABSENT from the JSON, not present as 0 or null"
        );
        assert!(value.get("items_in").is_none(), "items_in must not appear at all when unknown");
        assert!(value.get("items_out").is_none(), "items_out must not appear at all when unknown");
    }

    fn staffing(name: &str, model: &str, k: u32) -> ResolvedSeatStaffing {
        ResolvedSeatStaffing {
            name: name.to_string(),
            role_id: Some(format!("review-{name}")),
            pm: ProfileModel { id: model.to_string(), n_ctx: Some(32_000), ..Default::default() },
            k,
            passes: 2,
            max_tokens: None,
            selector: None,
            provenance: None,
        }
    }

    /// [`staffing_snapshot`] end to end: two probe seats + a judge + no
    /// verify seat, asserting the full JSON shape — including that
    /// `verify` and `request_changes` (false) are both absent, per their
    /// `skip_serializing_if`. **Proved failing first**: temporarily
    /// changing `staffing_snapshot`'s `verify: verify.map(one)` to always
    /// wrap `Some(one(judge))` (simulating a bug that always populates
    /// `verify`) made this test fail with an unexpected `"verify"` key
    /// present — restored before committing.
    #[test]
    fn staffing_snapshot_serializes_with_the_pre_move_shape() {
        let probes = vec![staffing("review-probe-high", "probe-model", 1)];
        let judge = staffing("review-judge", "judge-model", 1);

        let snap = staffing_snapshot(&probes, &judge, None, false);

        assert_eq!(
            serde_json::to_value(&snap).unwrap(),
            serde_json::json!({
                "probes": [{
                    "name": "review-probe-high",
                    "role_id": "review-review-probe-high",
                    "model": seat_identifier(&probes[0].pm),
                    "k": 1,
                    "passes": 2,
                    "n_ctx": 32000
                }],
                "judge": {
                    "name": "review-judge",
                    "role_id": "review-review-judge",
                    "model": seat_identifier(&judge.pm),
                    "k": 1,
                    "passes": 2,
                    "n_ctx": 32000
                }
            }),
            "no verify seat and request_changes=false must both be absent from the JSON"
        );
    }

    /// Same shape, but with a verify seat AND `request_changes: true` —
    /// pinning that BOTH optional fields serialize when present, the
    /// complement of the test above.
    #[test]
    fn staffing_snapshot_with_verify_and_request_changes_serializes_both() {
        let probes = vec![staffing("review-probe-high", "probe-model", 1)];
        let judge = staffing("review-judge", "judge-model", 1);
        let verify = staffing("review-verify", "verify-model", 1);

        let snap = staffing_snapshot(&probes, &judge, Some(&verify), true);

        let value = serde_json::to_value(&snap).unwrap();
        assert_eq!(value["request_changes"], serde_json::json!(true));
        assert_eq!(value["verify"]["name"], serde_json::json!("review-verify"));
        assert_eq!(value["verify"]["model"], serde_json::json!(seat_identifier(&verify.pm)));
    }
}
