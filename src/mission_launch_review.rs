//! `darkmux mission launch review` — the review-pipeline launcher (#1284
//! Packet 4b, the clean verb break). Retires `darkmux pr-review run`
//! (formerly `src/pr_review.rs::{cmd_run, run_dispatch, RunOpts, ...}`,
//! deleted in this packet) as the review dispatch entry point. The
//! top-level `pr-review` CLI verb itself (including `pr-review render`)
//! retired in #1426; the render IMPLEMENTATION survives in
//! `src/pr_review.rs` (`synthesize_review`/`emit_rendered`, envelope ->
//! PR payload), consumed by this launcher's own emit path.
//!
//! **Why review gets its own launcher instead of falling through
//! `mission_launch::launch`'s generic `mission_config::interpret` +
//! `crew::scheduler::run_step_graph` path** (the path `coder-phase` uses):
//! `build_review_graph`/`run_review_graph`
//! (`crates/darkmux-lab/src/lab/review.rs`) already own real, working,
//! tested cross-step behavior the generic scheduler call has no seam for —
//! a remote-token bucket shared across sibling probe steps, host telemetry
//! sampling (the "no blind runs" doctrine), and a post-run merge of probe
//! accumulators into the final envelope. `CLAUDE.md`'s StepKind tiering
//! section audits this exact pipeline and calls it a genuine non-collapse:
//! forcing it through the generic scheduler call would be "a feature change
//! wearing a tiering fix's clothes," not a real simplification. So this
//! module is a NEW CALLER of the SAME driver `pr_review.rs::run_dispatch`
//! used to own, wired through the same generic run-minting primitives
//! (`mission_launch::mint_run_id`, `mission_launch::
//! ensure_mission_and_phases_with_provenance`) every other config-launched
//! mission uses (#1503: a run id is minted fresh per launch, never derived
//! from `case_id` — a re-run of the same case is a DIFFERENT run, grouped
//! via `Mission.spec`, never a reopen of the prior one) —
//! `build_review_graph` still calls `mission_config::interpret` itself
//! internally (#1512: no expansion collection needed anymore — the probe
//! stage is explicit static tasks in the document — but the interpret call
//! still lives there since that's where the interpreted graph gets claimed
//! against the resolved crew's staffings), so no double interpretation
//! happens.
//!
//! **Gate semantics.** Review has no operator sign-off gate — unlike
//! `coder-phase`, there is nothing for `mission finalize`/`mission abort` to
//! finish. The mission/phase envelope finalizes GENERICALLY at the end of
//! every dispatching run via `crew::envelope::finalize_mission`
//! (Clean/Degraded -> Complete, Degenerate/Error -> Abandoned), matching
//! every other gate-less config-launched graph.
//!
//! **Flag -> input mapping** (old `pr-review run` CLI flag -> new
//! `--param key=value`, declared in `templates/builtin/mission-configs/
//! review.json`'s `inputs` block):
//!
//! | old flag              | new input        |
//! |------------------------|-------------------|
//! | `--github`              | `github`          |
//! | `--head-sha`             | `head_sha`        |
//! | `--worktree`             | `worktree`        |
//! | `--diff`                 | `diff_file`       |
//! | `--intent-file`          | `intent_file`     |
//! | `--crew`                 | `crew`            |
//! | `--mode`                 | `mode`            |
//! | `--k`                    | `k`               |
//! | `--envelope-out`         | `envelope_out`    |
//! | `--emit`                 | `emit`            |
//! | `--timeout`              | (`mission launch`'s own `--timeout` flag — NOT byte-identical at the default level: this launcher resolves an omitted `--timeout` to 3600, the retired CLI's own per-call default, while the generic coder-phase path resolves 600. #1284 Packet 4b review gate, must-fix 1.) |
//! | `--profiles-file`        | `profiles`        |
//! | `--attribution`          | `attribution`     |
//! | `--bundler`              | `bundler`         |
//! | `--charges-file`         | `charges_file`    |
//! | `--from-envelope`        | `from_envelope`   |
//!
//! `case_id` is a NEW optional input (derived from `worktree`/`github` +
//! `head_sha` when absent, matching the retired CLI's own case-label
//! derivation exactly) — never required from the operator.
//!
//! **Scope preserved from the retired CLI, deliberately unchanged:**
//! - `from_envelope` is synthesis-only: no crew/bundling/dispatch, and no
//!   mission is minted (there is no run to record) — just
//!   `pr_review::synthesize_review` over the saved envelope.
//! - `charges_file` (re-judge) also mints NO mission — the retired CLI
//!   never wrapped that path in a Mission/Phase either, so this preserves
//!   exact prior behavior rather than expanding scope.
//! - The full dispatch path (bundle -> probe -> dedup -> judge -> verify ->
//!   synthesis) mints + finalizes a mission with the SAME three phases
//!   `review.json` already declares (`investigate`/`adjudicate`/`report`).
//! - The process exit code is `0` on ANY produced output (`ReviewEnvelope`
//!   ok, Clean/Degraded/Degenerate all included) — a hard `Err` before
//!   synthesis is the only non-zero path. This matches the retired CLI's
//!   `cmd_run` exactly; CI-facing pass/fail already comes from the rendered
//!   payload's `mode` field, inspected by the workflow, not this exit code.

use crate::crew;
use crate::mission_launch;
use crate::pr_review;
use crate::launch_guard::{self, LaunchFinalizeGuard};
use anyhow::{anyhow, bail, Context, Result};
use crew::mission_config::MissionConfig;
use darkmux_crew::dispatch::build_dispatch_record_with_payload;
use darkmux_crew::single_shot::{
    single_shot_chat, single_shot_chat_hosted, HostedSingleShotRequest, SingleShotReply,
    SingleShotRequest,
};
use darkmux_lab::lab::bundle::{build_bundles, external_bundles, FileSource};
use darkmux_lab::lab::review::{
    build_review_graph_from_config, bundle_inputs_from_set, fingerprint, review_mission_outcome, run_judge_only,
    run_review_graph, seat_identifier, staffing_snapshot, BundleBuildSpec, BundleInput, BundleSourceSpec, ChatCall,
    ExecMode, LmsCycler, ProbeFlag, ReviewEmitter, ReviewEnvelope, ReviewInputs, ReviewStepContext,
};
use darkmux_crew::resourcing::{resolve_review_roles, ResolvedReviewRoles, ResolvedSeatStaffing, ReviewRoleStaffing};
// (#1959) The review's optional `workspace` input.
use darkmux_crew::workspace_spec::WorkspaceSpec;
use darkmux_profiles::profiles::load_registry;
use darkmux_profiles::swap;
use darkmux_types::dispatch_liveness::{liveness, liveness_case, liveness_detail};
use darkmux_types::style;
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

// ── input accessors ─────────────────────────────────────────────────────

fn str_input<'a>(collected: &'a BTreeMap<String, Value>, key: &str) -> Option<&'a str> {
    collected.get(key).and_then(Value::as_str)
}

fn path_input(collected: &BTreeMap<String, Value>, key: &str) -> Option<PathBuf> {
    str_input(collected, key).map(PathBuf::from)
}

/// (#1475 packet 3, #1512, #1513 review) Every role id the "review" mission
/// config declares anywhere (whatever task carries a `role_id` — probe,
/// judge, or verify alike) — the FULL set an operator may bind per run via
/// `--param <role>=<profile>`. Fully generic: no Rust-side enumeration of
/// "the probe roles" or "the judge/verify roles" — just "every `role_id`
/// this document names," read straight off it. Declared as inputs in
/// `review.json` so the surface is self-documenting.
fn declared_role_ids(config: &MissionConfig) -> Vec<String> {
    let mut ids: Vec<String> =
        config.phases.iter().flat_map(|p| p.tasks.iter()).filter_map(|t| t.role_id.clone()).collect();
    ids.sort();
    ids.dedup();
    ids
}

/// (#1475 packet 3, #1512, #1513 review) Collect the per-run role→profile
/// launch overrides — one `--param <role>=<profile>` per review role the
/// operator wants to rebind for this run without editing `config.json`.
/// Only roles the document actually declares are read (`declared_role_ids`);
/// a blank value is ignored. Fed to [`resolve_review_roles`], where it wins
/// over the `role_profiles` map and `default_profile`.
fn collect_role_overrides(
    collected: &BTreeMap<String, Value>,
    role_ids: &[String],
) -> BTreeMap<String, String> {
    let mut overrides = BTreeMap::new();
    for role in role_ids {
        if let Some(profile) = str_input(collected, role).map(str::trim).filter(|s| !s.is_empty()) {
            overrides.insert(role.clone(), profile.to_string());
        }
    }
    overrides
}

/// (#1426 ship-2) Parse a boolean `--param key=true` — truthy on
/// `true`/`1`/`yes`/`on` (case-insensitive), a real JSON `true`, or a nonzero
/// number. Absent/anything else => `false`.
fn bool_input(collected: &BTreeMap<String, Value>, key: &str) -> bool {
    match collected.get(key) {
        Some(Value::Bool(b)) => *b,
        Some(Value::String(s)) => {
            matches!(s.trim().to_ascii_lowercase().as_str(), "true" | "1" | "yes" | "on")
        }
        Some(Value::Number(n)) => n.as_u64().map(|v| v != 0).unwrap_or(false),
        _ => false,
    }
}

/// Tolerates both a real JSON number (an `--input <file>`) and a numeric
/// string (a `--param k=5`, which always arrives as a `Value::String`).
fn u32_input(collected: &BTreeMap<String, Value>, key: &str) -> Result<Option<u32>> {
    match collected.get(key) {
        None => Ok(None),
        Some(v) => {
            if let Some(n) = v.as_u64() {
                return Ok(Some(n as u32));
            }
            if let Some(s) = v.as_str() {
                return s
                    .trim()
                    .parse::<u32>()
                    .map(Some)
                    .map_err(|_| anyhow!("input `{key}` must be a non-negative integer (got \"{s}\")"));
            }
            bail!("input `{key}` must be a number or a numeric string")
        }
    }
}

/// The dispatch case handle: `owner/repo@sha` for a GitHub-API source, else
/// the worktree path, else `local` — an explicit `case_id` input always
/// wins. Used both as the review's `case_id` and the #1311 liveness case
/// field, so the floor trail and the flow records line up (mirrors the
/// retired CLI's own `case_label`).
fn derive_case_id(collected: &BTreeMap<String, Value>) -> String {
    if let Some(id) = str_input(collected, "case_id") {
        return id.to_string();
    }
    match (str_input(collected, "github"), str_input(collected, "head_sha")) {
        (Some(repo), Some(sha)) => format!("{repo}@{sha}"),
        _ => path_input(collected, "worktree")
            .map(|w| w.display().to_string())
            .unwrap_or_else(|| "local".to_string()),
    }
}

/// The worktree/GithubApi source, or a loud named error — never a silent
/// "which source?" guess (mirrors the retired CLI's `resolve_source`, which
/// clap used to enforce structurally; this module has no clap, so the
/// mutual-exclusion/`requires` rules are enforced here instead).
fn resolve_source(collected: &BTreeMap<String, Value>) -> Result<FileSource> {
    let worktree = path_input(collected, "worktree");
    let github = str_input(collected, "github");
    let head_sha = str_input(collected, "head_sha");
    match (&worktree, github, head_sha) {
        (Some(w), None, sha) => {
            // (C6, #1284 Packet 4b review gate) The retired clap surface
            // rejected `--head-sha` without `--github` structurally; here
            // a `head_sha` alongside `worktree` has nothing to shape —
            // surface it loud rather than silently ignoring (operator
            // sovereignty), but a warning is enough (no hard error).
            if sha.is_some() {
                eprintln!(
                    "mission launch review: input `head_sha` ignored with `worktree` \
                     (a local-checkout source reads the tree, not a commit SHA — \
                     `head_sha` only pairs with `github`)"
                );
            }
            Ok(FileSource::worktree(w))
        }
        (None, Some(repo), Some(sha)) => Ok(FileSource::github_api(repo, sha)),
        (None, Some(_), None) => bail!("mission launch review: input `github` requires `head_sha`"),
        (None, None, _) => bail!(
            "mission launch review: pass either --param worktree=<dir> or --param \
             github=<owner/repo> --param head_sha=<sha> to source the bundler (or --param \
             from_envelope=<path> for synthesis-only, which needs neither)"
        ),
        (Some(_), Some(_), _) => {
            bail!("mission launch review: `worktree` and `github` are mutually exclusive")
        }
    }
}

fn parse_exec_mode(mode: &str) -> Result<ExecMode> {
    match mode.trim().to_ascii_lowercase().as_str() {
        "sequential" => Ok(ExecMode::Sequential),
        "parallel" => Ok(ExecMode::Parallel),
        "auto" => Ok(ExecMode::Auto),
        other => bail!(
            "mission launch review: `mode` must be one of sequential|parallel|auto (got \"{other}\")"
        ),
    }
}

/// (#1247 Part 1) Production wiring of `review::ReviewEmitter` — writes
/// through the real darkmux-flow machinery, the same engagement-scoped
/// stream `dispatch`/`phase review` write through. This is the FLEET
/// sink: `mission launch review` drives ONE case per invocation (a real PR
/// review), so its run/step/ruling records belong on the operator's real
/// engagement stream — contrast `darkmux lab eval --funnel`'s
/// per-run-local JSONL sink (lab-vs-fleet scope boundary). Failure to
/// record is swallowed (`let _ =`) — same discipline as every other flow
/// emit site; a flow-record write failure must never abort a review.
///
/// `mission_id` (#1641): the launcher's own run identity, stamped in
/// AFTER `mission_launch::mint_run_id` mints it (`None` for the whole
/// `--charges-file` judge-only path, which mints no Mission at all — see
/// [`review_bookend_record`]'s doc). `emit` uses `get_or_insert`-style
/// semantics — a record that already carries a `mission_id` (the dispatch
/// bookends, which `review_bookend_record` stamps explicitly) is never
/// overwritten; this only backfills the records that don't, which is most
/// of them: `run_step_graph`'s own generic step-lifecycle bookends
/// (`crates/darkmux-crew/src/scheduler.rs`'s `step_lifecycle_record`) and
/// any `StepKind`'s own records built with a hardcoded `mission_id: None`
/// (e.g. `dispatch.map`'s per-item "step result", which streams through
/// this same emitter via `StepRunCtx::emit`). Before this fix, every one
/// of those records carried NO instance-scoped identity, so two review
/// missions launched from the SAME config (the `review.json` built-in) —
/// hence sharing the exact same CONFIG-scoped `session_id`/`handle`, e.g.
/// `task-review-probe-mid-task` / `review-probe-mid-step` — collided in
/// the mission-graph viewer: one mission's step absorbing another's token
/// counts, a dead step resurrected to "running" by the other mission's
/// heartbeat.
struct FleetFlowEmitter {
    mission_id: Option<String>,
}

impl ReviewEmitter for FleetFlowEmitter {
    fn emit(&mut self, mut record: darkmux_flow::FlowRecord) {
        // `get_or_insert`-style: only backfills an ABSENT `mission_id`,
        // never overwrites one a record already carries (see this struct's
        // doc). `self.mission_id` itself may be `None` (the
        // `--charges-file` judge-only path mints no Mission) — that's a
        // no-op here, matching pre-fix behavior exactly for that path.
        if record.mission_id.is_none() {
            record.mission_id = self.mission_id.clone();
        }
        let _ = darkmux_flow::record(record);
    }
}

/// Every distinct model this crew's resolved seats will dispatch,
/// darkmux-namespaced and deduped/sorted for a stable display string. A
/// review run is inherently multi-model (>=1 probe seat plus the judge
/// seat) — unlike a single-model `dispatch`, there's no one "the
/// model" for the dispatch bookend's `model` field, so this is the closest
/// honest equivalent: every model actually in play, comma-joined.
fn crew_model_summary(roles: &ResolvedReviewRoles) -> Option<String> {
    let one = |s: &ResolvedSeatStaffing| {
        if s.pm.is_remote() {
            s.pm.id.clone()
        } else {
            swap::namespaced_identifier(&s.pm)
        }
    };
    let mut ids: Vec<String> = roles
        .probes
        .iter()
        .chain(std::iter::once(&roles.judge))
        .chain(roles.verify.iter())
        .map(one)
        .collect();
    ids.sort();
    ids.dedup();
    if ids.is_empty() {
        None
    } else {
        Some(ids.join(", "))
    }
}

/// One review-run dispatch bookend record. Same builder + field shape
/// `dispatch` uses, with ONE deliberate difference: `source` is
/// overridden from the builder's hardcoded `"crew_dispatch"` to `"review"`
/// — the SAME provenance tag every sibling `step result` record the review
/// driver emits in this session carries.
///
/// `mission_id` (#1508 /runs gate finding, must-fix 1): threaded through
/// from the call site — `None` for the `--charges-file` judge-only path
/// (which mints no Mission at all, so there's genuinely nothing to join
/// to), `Some(&mission_id)` for a real review launch. Before this fix it
/// was hardcoded `None` unconditionally, matching neither the container
/// dispatch path (`dispatch_internal.rs`'s bookends DO stamp `mission_id`)
/// nor what a real review launch actually has available — every review
/// run's case-string-keyed bookend session was structurally unrecoverable
/// from its own Mission record's read side, which is exactly the gap
/// `/runs` (#1508 step 3) needs closed to avoid double-listing every review
/// run.
fn review_bookend_record(
    level: darkmux_flow::Level,
    action: &str,
    crew_name: &str,
    case_id: &str,
    model: Option<&str>,
    mission_id: Option<&str>,
    payload: serde_json::Value,
) -> darkmux_flow::FlowRecord {
    let mut record = build_dispatch_record_with_payload(
        level,
        action,
        crew_name,
        case_id,
        model,
        mission_id,
        None,
        Some(payload),
    );
    record.source = Some("review".to_string());
    record
}

/// Merge `extra`'s top-level keys into `base` (both expected to be JSON
/// objects) and return the result — `extra`'s keys win on collision.
fn merge_json_object(mut base: serde_json::Value, extra: serde_json::Value) -> serde_json::Value {
    if let (serde_json::Value::Object(extra), serde_json::Value::Object(base)) = (extra, &mut base) {
        base.extend(extra);
    }
    base
}

/// Adapts a [`ReviewEmitter`] to `darkmux_flow`'s generic `BookendSink`.
struct EmitterSink<'a>(&'a mut dyn ReviewEmitter);

impl darkmux_flow::BookendSink for EmitterSink<'_> {
    fn emit(&mut self, record: darkmux_flow::FlowRecord) {
        self.0.emit(record);
    }
}

impl ReviewEmitter for EmitterSink<'_> {
    fn emit(&mut self, record: darkmux_flow::FlowRecord) {
        self.0.emit(record);
    }
}

/// Emit `dispatch start`, run `f`, then emit the matching terminal record —
/// `dispatch complete` on `Ok`, `dispatch error` (carrying the real error
/// text) on `Err`. Same shape `dispatch` emits around every dispatch,
/// so a production review dispatch opens/closes the SAME liveness edge the
/// viewer's fleet/machine surfaces key on.
///
/// `mission_id`: see [`review_bookend_record`]'s doc — `None` for the
/// `--charges-file` judge-only path (mints no Mission), `Some(&mission_id)`
/// for a real review launch (minted just before this is called).
fn with_dispatch_bookends(
    emitter: &mut dyn ReviewEmitter,
    case_id: &str,
    crew_name: &str,
    model: Option<&str>,
    mission_id: Option<&str>,
    start_extra: serde_json::Value,
    f: impl FnOnce(&mut dyn ReviewEmitter) -> Result<ReviewEnvelope>,
) -> Result<ReviewEnvelope> {
    let mut sink = EmitterSink(emitter);
    let case_id_owned = case_id.to_string();
    let crew_owned = crew_name.to_string();
    let model_owned = model.map(str::to_string);
    let mission_id_owned = mission_id.map(str::to_string);
    let on_abort = move |_id: &str, _kind: &str| {
        review_bookend_record(
            darkmux_flow::Level::Error,
            "dispatch error",
            &crew_owned,
            &case_id_owned,
            model_owned.as_deref(),
            mission_id_owned.as_deref(),
            json!({
                "runtime": "review",
                "result_class": "error",
                "error": "review dispatch terminated before completion (early return or panic)",
            }),
        )
    };
    let mut guard = darkmux_flow::BookendGuard::new(&mut sink, on_abort);
    guard.open(
        "dispatch",
        "dispatch",
        review_bookend_record(
            darkmux_flow::Level::Info,
            "dispatch start",
            crew_name,
            case_id,
            model,
            mission_id,
            merge_json_object(json!({ "runtime": "review" }), start_extra),
        ),
    );

    let result = f(guard.sink_mut());

    match result {
        Ok(env) => {
            let mut payload = json!({
                "runtime": "review",
                "result_class": "ok",
                "confirmed": env.confirmed,
                "needs_check": env.needs_check,
                "archived": env.archived,
            });
            if let Some(reason) = &env.degenerate {
                payload["degenerate"] = json!(reason);
            } else if !env.warnings.is_empty() {
                // (#1876 QA follow-up, MUST FIX 1) A judge-coverage
                // shortfall (or any other #1260-style non-fatal warning)
                // sets `env.warnings` without setting `env.degenerate` —
                // exactly `review_result_to_mission_envelope`'s `Degraded`
                // condition, one function over. Before #1876 this same
                // incident shape always went through `degenerate`, which
                // the arm above already marks; #1876 moved it onto
                // `warnings` and this bookend never learned to read that
                // field, so the flow stream recorded an unqualified
                // `result_class: "ok"` for a run that silently dropped
                // flags. Mirrors the `"partial"` `result_class` precedent
                // in `step_kinds::builtins`'s map-step terminal.
                payload["result_class"] = json!("partial");
                payload["warnings"] = json!(env.warnings);
            }
            if let Some(member) = env.members.iter().find(|m| m.remote) {
                let remote_tokens: u64 =
                    env.members.iter().filter(|m| m.remote).map(|m| m.total_tokens).sum();
                let host = member.endpoint.as_deref().unwrap_or("remote");
                let endpoint_label = darkmux_flow::remote_route_label(host, &member.model);
                darkmux_flow::stamp_remote_classification(
                    &mut payload,
                    Some(&endpoint_label),
                    Some(remote_tokens),
                );
            }
            guard.close(
                "dispatch",
                review_bookend_record(
                    darkmux_flow::Level::Info,
                    "dispatch complete",
                    crew_name,
                    case_id,
                    model,
                    mission_id,
                    payload,
                ),
            );
            Ok(env)
        }
        Err(e) => {
            guard.close(
                "dispatch",
                review_bookend_record(
                    darkmux_flow::Level::Error,
                    "dispatch error",
                    crew_name,
                    case_id,
                    model,
                    mission_id,
                    json!({
                        "runtime": "review",
                        "result_class": "error",
                        "error": e.to_string(),
                    }),
                ),
            );
            Err(e)
        }
    }
}

/// NON-SECRET liveness detail for `config-resolved` — the resolved darkmux
/// home, machine id, and which flow sinks are enabled.
fn config_detail() -> String {
    let home = darkmux_types::paths::resolve(darkmux_types::paths::ResolveScope::Auto).root;
    format!(
        "home={} machine_id={} redis={} audit={}",
        home.display(),
        darkmux_types::config_access::machine_id().unwrap_or_else(|| "unknown".to_string()),
        if darkmux_types::config_access::redis_enabled() { "on" } else { "off" },
        if darkmux_types::config_access::audit_enabled() { "on" } else { "off" },
    )
}

/// (#1520) The remote ROUTE this review will run on, as the flow record's
/// `payload.endpoint` label — or `None` when every seat is local.
///
/// Judge-first, then the first remote probe, mirroring
/// `lab_staffing_role_model_route`'s precedence: the judge is the
/// load-bearing seat, so when seats disagree its route is the honest
/// headline. Uses `remote_route_label` — the same single-source shape the
/// TERMINAL bookend stamps (host + model, never the full URL, never auth)
/// so the two bookends agree byte-for-byte and the viewer's
/// start-or-complete resolution can never disagree with itself.
///
/// Pure and separate from the launcher so the precedence is testable
/// without standing up a dispatch.
fn crew_route_label(crew: &ResolvedReviewRoles) -> Option<String> {
    // (#1660) `.filter(|e| e.is_remote())` is NOT optional, and omitting it
    // was a bug: `is_remote()` is just `url.is_some()`, so a profile carrying
    // a bare `"endpoint": {}` (legal under the lenient schema) makes
    // `base_url()` fall back to the LOCAL LMStudio URL. Without the filter
    // this stamps a cloud label on a fully-local run — the exact inverted
    // misclassification `crew_route_label_is_none_when_every_seat_is_local`
    // claims to prevent, which its `endpoint: None` fixture structurally
    // could not see. Same filter the terminal side (`seat_endpoint_host`)
    // has always applied.
    let (ep, model) = crew
        .judge
        .pm
        .endpoint
        .as_ref()
        .filter(|e| e.is_remote())
        .map(|ep| (ep, crew.judge.pm.id.as_str()))
        .or_else(|| {
            crew.probes.iter().find_map(|s| {
                s.pm.endpoint.as_ref().filter(|e| e.is_remote()).map(|ep| (ep, s.pm.id.as_str()))
            })
        })?;
    let url = ep.base_url();
    let host = url.split("://").nth(1).and_then(|h| h.split('/').next()).unwrap_or("remote");
    Some(darkmux_flow::remote_route_label(host, model))
}

/// NON-SECRET liveness detail for `crew-resolved` — crew name, seat count,
/// and the distinct endpoint HOSTS of the remote seats. HOST ONLY: never
/// the full URL, never any credential.
fn crew_detail(roles: &ResolvedReviewRoles) -> String {
    let all = || roles.probes.iter().chain(std::iter::once(&roles.judge)).chain(roles.verify.iter());
    let seat_count = all().count();
    let mut hosts: Vec<String> = all()
        .filter(|s| s.pm.is_remote())
        .filter_map(|s| s.pm.endpoint.as_ref().map(|ep| ep.base_url()))
        .filter_map(|url| {
            url.split("://")
                .nth(1)
                .and_then(|s| s.split('/').next())
                .map(str::to_string)
        })
        .collect();
    hosts.sort();
    hosts.dedup();
    let hosts_str = if hosts.is_empty() { "(local-only)".to_string() } else { hosts.join(",") };
    format!("crew={} seats={seat_count} remote_hosts={hosts_str}", roles.distinct_profile_names())
}

/// (#1284 Packet 2; #1877 item 2) Map a review dispatch's
/// `Result<ReviewEnvelope>` onto the generalized
/// [`crew::envelope::MissionEnvelope`] contract.
///
/// **ONE source of truth (#1877 item 2):** `status` is no longer computed
/// here by its own if/else chain over `env.degenerate`/`env.warnings` — it
/// is [`crew::envelope::MissionOutcomeStatus::from_outcome`] applied to
/// [`darkmux_lab::lab::review::review_mission_outcome(env)`], via
/// [`crew::envelope::MissionEnvelope::from_outcome`]. That function is
/// where the `Degenerate`/`Degraded`/`Clean` predicate now lives (see its
/// own doc for why it's a DIFFERENT function from
/// `darkmux_lab::lab::review::review_outcome`, the PR-banner one) — this
/// function only reads the result:
///
/// - `Err` -> `Error` (a hard failure before any envelope was produced;
///   `Error` has no `RunOutcome` counterpart — there was no run to summarize).
/// - `Ok(env)` -> whatever `review_mission_outcome(env)` decides, mapped
///   through `MissionOutcomeStatus::from_outcome`. Reproduces the exact
///   status this function computed before #1877 for every input that
///   produced one then (pinned by this module's own tests): a benign or
///   unsupported-language `degenerate` still reads `Clean` (#1654/#1757 —
///   `Degenerate` is a retry signal, and a diff with nothing to review
///   produces the identical result on every repeat, so flagging it would
///   manufacture an unbounded retry loop); any other `degenerate` still
///   reads `Degenerate`; any non-empty `env.warnings` with no degenerate
///   still reads `Degraded`; otherwise `Clean`.
///
/// `reason`'s computation is UNCHANGED by this migration — it still reads
/// `env.degenerate`/`env.warnings` directly rather than `outcome`, because
/// its wording (`"review degenerate: {r}"`, present even on the neutral
/// `Clean` case so the "why zero findings" provenance survives) predates
/// `RunOutcome` and is prose review owns, not a mission-agnostic concept
/// worth generalizing in this PR.
///
/// The FULL `ReviewEnvelope` rides in `MissionEnvelope::payload`.
fn review_result_to_mission_envelope(
    mission_id: &str,
    phase_ids: &[&str],
    result: &Result<ReviewEnvelope>,
) -> crew::envelope::MissionEnvelope {
    use crew::envelope::{MissionEnvelope, MissionOutcomeStatus, RemoteBudgetRow};

    match result {
        Ok(env) => {
            let outcome = review_mission_outcome(env);
            let status = MissionOutcomeStatus::from_outcome(&outcome);
            let reason = env
                .degenerate
                .as_deref()
                .map(|r| format!("review degenerate: {r}"))
                .or_else(|| (status == MissionOutcomeStatus::Degraded).then(|| env.warnings.join("; ")));
            let mut envelope = MissionEnvelope::from_outcome(mission_id, outcome, phase_ids);
            envelope.reason = reason;
            // (#1877 QA ALSO FIX 2) INVARIANT, not enforced by any type:
            // when `envelope.outcome` is `Some(RunOutcome::Partial {
            // reasons })`, `reasons` is `env.warnings.clone()` — the SAME
            // `Vec` `review_mission_outcome` built `outcome` from (see its
            // own doc). So `envelope.warnings` here duplicates
            // `envelope.outcome`'s `reasons` verbatim in that case; every
            // warning rides `envelope.json` twice. Deliberate, not
            // accidental: `warnings` is the field every EXISTING consumer
            // reads (the PR banner via `review_outcome`, the mission board,
            // `darkmux-serve/src/runs.rs` — see `envelope.rs`'s module doc
            // on why those consumers stay on `status`/`warnings`, never
            // `outcome`, until a separately-reviewed change), so it can't
            // be dropped here without breaking them; `outcome.reasons` is
            // kept as the new typed value #1877 adds. A future cleanup that
            // wants to stop the duplication has to migrate every one of
            // those consumers onto `outcome` first, not just delete this
            // line — `judge_gate_outcome_keeps_review_outcome_and_review_
            // mission_outcome_in_sync` (darkmux-lab's review_tests.rs) and
            // `a_partial_coverage_review_round_trips_as_a_typed_partial_
            // outcome_with_real_numbers` (below) both pin that the two
            // copies agree today.
            envelope.warnings = env.warnings.clone();
            envelope.remote_budgets = env
                .remote_budgets
                .iter()
                .map(|r| RemoteBudgetRow {
                    stage: r.stage.clone(),
                    max_tokens: r.max_tokens,
                    used_tokens: r.used_tokens,
                    exhausted: r.exhausted,
                    skipped_calls: r.skipped_calls,
                })
                .collect();
            envelope.payload = serde_json::to_value(env).unwrap_or(serde_json::Value::Null);
            envelope
        }
        Err(e) => {
            let mut envelope =
                crew::envelope::MissionEnvelope::new(mission_id, crew::envelope::MissionOutcomeStatus::Error, phase_ids);
            envelope.reason = Some(format!("review errored: {e:#}"));
            envelope
        }
    }
}

/// Finalize a review-launched Mission's three Phases once the dispatch is
/// done, so `darkmux mission status`/the viewer's mission lens never shows
/// a permanently "active, N running" review. Review has no operator
/// sign-off gate (unlike `coder-phase`), so this always drives the mission
/// to a terminal status — see [`review_result_to_mission_envelope`]'s doc
/// for the status decision.
///
/// `pub(crate)` (#2124, generalized #2131): [`run_dispatch`]'s
/// `crate::launch_guard::LaunchFinalizeGuard` calls this directly (both its
/// normal-path `close()`'s writer and its abort-path `Drop`'s writer) so
/// there is exactly ONE place that ever writes a review mission's terminal
/// record, whether the run finished normally, errored, panicked, or was
/// cut short by SIGTERM/SIGINT/SIGHUP.
pub(crate) fn finalize_review_mission(mission_id: &str, phase_ids: &[&str], result: &Result<ReviewEnvelope>) {
    let envelope = review_result_to_mission_envelope(mission_id, phase_ids, result);
    crew::envelope::finalize_mission(&envelope);
}

/// The review launcher's per-dispatch timeout default when `--timeout` is
/// omitted — the retired `pr-review run`'s own `--timeout` default,
/// preserved verbatim (#1284 Packet 4b review gate, must-fix 1: the
/// generic `mission launch` default of 600 would have silently cut the
/// per-call ceiling 6x, degrading long judge passes that used to pass).
const REVIEW_DEFAULT_TIMEOUT_SECONDS: u32 = 3600;

/// `darkmux mission launch review` entry point (dispatched from
/// `mission_launch::launch` when `config.id == "review"`). Returns the
/// process exit code — `0` on any produced review output (Clean/Degraded/
/// Degenerate alike; CI-facing pass/fail comes from the rendered payload's
/// `mode` field, not this code), propagating a hard `Err` (via `?`) for
/// anything that fails before an envelope was ever produced.
///
/// The `#[1311]` liveness floor's `process-start` marker fires in
/// `mission_launch::launch` (before the config load's user-tier dir I/O —
/// C7 of the Packet 4b review gate), not here; this function picks the
/// trail up with `run_dispatch`'s own `config-resolved`/`crew-resolved`
/// markers (#1530: the `bundling-start`/`bundling-done` pair that used to
/// follow them now fires only on the `charges_file` side path — the graph
/// path's own bundling runs INSIDE the dispatch, observed instead through
/// `review-bundle-step`'s ordinary `step result` flow record) and closes it
/// with `synthesis`/`done` below.
pub(crate) fn launch(
    config: &MissionConfig,
    input_file: Option<&Path>,
    params: &[String],
    timeout_seconds: Option<u32>,
    // (#1562) The launched config's tier, recorded onto `MissionSpec.origin`
    // at mint — a user-tier review variant (`review-lean` in the operator's
    // own dir) is named work; the shipped `review` is a run instance.
    spec_origin: crew::types::MissionSpecOrigin,
) -> Result<i32> {
    let timeout_seconds = timeout_seconds.unwrap_or(REVIEW_DEFAULT_TIMEOUT_SECONDS);
    let collected = mission_launch::collect_inputs(input_file, params)?;

    let diff_file = path_input(&collected, "diff_file").ok_or_else(|| {
        anyhow!(
            "mission launch review: missing required input `diff_file` (path to the PR unified \
             diff) — pass --param diff_file=<path>"
        )
    })?;
    let diff_text = std::fs::read_to_string(&diff_file)
        .with_context(|| format!("reading diff_file {}", diff_file.display()))?;

    // (#1959) `--dry-run`: resolve inputs/source/crew (cheap, no mint, no
    // dispatch), print what would run, return without minting a Mission or
    // running the graph. Checked BEFORE `from_envelope` handling too — a
    // dry run of a synthesis-only replay is a contradiction the operator
    // should never hit silently (it would just print the SAME resolved
    // inputs and skip the file read, which is more confusing than useful),
    // so `--dry-run --from-envelope` still routes through the normal
    // report — the bundle count line honestly says "not computed" if the
    // resolved source is a GitHub one either way.
    if bool_input(&collected, "dry_run") {
        return review_dry_run_report(config, &collected, &diff_file, &diff_text);
    }

    // (#2112 review, second pass finding A) `review` routes through this
    // DEDICATED launcher from `mission_launch::launch` BEFORE that
    // function's own `#2112` pre-flight call — so without a call here, a
    // review pipeline (long-running, many real dispatches) got NEITHER
    // the thermal-refusal pre-flight NOR the held sleep assertion, the
    // exact class of long unattended run this exists to protect. Placed
    // after the `--dry-run` short-circuit above for the same reason
    // `mission_launch::launch` moved its own call there: a dry run must
    // never be refused on thermal grounds. See `src/preflight.rs` and
    // `crates/darkmux-crew/src/sleep_assertion.rs`.
    crate::preflight::check_power_posture(params)?;
    let _sleep_assertion = darkmux_crew::sleep_assertion::SleepAssertion::hold(&format!("darkmux mission review {}", config.id));

    let envelope_out = path_input(&collected, "envelope_out");
    let emit = path_input(&collected, "emit");
    let attribution = str_input(&collected, "attribution").map(str::to_string);

    let env: ReviewEnvelope = match path_input(&collected, "from_envelope") {
        Some(path) => {
            // Synthesis-only: dispatch-shaping inputs have nothing to
            // shape. Warn (don't error) — operator sovereignty: surface,
            // never silently ignore.
            let mut ignored: Vec<String> = Vec::new();
            // The static dispatch-shaping inputs never need the config —
            // check them unconditionally.
            for key in ["worktree", "github", "head_sha", "bundler", "passes"] {
                if collected.contains_key(key) {
                    ignored.push(key.to_string());
                }
            }
            // The per-run role→profile overrides (#1475 packet 3, #1512,
            // #1513 review) ALSO shape staffing a synthesis-only launch
            // never resolves — but discovering which role ids the document
            // declares needs the "review" mission config, and that load is
            // BEST-EFFORT here (#1513 review C4): a malformed USER-TIER
            // `~/.darkmux/mission-configs/review.json` must not break a
            // replay that otherwise needs no config at all — warn and
            // continue with whatever the static check above already found,
            // rather than hard-failing on a check that only ever produces a
            // warning anyway.
            // (#1538 follow-up) Read the role ids off the LAUNCHED document,
            // which `launch` already holds — not a hardcoded `load("review")`.
            // Same correctness reason as `run_dispatch`'s own switch (a
            // `review-lean` replay must be told about ITS ignored overrides,
            // not the built-in's), and it happens to delete the best-effort
            // load + its failure branch entirely: there is no longer a second
            // file read here that could fail, so the #1513-C4 hazard this
            // block was written to survive no longer exists.
            for key in declared_role_ids(config) {
                if collected.contains_key(&key) {
                    ignored.push(key);
                }
            }
            if !ignored.is_empty() {
                eprintln!(
                    "mission launch review: {} ignored with `from_envelope` (synthesis-only, no \
                     dispatch)",
                    ignored.join(", ")
                );
            }
            let raw = std::fs::read_to_string(&path)
                .with_context(|| format!("reading from_envelope {}", path.display()))?;
            serde_json::from_str(&raw).with_context(|| {
                format!("parsing from_envelope {} as a review envelope", path.display())
            })?
        }
        None => run_dispatch(config, &collected, &diff_file, &diff_text, timeout_seconds, spec_origin)?,
    };

    if let Some(path) = &envelope_out {
        let pretty = serde_json::to_string_pretty(&env).context("serializing the review envelope")?;
        std::fs::write(path, pretty)
            .with_context(|| format!("writing envelope_out {}", path.display()))?;
    }

    // (#1311) Dispatch is done (or was synthesis-only); the model work is
    // behind us. `synthesis` then `done` bracket the pure-CPU render so a
    // wedge in the (local) synthesis code is still visible.
    liveness("synthesis");
    let rendered = pr_review::synthesize_review(&env, &diff_text, attribution.as_deref());
    let code = pr_review::emit_rendered(&rendered, emit.as_deref())?;
    liveness("done");
    Ok(code)
}

/// Everything but `from_envelope`: resolve the source + crew, then dispatch
/// either the review graph (`build_review_graph` + `run_review_graph`) or
/// `run_judge_only` (`charges_file` — re-judge a saved flag list without
/// re-running the probe).
///
/// (#1530: "no graph does data-producing work before graph execution — every
/// graph runs the same way") Bundling itself (resolving the diff source's
/// file content and running the built-in or external bundler) is NO LONGER
/// done here for the graph path — it used to run in this function's own
/// prelude, so nothing upstream of the graph (a coder step, say) could ever
/// feed review: by the time any step ran, the bundles were already required
/// launch input. It now runs INSIDE the graph, as `review-bundle-step`'s own
/// `run_streaming` (`ReviewBundleStepKind`, `crates/darkmux-lab/src/lab/
/// review.rs`) — this function only resolves + VALIDATES the diff source
/// (`resolve_source`, pure — no I/O) and stamps that already-known data into
/// a [`BundleBuildSpec`] the graph builder writes onto the bundle step's
/// `Step.config`.
///
/// (#1959) `--dry-run`'s report: resolve source + crew (cheap — no mint,
/// no dispatch, no graph), print what would run, return 0. The bundle
/// count is computed only for a `FileSource::Worktree` source (a local
/// `build_bundles` pass — CPU-only, no network); a `GithubApi` source
/// would need one API fetch per changed file, which is NOT cheap, so the
/// report says so instead of paying that cost just to print a number.
fn review_dry_run_report(
    config: &MissionConfig,
    collected: &BTreeMap<String, Value>,
    diff_file: &Path,
    diff_text: &str,
) -> Result<i32> {
    let case = derive_case_id(collected);
    let source = resolve_source(collected)?;
    let loaded = load_registry(str_input(collected, "profiles"))?;
    let role_ids = declared_role_ids(config);
    let overrides = collect_role_overrides(collected, &role_ids);
    let resourcing = ReviewRoleStaffing {
        passes: u32_input(collected, "passes")?,
        request_changes: bool_input(collected, "request_changes"),
    };
    let crew = resolve_review_roles(&loaded.registry, config, &resourcing, &|role| {
        if let Some(p) = overrides.get(role).map(|s| s.trim()).filter(|s| !s.is_empty()) {
            darkmux_profiles::profiles::RoleBinding::Overridden(p.to_string())
        } else if let Some(p) = darkmux_types::config_access::role_profile(role) {
            darkmux_profiles::profiles::RoleBinding::Mapped(p)
        } else {
            darkmux_profiles::profiles::RoleBinding::Unmapped
        }
    })?;

    println!("darkmux mission launch review --dry-run — nothing minted");
    println!("  case_id = {case}");
    println!("  diff_file = {}", diff_file.display());
    match &source {
        FileSource::Worktree(p) => println!("  source = worktree {}", p.display()),
        FileSource::GithubApi { repo, head_sha, .. } => println!("  source = github {repo}@{head_sha}"),
    }
    println!("  crew = {}", crew_detail(&crew));

    match &source {
        FileSource::Worktree(_) => {
            let bundle_set = build_bundles(&source, diff_text)?;
            println!(
                "  bundles = {} ({} file(s) considered, {} skipped)",
                bundle_set.bundles.len(),
                bundle_set.skip.files_considered,
                bundle_set.skip.files_skipped.len()
            );
        }
        FileSource::GithubApi { .. } => {
            println!(
                "  bundles = (not computed in dry-run — a GitHub API source needs a network                  fetch per changed file)"
            );
        }
    }
    Ok(0)
}

/// The `charges_file` (re-judge) side path is the ONE exception, and
/// deliberately so: it mints no Mission and runs no graph at all (see this
/// launcher's own module doc), so it is OUT OF SCOPE for the "no data
/// production before graph execution" invariant — a graph is exactly what
/// it isn't. It still bundles eagerly, right here, exactly as before.
fn run_dispatch(
    config: &MissionConfig,
    collected: &BTreeMap<String, Value>,
    diff_file: &Path,
    diff_text: &str,
    timeout_seconds: u32,
    spec_origin: crew::types::MissionSpecOrigin,
) -> Result<ReviewEnvelope> {
    // (#2124, generalized #2131) Installed unconditionally, ahead of
    // everything else this function does — including the `charges_file`
    // re-judge side path, which spawns remote `curl` children of its own
    // even though it mints no Mission. Only installs signal handlers
    // (never touches this process's own group — see
    // `darkmux_types::child_registry`'s module doc for why an earlier
    // version did and was proven wrong by a pty test): the re-judge path
    // has no `LaunchFinalizeGuard` and no mission to finalize, so it still
    // accepts the pre-#2124 signal behavior — see this launcher's own
    // module doc on why that path is out of scope for this fix.
    launch_guard::arm();
    let case = derive_case_id(collected);

    let source = resolve_source(collected)?;
    liveness_detail("config-resolved", &case, &config_detail());
    let loaded = load_registry(str_input(collected, "profiles"))?;
    // (#1512, #1513 review) The review's roles are discovered + resolved in
    // ONE generic pass, `resolve_review_roles` — never a Rust-side
    // enumeration of "the probe roles". Loading the config here (ahead of
    // `build_review_graph`'s own later `load` + `interpret`) is a second,
    // cheap parse of the same document; `mission_config::load` is pure JSON
    // I/O, and this call site needs only the RAW (pre-interpret) task shape
    // `resolve_review_roles` classifies by step kind, while
    // `build_review_graph` separately needs the INTERPRETED graph to stamp
    // dispatch config — two different representations of the same
    // document, not a second interpretation.
    // (#1538 follow-up) Use the LAUNCHED document, not a hardcoded
    // `load("review")`. Routing became structural in #1538 — any config whose
    // graph names the review kinds reaches this launcher — but both consumers
    // downstream still resolved the built-in by id, so a named variant
    // (`review-lean`) routed here and then silently executed the BUILT-IN
    // three-probe graph: its own probe tasks ignored, its `--param
    // <role>=<profile>` overrides never looked up (and never warned about),
    // and `config-snapshot.json` recording a document that did not run. Before
    // #1538 that launch failed LOUDLY on an unknown kind; the structural
    // routing turned a loud refusal into silent wrong execution, which is the
    // inversion operator sovereignty and contract 7 both forbid.
    let review_config_doc: &MissionConfig = config;
    // (#1475, #1512, #1513 review) Review staffing is the role→profile
    // rollup. Every review role (however many probe roles review.json
    // declares, plus review-judge, plus the optional review-verify)
    // resolves INDEPENDENTLY: a per-run launch override (`--param
    // <role>=<profile>`) wins, else the machine-local `role_profiles` map
    // in config.json, else `default_profile` (the fresh-user floor). A bare
    // `mission launch review` assembles the operator's heterogeneous crew
    // straight from the map; a `--param review-judge=<profile>` rebinds one
    // seat for this run without editing config.json. Only the non-model
    // knobs live in `ReviewRoleStaffing`: judge consensus DEPTH (`passes`)
    // and the blocking-vs-advisory render choice. The envelope snapshot
    // records what role→profile actually resolved AND from which tier
    // (operator sovereignty #44).
    let role_ids = declared_role_ids(review_config_doc);
    let overrides = collect_role_overrides(collected, &role_ids);
    let resourcing = ReviewRoleStaffing {
        // (#1266) `None` => the resolver's double-confirm default; validated
        // `>= 1` in the resolver.
        passes: u32_input(collected, "passes")?,
        request_changes: bool_input(collected, "request_changes"),
    };
    let crew = resolve_review_roles(&loaded.registry, review_config_doc, &resourcing, &|role| {
        // Precedence: launch override > role_profiles map > unmapped (default).
        if let Some(p) = overrides.get(role).map(|s| s.trim()).filter(|s| !s.is_empty()) {
            darkmux_profiles::profiles::RoleBinding::Overridden(p.to_string())
        } else if let Some(p) = darkmux_types::config_access::role_profile(role) {
            darkmux_profiles::profiles::RoleBinding::Mapped(p)
        } else {
            darkmux_profiles::profiles::RoleBinding::Unmapped
        }
    })?;
    liveness_detail("crew-resolved", &case, &crew_detail(&crew));

    let worktree = path_input(collected, "worktree");
    let bundler_cmd = str_input(collected, "bundler").map(str::to_string);

    // (#1530) Eager bundling survives ONLY for the `charges_file` re-judge
    // side path — it mints no Mission and runs no graph, so it is out of
    // scope for the "no data production before graph execution" invariant
    // (see this function's own doc). The graph path builds a
    // `BundleBuildSpec` instead (below) and lets `review-bundle-step`
    // resolve the real bundles at run time.
    let charges_bundles: Option<Vec<BundleInput>> = if path_input(collected, "charges_file").is_some() {
        liveness_case("bundling-start", &case);
        let bundle_set = match bundler_cmd.as_deref() {
            Some(cmd) => external_bundles(cmd, worktree.as_deref(), diff_file)?,
            None => build_bundles(&source, diff_text)?,
        };
        let bundles = bundle_inputs_from_set(&bundle_set, &source)?;
        liveness_detail("bundling-done", &case, &format!("bundles={}", bundles.len()));
        Some(bundles)
    } else {
        None
    };

    // (#1530) The bundle step's own `Step.config` source — everything
    // `ReviewBundleStepKind::run_streaming` needs to reconstruct a
    // `FileSource` and run the bundler at RUN TIME, carried as plain data
    // (see `BundleBuildSpec`'s own doc). Built unconditionally (cheap, no
    // I/O) even on the `charges_file` path, which never reads it — simpler
    // than threading an `Option` through `build_review_graph`'s signature
    // for a branch that never calls it.
    let bundle_spec = BundleBuildSpec {
        source: match &source {
            FileSource::Worktree(path) => BundleSourceSpec::Worktree { path: path.clone() },
            FileSource::GithubApi { repo, head_sha, .. } => {
                BundleSourceSpec::Github { repo: repo.clone(), head_sha: head_sha.clone() }
            }
        },
        bundler: bundler_cmd,
        diff_file: diff_file.to_path_buf(),
    };

    // (#2310 P1 review finding I4) The path is kept (not just the text) so
    // `ReviewStepContext::intent_file` below can hand it to `build_review_
    // graph_from_config`, which stamps the PATH onto `review-context-step`'s
    // config instead of the whole intent TEXT — the same shrink as
    // `diff_file` a few lines up. Reading it into `intent` here too is not
    // a second I/O pass for this graph path specifically: the same text is
    // also needed by the separate `--charges-file` re-judge side path
    // (`ReviewInputs::intent_body`, below), which never touches the graph
    // or `review-context-step` at all.
    let intent_file_path = path_input(collected, "intent_file");
    let intent = match &intent_file_path {
        Some(p) => std::fs::read_to_string(p).with_context(|| format!("reading intent_file {}", p.display()))?,
        None => String::new(),
    };

    // (#1959) Optional workspace scope: `resolve_bundles` (inside
    // `run_judge_only`) drops every bundle whose path fails the spec's
    // include/exclude. Loaded eagerly here — cheap (JSON parse +
    // validate, no git resolution) — so it's ready for the `charges_file`
    // branch's `ReviewInputs` below. NOT wired into the graph launch path
    // (`ReviewBundleStepKind`) yet — see `review.json`'s own doc for that
    // follow-up.
    let workspace_spec: Option<WorkspaceSpec> = match path_input(collected, "workspace") {
        Some(p) => {
            let (spec, warnings) =
                WorkspaceSpec::load(&p).with_context(|| format!("loading workspace spec {}", p.display()))?;
            for w in &warnings {
                eprintln!("{}", style::warn(w));
            }
            Some(spec)
        }
        None => None,
    };

    let mode_str = str_input(collected, "mode").unwrap_or("auto").to_string();
    let mode = parse_exec_mode(&mode_str)?;
    // (#1530) `bundles` is stamped onto the "dispatch start" payload only
    // when it's genuinely already known (the `charges_file` path, which
    // still bundles eagerly, above) — the graph path doesn't know the real
    // count until `review-bundle-step` runs, which is AFTER this record
    // fires, so it's honestly omitted rather than reporting a stale 0.
    let mut dispatch_start_extra = json!({ "exec_mode": mode_str });
    if let Some(bundles) = &charges_bundles {
        dispatch_start_extra["bundles"] = json!(bundles.len());
    }
    // (#1520) Stamp the route on the START record, not only the terminal.
    //
    // `payload.endpoint` is the ONLY field the viewer reads to classify a
    // session as cloud vs local. The review path stamped it exclusively on
    // `dispatch complete`, so #1518's viewer fix (resolve from start OR
    // complete) repaired every FINISHED remote review and left the live one
    // broken: an in-flight Azure review has no terminal record yet, so it
    // renders as a local LMStudio run — wrong route, plus a phantom
    // `model (lms)` residency — for its whole duration, which is exactly
    // when the operator is watching it.
    if let Some(label) = crew_route_label(&crew) {
        dispatch_start_extra["endpoint"] = json!(label);
    }

    // (#2310 P1) ONE call resolves the three system prompts (`role_prompt`),
    // the per-seat probe prompts, and the two `config_access` budget knobs
    // — exactly what used to be five separate inline blocks here. The SAME
    // function is what `review.context`'s step kind calls at run time (see
    // its own doc); this launcher call is the build-time twin
    // `build_review_graph_from_config` needs `ReviewStepContext` ready for
    // (residency/prompt/budget stamping onto probe/judge/verify step
    // configs) — `diff_override`/`intent_body_override` pass the text this
    // function ALREADY read (`diff_text`/`intent`, above) through rather
    // than re-reading either file a second time.
    let review_ctx = darkmux_lab::lab::review::resolve_review_context(
        &case,
        &crew,
        None,
        Some(diff_text),
        None,
        Some(&intent),
        "",
        timeout_seconds,
    )?;
    let probe_system = review_ctx.probe_system.clone();
    let judge_system = review_ctx.judge_system.clone();
    let verify_system = review_ctx.verify_system.clone();
    let remote_max_tokens_per_execution = review_ctx.remote_max_tokens_per_execution;
    // (#1876/#1877) The judge stage's remote-budget exhaustion policy —
    // resolved once inside `resolve_review_context`, same as the token
    // budget above, and passed through to both the `--charges-file`
    // (`ReviewInputs`) and the graph (`ReviewStepContext`) launch shapes
    // below.
    let judge_exhaustion_strict = review_ctx.judge_exhaustion_strict;

    let case_id_for_bookends = case.clone();
    let crew_name_for_bookends = crew.distinct_profile_names();
    let model_for_bookends = crew_model_summary(&crew);

    // (#1641) `mission_id` starts unset — `--charges-file` never mints a
    // Mission, so it stays `None` on that path. The real-launch branch
    // below sets it right after `mission_launch::mint_run_id` mints one,
    // before `emitter` is handed to `run_review_graph`/`with_dispatch_
    // bookends` — see [`FleetFlowEmitter`]'s doc.
    let mut emitter = FleetFlowEmitter { mission_id: None };

    liveness_case("flow-sinks-up", &case);
    liveness_detail(
        "dispatch-start",
        &case,
        &format!(
            "crew={crew_name_for_bookends} models={}",
            model_for_bookends.as_deref().unwrap_or("(none)")
        ),
    );

    let outcome: Result<ReviewEnvelope> = if let Some(charges_path) = path_input(collected, "charges_file") {
        let inputs = ReviewInputs {
            case_id: case.clone(),
            roles: &crew,
            intent_title: "",
            intent_body: &intent,
            diff: diff_text,
            mode,
            probe_system: &probe_system,
            judge_system: &judge_system,
            verify_system: &verify_system,
            bundles: Some(charges_bundles.expect(
                "charges_bundles is always Some when input `charges_file` is set (computed above)",
            )),
            remote_max_tokens_per_execution,
            judge_exhaustion_strict,
            workspace: workspace_spec.as_ref(),
            // (#1748) The same `FileSource` bundling used above — lets the
            // mechanical absence-claim backstop check a confirmed finding
            // against the whole file on the `--charges-file` re-judge path
            // too, not just the graph path.
            source: Some(&source),
        };
        let timeout = timeout_seconds;
        let mut chat = move |call: &ChatCall| -> Result<SingleShotReply> {
            match call.endpoint {
                Some(endpoint) => single_shot_chat_hosted(&HostedSingleShotRequest {
                    endpoint,
                    model: call.model,
                    system: call.system,
                    user: call.user,
                    max_tokens: call.max_tokens,
                    timeout_seconds: timeout,
                }),
                None => single_shot_chat(&SingleShotRequest {
                    base_url: None,
                    model: call.model,
                    system: call.system,
                    user: call.user,
                    temperature: call.temperature,
                    max_tokens: call.max_tokens,
                    timeout_seconds: timeout,
                }),
            }
        };
        let mut cycler = LmsCycler;
        let raw = std::fs::read_to_string(&charges_path)
            .with_context(|| format!("reading charges_file {}", charges_path.display()))?;
        let flags: Vec<ProbeFlag> = serde_json::from_str(&raw)
            .with_context(|| format!("parsing charges_file {} as a flag list", charges_path.display()))?;
        with_dispatch_bookends(
            &mut emitter,
            &case_id_for_bookends,
            &crew_name_for_bookends,
            model_for_bookends.as_deref(),
            // No Mission is minted on the `--charges-file` judge-only path —
            // there's genuinely nothing to join this dispatch's flow session
            // to, so `None` here is honest, not a gap (see
            // `review_bookend_record`'s doc).
            None,
            dispatch_start_extra,
            move |emitter| run_judge_only(flags, &inputs, &mut chat, &mut cycler, emitter),
        )
    } else {
        // (#1512, #1513 review) `crew` is already the validated, resolved
        // shape — no separate crew-validation step.
        let probes = crew.probes.clone();
        let judge = crew.judge.clone();
        let verify = crew.verify.clone();
        let judge_identifier = seat_identifier(&judge.pm);
        let request_changes = crew.request_changes;

        // (#1503) The run id is minted fresh — never derived from
        // `case_id`. A re-run of the same case (a new commit push, a
        // manual re-trigger) is a genuinely different run — AI work is
        // non-deterministic — so it gets its own id; `id_input`'s
        // fingerprint still groups same-case runs together via
        // `Mission.spec`, just as metadata rather than identity.
        //
        // (#1641) Minted BEFORE `ctx`/`emitter` below (moved up from after)
        // so both can carry it from construction: `ctx.mission_id` reaches
        // this pipeline's OWN directly-emitted records (`emit_review_step_
        // result` et al, which write straight to the global sink and can't
        // hold `emitter` — see `ReviewStepContext::mission_id`'s doc), and
        // `emitter.mission_id` backfills everything routed through
        // `run_step_graph`'s `emit` closure instead (see
        // `FleetFlowEmitter`'s doc). Neither depends on anything computed
        // between the old and new position.
        let mut id_input: BTreeMap<String, Value> = BTreeMap::new();
        id_input.insert("case_id".to_string(), Value::String(case_id_for_bookends.clone()));
        let mission_id = mission_launch::mint_run_id("review")?;
        emitter.mission_id = Some(mission_id.clone());
        let spec = crew::types::MissionSpec {
            config_id: "review".to_string(),
            inputs_fingerprint: mission_launch::spec_fingerprint(&id_input)?,
            // (#1562) Recorded at mint — a user-tier review variant is the
            // operator's named work; the shipped `review` is a run instance.
            origin: Some(spec_origin),
        };

        let ctx = Arc::new(ReviewStepContext {
            case_id: case_id_for_bookends.clone(),
            roles: crew.clone(),
            intent_title: String::new(),
            intent_body: intent,
            diff: diff_text.to_string(),
            // (#2310 P1 review finding I4) The real CLI-provided paths —
            // `build_review_graph_from_config` prefers these over the TEXT
            // above when stamping `review-context-step`'s config, so the
            // diff/intent no longer ride `Step.config` (persisted at every
            // transition by `save_step`) in full.
            diff_file: Some(diff_file.to_path_buf()),
            intent_file: intent_file_path.clone(),
            probe_system,
            probe_role_prompts: review_ctx.probe_role_prompts.clone(),
            judge_system,
            verify_system,
            remote_max_tokens_per_execution,
            judge_exhaustion_strict,
            timeout_seconds,
            chat_override: None,
            // (#1530) Production dispatch always reconstructs a real
            // `FileSource` from `review-bundle-step`'s own `Step.config`
            // (stamped from `bundle_spec` below) — the test-only seam stays
            // `None` here (see `ReviewStepContext::bundle_override`'s doc).
            bundle_override: None,
            // (#1641) See `ReviewStepContext::mission_id`'s doc.
            mission_id: Some(mission_id.clone()),
            // (#2310 P2) `run_review_graph` overlays these onto its own
            // bus-seeded copy from its own `crew_name`/`mode`/
            // `fingerprint_val`/`staffing` parameters, computed further
            // below — this literal's values are never read.
            crew_name: None,
            mode_label: None,
            fingerprint: None,
            staffing: None,
            interpret_warnings: Vec::new(),
            // (#2310 P1 fix) The real launch path never overrides
            // `review.context`'s resolution — it resolves for real.
            context_test_overrides: Default::default(),
        });

        // (#1417) Resolve the phase ids and build the review graph BEFORE
        // minting the Mission — `derive_phase_ids` is a pure, no-I/O
        // derivation of the same doc-id -> real-id map the mint below would
        // otherwise produce, so computing it here first doesn't change what
        // gets minted, only WHEN. This closes the strand where a config
        // whose phases don't resolve to `investigate`/`adjudicate`/`report`,
        // or that `build_review_graph` itself can't load/interpret (a
        // malformed USER-tier review.json — see that function's own doc),
        // used to mint a Mission (Active, 3 Planned phases) and then `?`
        // out with no finalize, leaving it permanently Active. Now those
        // failures happen before any Mission/Phase file exists, so there is
        // nothing left stranded.
        let judge_concurrency = darkmux_types::config_access::review_judge_concurrency();
        let real_phase_ids = mission_launch::derive_phase_ids(&mission_id, config);
        // (C3, #1284 Packet 4b review gate) `.get()` + a named error, never
        // a raw index panic: a USER-TIER review.json with renamed phase ids
        // lands exactly here (contract 7 — loud validation at the
        // consumption point, never a hot-path panic).
        let real_phase = |doc_id: &str| -> Result<String> {
            real_phase_ids.get(doc_id).cloned().ok_or_else(|| {
                anyhow!(
                    "mission launch review: config `{}` does not declare a phase with id \
                     `{doc_id}` — the review launcher requires the phase ids `investigate`, \
                     `adjudicate`, and `report` (a user-tier ~/.darkmux/mission-configs/\
                     review.json with renamed phases cannot be executed; declared: {})",
                    config.id,
                    real_phase_ids.keys().cloned().collect::<Vec<_>>().join(", ")
                )
            })
        };
        let investigate_phase_id = real_phase("investigate")?;
        let adjudicate_phase_id = real_phase("adjudicate")?;
        let report_phase_id = real_phase("report")?;

        // (#1538 follow-up) Build from the LAUNCHED document. `build_review_graph`
        // resolves `load("review")` by id, so calling it here would interpret the
        // built-in no matter what was launched — the silent-wrong-execution bug
        // this fix exists to close. `build_review_graph_from_config` is the same
        // function minus that hardcoded load.
        let graph = build_review_graph_from_config(
            review_config_doc,
            &format!("the launched config `{}`", config.id),
            ctx.clone(),
            &bundle_spec,
            judge.clone(),
            verify.clone(),
            &probes,
            &investigate_phase_id,
            &adjudicate_phase_id,
            &report_phase_id,
            judge_concurrency,
        )?;

        // (#1417, id-minting fixed #1503) Mint the Mission + the SAME three
        // phases review.json declares (investigate/adjudicate/report) — the
        // GENERIC run-minting primitive every config-launched mission uses.
        // Every review dispatch is a FRESH run now — a re-run of the same
        // case never reopens the prior run's record; `spec` (built above)
        // is what still lets same-case runs group for corpus analysis.
        // Moved to run AFTER `build_review_graph` succeeds (see the comment
        // above) — everything the graph needed (the phase id strings) was
        // already derived without touching the Mission, so nothing here
        // needs to run earlier.
        //
        // (must-fix 2, #1284 Packet 4b review gate) Case-bearing provenance,
        // matching the retired `build_mission_for_review`'s board-facing
        // strings: a fresh mint's description names the case + crew (N CI
        // reviews must be distinguishable on `mission status`/the viewer —
        // the config's own description is an 800-char transcription
        // paragraph, useless as a board row).
        //
        // (C4, same review gate) Mission bookkeeping on this path is
        // HARD-FAIL (`?`) — a DELIBERATE reversal of the retired
        // `build_mission_for_review`'s best-effort `let _ =` discipline
        // ("a persistence hiccup must never block the review"). An
        // unwritable crew dir would break the envelope/step saves later in
        // this same run anyway, leaving a half-recorded review; failing
        // loud here, still before any token is spent (the graph build above
        // is pure interpretation, no dispatch), beats that quiet partial
        // state.
        let description =
            format!("PR review — {case_id_for_bookends} (crew `{crew_name_for_bookends}`)");
        // (#1504) Both calls below are post-mint strand windows the #1417
        // comment above didn't close: a bare `?` here would leave a fresh,
        // permanently-Active mission on disk (a partial mint from
        // `ensure_mission_and_phases_with_provenance`, or a fully-minted one
        // whose `config-snapshot.json` write then failed) — and since #1503
        // mints a UNIQUE run id per launch, a repeated failing launch no
        // longer converges onto one reused instance the way the old
        // derive-from-inputs id used to; each failure needs its own reconcile
        // or it strands a NEW Active mission per attempt. `reconcile_mint_
        // failure` closes the mission (cascading any Planned phases to
        // Abandoned) ONLY if `mission.json` was actually written.
        if let Err(e) = mission_launch::ensure_mission_and_phases_with_provenance(
            &mission_id,
            config,
            Some(&description),
            Some(spec),
        ) {
            crew::lifecycle::reconcile_mint_failure(
                &mission_id,
                &format!("mission launch review errored during mint: {e:#}"),
            );
            return Err(e);
        }
        if let Err(e) = crew::lifecycle::save_config_snapshot(&mission_id, config)
            .context("persisting config-snapshot.json")
        {
            crew::lifecycle::reconcile_mint_failure(
                &mission_id,
                &format!("mission launch review errored during mint: {e:#}"),
            );
            return Err(e);
        }

        for task in &graph.tasks {
            let _ = crew::lifecycle::save_task(&mission_id, task);
        }

        // (#1400) Phases no longer start eagerly here — `investigate`/
        // `adjudicate`/`report` used to ALL flip Planned -> Running before
        // any dispatch happened, so the graph lens showed all three
        // pulsing "running" from second zero regardless of which one the
        // pipeline had actually reached. Each phase now starts LAZILY,
        // inside the `persist` closure below, the first time one of its
        // OWN steps flips `Running` — see `mission_launch::
        // lazy_start_phase_for_step`'s doc.

        let fingerprint_val = fingerprint(&judge_identifier, &ctx.judge_system);
        let staffing = staffing_snapshot(&probes, &judge, verify.as_ref(), request_changes);
        let phase_id_of_step = graph.phase_id_of_step.clone();
        // (#1397/#1400) A second clone for the transition-time `persist`
        // closure below — the post-run loop's own clone stays as the
        // cheap, idempotent final reconcile every other `run_step_graph`
        // caller also keeps.
        let phase_id_of_step_for_persist = graph.phase_id_of_step.clone();
        let mission_id_for_steps = mission_id.clone();
        let mission_id_for_persist = mission_id.clone();
        let crew_name_for_closure = crew_name_for_bookends.clone();
        let report_phase_id_for_closure = report_phase_id.clone();
        let report_phase_id_for_persist = report_phase_id.clone();
        let mut started_phases: std::collections::HashSet<String> = std::collections::HashSet::new();
        // (#1620) Phase order, for closing the bands an advance has moved past.
        // `derive_phase_ids` is keyed by config id, so the order comes from the
        // config's own declaration rather than from map iteration.
        let phase_order_for_persist: Vec<String> = vec![
            investigate_phase_id.clone(),
            adjudicate_phase_id.clone(),
            report_phase_id.clone(),
        ];
        let mut closed_phases: std::collections::HashSet<String> = std::collections::HashSet::new();

        // (#2124, generalized #2131) Armed here, right after the mint above
        // succeeded — every exit from this point on (the normal `close()`
        // call at the bottom of this block, an early `?`-return this
        // function doesn't currently have but might grow, a panic that
        // unwinds past this point, or a caught SIGTERM/SIGINT/SIGHUP)
        // leaves a matching mission terminal record behind. See
        // `launch_guard`'s own module doc for why the dispatch below runs
        // on a supervised worker thread rather than inline.
        let mission_id_for_close = mission_id.clone();
        let abort_mission_id = mission_id.clone();
        let abort_phase_ids =
            [investigate_phase_id.clone(), adjudicate_phase_id.clone(), report_phase_id.clone()];
        let mut guard = LaunchFinalizeGuard::new(move || {
            let err: Result<ReviewEnvelope> = Err(anyhow!(
                "mission launch review: mission aborted — the launcher exited before a terminal \
                 outcome was recorded (a signal, a panic, or an early return this guard did not \
                 expect)"
            ));
            let phase_refs: Vec<&str> = abort_phase_ids.iter().map(String::as_str).collect();
            finalize_review_mission(&abort_mission_id, &phase_refs, &err);
        });

        // (#2124) `with_dispatch_bookends`/`run_review_graph` is ONE
        // blocking, synchronous call — there is no seam inside it (or
        // inside the generic `run_step_graph` it drives) to check an
        // interrupt flag mid-run. Running it on its own thread and
        // supervising from here is what makes a SIGTERM/SIGINT arriving
        // mid-probe (the exact #2124 repro) actually interruptible: the
        // supervisor loop below polls the interrupt flag independently of
        // whatever step the worker happens to be blocked in.
        let dispatch_worker = std::thread::spawn(move || -> Result<ReviewEnvelope> {
            with_dispatch_bookends(
                &mut emitter,
                &case_id_for_bookends,
                &crew_name_for_bookends,
                model_for_bookends.as_deref(),
                // (#1508 /runs gate finding, must-fix 1) Stamp the freshly-
                // minted `mission_id` onto this run's dispatch bookends — a
                // default review launch previously carried `mission_id: None`
                // here, making the review's own case-string-keyed flow session
                // unrecoverable from the Mission record's read side (`/runs`
                // synthesized a spurious untracked ghost for every review run).
                Some(mission_id.as_str()),
                dispatch_start_extra,
                move |emitter| {
                    run_review_graph(
                        &ctx,
                        &crew_name_for_closure,
                        mode,
                        fingerprint_val,
                        staffing,
                        graph,
                        emitter,
                        // (#1397) Durably persist each step at ITS OWN
                        // transition (Running at dispatch, Complete/Error at
                        // completion) — the review pipeline runs through the
                        // SAME `run_step_graph` call the crew scheduler's other
                        // callers use, so it gets the identical mid-run
                        // observability fix: a graph page opened while a probe
                        // is still dispatching reads that step's real `Running`
                        // status instead of the pre-run `Planned` snapshot.
                        &mut |step| {
                            let phase_id = phase_id_of_step_for_persist
                                .get(&step.id)
                                .map(String::as_str)
                                .unwrap_or(&report_phase_id_for_persist);
                            let was_new = mission_launch::lazy_start_phase_for_step(
                                &mission_id_for_persist,
                                phase_id,
                                step.status,
                                &mut started_phases,
                            );
                            // (#1620) A phase becoming live is the signal that the
                            // bands before it are over — phases are strictly
                            // sequential (#1341), so reaching this one means the
                            // earlier ones' work is done. Close them at their EARNED
                            // outcome now, instead of leaving every touched phase
                            // `Running` until the whole mission finalizes and
                            // reconciles them in bulk (which made the board read
                            // `0/3` for the entire run, then jump to `3/3`).
                            if was_new {
                                mission_launch::lazy_close_prior_phases(
                                    &mission_id_for_persist,
                                    &phase_order_for_persist,
                                    phase_id,
                                    &mut closed_phases,
                                );
                            }
                            // (F2, gate remediation) Warn, never silently
                            // swallow — same dim-warning parity as
                            // `mission_launch.rs`/`coder_phase.rs`'s persist
                            // closures: a disk-full mid-review would otherwise
                            // freeze the graph page with zero operator signal.
                            if let Err(e) = crew::lifecycle::save_step(&mission_id_for_persist, phase_id, step) {
                                eprintln!(
                                    "{}",
                                    style::dim(&format!(
                                        "mission launch review: step persist warning (transition): {e:#}"
                                    ))
                                );
                            }
                        },
                    )
                    .map(|(env, steps)| {
                        for (step_id, step) in &steps {
                            let phase_id = phase_id_of_step
                                .get(step_id)
                                .map(String::as_str)
                                .unwrap_or(&report_phase_id_for_closure);
                            // (F2) Same dim-warning parity as the transition
                            // persist above and `mission_launch.rs`'s own
                            // post-run reconcile loop.
                            if let Err(e) = crew::lifecycle::save_step(&mission_id_for_steps, phase_id, step) {
                                eprintln!(
                                    "{}",
                                    style::dim(&format!(
                                        "mission launch review: step persist warning: {e:#}"
                                    ))
                                );
                            }
                        }
                        env
                    })
                },
            )
        });

        // (#2124) Poll BOTH the interrupt flag and the worker's own
        // completion on a short timer — this is the "checked between
        // steps" checkpoint for this launcher, at poll-tick granularity
        // rather than graph-step granularity (see this module's own doc for
        // why). A productive run never observes anything
        // here beyond the worker finishing first.
        let poll_interval = std::time::Duration::from_millis(150);
        while !dispatch_worker.is_finished() && !darkmux_types::interrupt::is_set() {
            std::thread::sleep(poll_interval);
        }
        let result: Result<ReviewEnvelope> = if darkmux_types::interrupt::is_set() && !dispatch_worker.is_finished() {
            // The worker is still running — almost certainly blocked
            // inside a synchronous `curl`/`docker` child. Joining here
            // would block on exactly the call this fix exists to escape,
            // so the handle is deliberately abandoned: `guard.close` below
            // still writes a real terminal record, then reaps whatever
            // that worker's child process was, once the record is durable.
            Err(anyhow!(
                "mission launch review: interrupted by signal (SIGINT/SIGTERM) before the \
                 review pipeline finished"
            ))
        } else {
            match dispatch_worker.join() {
                Ok(result) => result,
                Err(panic_payload) => Err(anyhow!(
                    "mission launch review: the review dispatch thread panicked: {}",
                    launch_guard::panic_message(&*panic_payload)
                )),
            }
        };

        // The Mission/Phases minted above start life Active/Running and,
        // without this, never reach a terminal status regardless of
        // outcome — every review, clean, errored, or signal-interrupted,
        // would be left permanently "stuck" in `darkmux mission status`.
        guard.close(|| {
            let phase_refs: Vec<&str> =
                vec![investigate_phase_id.as_str(), adjudicate_phase_id.as_str(), report_phase_id.as_str()];
            finalize_review_mission(&mission_id_for_close, &phase_refs, &result);
        });
        // (#2124, generalized #2131) The worker thread may still be blocked
        // deep inside a `curl` child at this point — the terminal record
        // just written above is already durable, so it's safe to reap
        // every registered child pid and exit. A no-op when no signal was
        // observed — a normal completion falls through to the
        // rendering/exit-code work below unaffected.
        launch_guard::reap_and_exit_on_signal();

        result
    };

    // (#1513 review C3) Fold `resolve_review_roles`'s own warnings (today,
    // just the "a `review.verify-render` task exists but declares no
    // role_id" case) into the returned envelope. Both dispatch paths above
    // — the charges-file replay and the graph run — resolve `crew` from the
    // SAME `resolve_review_roles` call near the top of this function, so
    // one merge point covers both without threading the warnings through
    // `build_review_graph`'s signature (which the pure-refactor gate keeps
    // byte-identical).
    outcome.map(|mut env| {
        env.warnings.extend(crew.warnings.iter().cloned());
        env
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use darkmux_lab::lab::review::MemberRecord;
    use darkmux_crew::resourcing::ResolvedSeatStaffing;
    use darkmux_types::ProfileModel;

    // ── #1512, #1513 review: config-driven `--param <role>=<profile>` override collection ──

    /// A hand-built "review" mission config declaring `n` probe tasks (each
    /// its own `role_id`) plus a judge task — the shape [`declared_role_ids`]
    /// walks. Mirrors `resourcing.rs`'s own test fixture shape.
    fn config_with_n_probe_roles(n: usize) -> MissionConfig {
        let probe_tasks: Vec<serde_json::Value> = (0..n)
            .map(|i| {
                serde_json::json!({
                    "id": format!("review-probe-{i}-task"),
                    "role_id": format!("review-probe-{i}"),
                    "depends_on": [],
                    "steps": [{"id": format!("review-probe-{i}-step"), "kind": "dispatch.map"}]
                })
            })
            .collect();
        let doc = serde_json::json!({
            "id": "review",
            "name": "PR Review",
            "phases": [
                {"id": "investigate", "tasks": probe_tasks},
                {"id": "adjudicate", "tasks": [
                    {"id": "review-judge-task", "role_id": "review-judge", "depends_on": [],
                     "steps": [{"id": "review-judge-step", "kind": "review.judge"}]}
                ]}
            ]
        });
        serde_json::from_value(doc).expect("hand-built review config parses")
    }

    /// The set of overridable role ids is read straight off the document's
    /// own declared tasks (#1512, #1513 review) — never a fixed Rust-side
    /// count. A one-probe review config yields exactly one probe role in
    /// the returned set (plus judge); a five-probe config yields five.
    #[test]
    fn declared_role_ids_is_driven_by_the_document() {
        let one = declared_role_ids(&config_with_n_probe_roles(1));
        assert_eq!(one, vec!["review-judge".to_string(), "review-probe-0".to_string()]);

        let five = declared_role_ids(&config_with_n_probe_roles(5));
        assert_eq!(five.len(), 6, "5 probe roles + judge");
        for i in 0..5 {
            assert!(five.contains(&format!("review-probe-{i}")));
        }
    }

    /// (#1475 packet 3, #1512) `--param <role>=<profile>` per-run overrides
    /// still bind against whichever roles the review config declares —
    /// preserved end to end through the config-driven refactor. A role NOT
    /// in the declared set (nor judge/verify) is ignored, matching the
    /// pre-#1512 behavior of only reading the five known roles.
    #[test]
    fn collect_role_overrides_reads_only_declared_probe_roles_plus_judge_and_verify() {
        // (#1513 review) `role_ids` is the FULL declared set now — the
        // caller's job (`declared_role_ids`), not something this function
        // pads with judge/verify automatically.
        let role_ids = vec![
            "review-probe-high".to_string(),
            "review-probe-mid".to_string(),
            "review-judge".to_string(),
        ];
        let mut collected = BTreeMap::new();
        collected.insert("review-probe-high".to_string(), Value::String("fast".to_string()));
        collected.insert("review-probe-mid".to_string(), Value::String("  ".to_string())); // blank -> ignored
        collected.insert("review-probe-low".to_string(), Value::String("ghost".to_string())); // not declared -> ignored
        collected.insert("review-judge".to_string(), Value::String("frontier".to_string()));
        collected.insert("review-verify".to_string(), Value::String("".to_string())); // not declared -> ignored

        let overrides = collect_role_overrides(&collected, &role_ids);

        assert_eq!(overrides.get("review-probe-high").map(String::as_str), Some("fast"));
        assert_eq!(overrides.get("review-judge").map(String::as_str), Some("frontier"));
        assert!(!overrides.contains_key("review-probe-mid"), "blank value is ignored");
        assert!(!overrides.contains_key("review-probe-low"), "undeclared role is never read (#1512)");
        assert!(!overrides.contains_key("review-verify"), "undeclared role is never read, even with a value present");
        assert_eq!(overrides.len(), 2);
    }

    // ── #1272-equivalent: dispatch.start/terminal bookends around a
    // production review run (ported from the retired `pr_review.rs` test
    // module — the logic under test moved here in #1284 Packet 4b) ───────

    /// Recording [`ReviewEmitter`] mock — pushes every emitted record into a
    /// `Vec` so a test can assert the exact action sequence.
    #[derive(Default)]
    struct RecordingEmitter {
        records: Vec<darkmux_flow::FlowRecord>,
    }
    impl ReviewEmitter for RecordingEmitter {
        fn emit(&mut self, record: darkmux_flow::FlowRecord) {
            self.records.push(record);
        }
    }

    /// A minimal, valid `FlowRecord` standing in for whatever `run_review_graph`/
    /// `run_judge_only` would really emit through the injected `ReviewEmitter`
    /// mid-dispatch — the bookend wrapper doesn't inspect these records, only
    /// brackets them, so a bare action string is enough to prove ordering.
    fn fake_review_record(action: &str) -> darkmux_flow::FlowRecord {
        darkmux_flow::FlowRecord {
            ts: darkmux_flow::ts_utc_now(),
            level: darkmux_flow::Level::Info,
            category: darkmux_flow::Category::Work,
            tier: darkmux_flow::Tier::Local,
            stage: darkmux_flow::Stage::Dispatch,
            action: action.to_string(),
            handle: "test-crew".to_string(),
            phase_id: None,
            session_id: Some("case-1".to_string()),
            source: Some("review".to_string()),
            model: None,
            reasoning: None,
            mission_id: None,
            machine_id: None,
            machine_uid: None,
            prev_hash: None,
            hash: None,
            payload: None,
            work_id: None,
            attempt: None,
        }
    }

    #[test]
    fn with_dispatch_bookends_start_precedes_review_records_with_exactly_one_terminal_on_success() {
        let mut emitter = RecordingEmitter::default();
        let result = with_dispatch_bookends(&mut emitter, "case-1", "test-crew", Some("darkmux:judge-model"), None, json!({}), |em| {
            // (#1434) The review driver emits the generic `step result`
            // vocabulary; the bookend wrapper brackets whatever inner records
            // it emits, so these stand in for a real run's step results.
            em.emit(fake_review_record("step result"));
            em.emit(fake_review_record("step result"));
            em.emit(fake_review_record("step result"));
            Ok(ReviewEnvelope::default())
        });
        assert!(result.is_ok());

        let actions: Vec<&str> = emitter.records.iter().map(|r| r.action.as_str()).collect();
        assert_eq!(actions, vec!["dispatch start", "step result", "step result", "step result", "dispatch complete"]);

        let terminals = actions.iter().filter(|a| **a == "dispatch complete" || **a == "dispatch error").count();
        assert_eq!(terminals, 1, "exactly one terminal dispatch record: {actions:?}");

        let start = &emitter.records[0];
        assert_eq!(start.session_id.as_deref(), Some("case-1"));
        assert_eq!(start.handle, "test-crew");
        assert_eq!(start.model.as_deref(), Some("darkmux:judge-model"));
        assert_eq!(start.source.as_deref(), Some("review"));
        // The `--charges-file` judge-only path (and this test, standing in
        // for it) mints no Mission — `None` here is honest, not a gap.
        assert_eq!(start.mission_id, None);

        let terminal = emitter.records.last().unwrap();
        assert_eq!(terminal.action, "dispatch complete");
        assert_eq!(terminal.source.as_deref(), Some("review"));
        assert_eq!(terminal.payload.as_ref().unwrap()["result_class"], "ok");
    }

    /// (#1508 /runs gate finding, must-fix 1) A real review launch mints a
    /// Mission before calling `with_dispatch_bookends` and now threads that
    /// id through — every bookend record (start AND terminal) must carry it,
    /// closing the gap that made `/runs` double-list every review run
    /// (the tracked Mission row joined zero sessions; the case-string
    /// session became a spurious untracked ghost carrying the real route).
    #[test]
    fn with_dispatch_bookends_stamps_mission_id_on_start_and_terminal_when_provided() {
        let mut emitter = RecordingEmitter::default();
        let result = with_dispatch_bookends(
            &mut emitter,
            "owner/repo@deadbeef",
            "test-crew",
            Some("darkmux:judge-model"),
            Some("review-1700000000-abcdef"),
            json!({}),
            |em| {
                em.emit(fake_review_record("step result"));
                Ok(ReviewEnvelope::default())
            },
        );
        assert!(result.is_ok());

        let start = &emitter.records[0];
        assert_eq!(start.action, "dispatch start");
        assert_eq!(start.mission_id.as_deref(), Some("review-1700000000-abcdef"));

        let terminal = emitter.records.last().unwrap();
        assert_eq!(terminal.action, "dispatch complete");
        assert_eq!(terminal.mission_id.as_deref(), Some("review-1700000000-abcdef"));
    }

    /// The error terminal (a hard `Err` from the review driver, not the
    /// abort/panic guard) must ALSO carry `mission_id` — a partial fix that
    /// stamped only the happy path would still leave failed review runs
    /// double-listing in `/runs`.
    #[test]
    fn with_dispatch_bookends_stamps_mission_id_on_the_error_terminal_too() {
        let mut emitter = RecordingEmitter::default();
        let result = with_dispatch_bookends(
            &mut emitter,
            "owner/repo@baddeed",
            "test-crew",
            None,
            Some("review-1700000001-fedcba"),
            json!({}),
            |_em| Err(anyhow!("probe dispatch failed: connection refused")),
        );
        assert!(result.is_err());

        let terminal = emitter.records.last().unwrap();
        assert_eq!(terminal.action, "dispatch error");
        assert_eq!(terminal.mission_id.as_deref(), Some("review-1700000001-fedcba"));
    }

    /// (#1876 QA follow-up, MUST FIX 1) A judge-coverage shortfall
    /// (`env.degenerate.is_none()` but `env.warnings` non-empty — exactly
    /// the #1876 incident shape, 11 of 134 flags unjudged) must NOT read as
    /// an unqualified `result_class: "ok"` on the flow record. Before
    /// #1876, that same run set `env.degenerate` and the bookend already
    /// stamped a marker for it; #1876 moved the incident case off
    /// `degenerate` and onto `warnings`, and this bookend never learned to
    /// read the new field — so the flow stream (and everything reading it:
    /// the viewer, the Redis fleet stream, the hash-chained audit sink)
    /// recorded a review that silently dropped 11 of 134 flags as a clean
    /// pass. `result_class` must flip to `"partial"` (matching the
    /// `"partial"`/`ok_count == results.len()` precedent already used by
    /// `step_kinds::builtins`'s map-step terminal) and the warning text
    /// must ride in the payload.
    #[test]
    fn with_dispatch_bookends_marks_partial_coverage_on_the_terminal_payload() {
        let mut emitter = RecordingEmitter::default();
        let result = with_dispatch_bookends(&mut emitter, "case-1", "test-crew", Some("darkmux:judge-model"), None, json!({}), |em| {
            em.emit(fake_review_record("step result"));
            Ok(ReviewEnvelope {
                warnings: vec!["11 of 134 flags went unjudged on the `judge-pass1` stage".to_string()],
                ..ReviewEnvelope::default()
            })
        });
        assert!(result.is_ok());

        let terminal = emitter.records.last().unwrap();
        assert_eq!(terminal.action, "dispatch complete");
        let payload = terminal.payload.as_ref().unwrap();
        assert_eq!(
            payload["result_class"], "partial",
            "a run with unresolved warnings must not report result_class \"ok\": {payload:?}"
        );
        assert_eq!(
            payload["warnings"][0], "11 of 134 flags went unjudged on the `judge-pass1` stage",
            "the shortfall reason must ride in the flow record, not just the posted comment: {payload:?}"
        );
        // The pre-existing `degenerate` marker is untouched by this case —
        // this run is NOT degenerate, only partially covered.
        assert!(payload.get("degenerate").is_none());
    }

    /// The abort/panic path's `on_abort` closure builds its own terminal
    /// record independently of the `Ok`/`Err` match arms above — it needs
    /// its own assertion that `mission_id` survives the `move` closure
    /// capture (`mission_id_owned`).
    #[test]
    fn with_dispatch_bookends_stamps_mission_id_on_the_panic_abort_terminal_too() {
        let mut emitter = RecordingEmitter::default();
        let prev_hook = std::panic::take_hook();
        std::panic::set_hook(Box::new(|_| {}));
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            with_dispatch_bookends(
                &mut emitter,
                "owner/repo@panicky",
                "test-crew",
                None,
                Some("review-1700000002-111222"),
                json!({}),
                |_em| panic!("simulated crash mid-dispatch"),
            )
        }));
        std::panic::set_hook(prev_hook);
        assert!(result.is_err());

        let terminal = emitter.records.last().unwrap();
        assert_eq!(terminal.action, "dispatch error");
        assert_eq!(terminal.mission_id.as_deref(), Some("review-1700000002-111222"));
    }

    #[test]
    fn with_dispatch_bookends_error_path_emits_dispatch_error_terminal_with_the_real_message() {
        let mut emitter = RecordingEmitter::default();
        let result = with_dispatch_bookends(&mut emitter, "case-2", "test-crew", None, None, json!({}), |em| {
            em.emit(fake_review_record("step result"));
            Err(anyhow!("probe dispatch failed: connection refused"))
        });
        assert!(result.is_err());

        let terminal = emitter.records.last().unwrap();
        assert_eq!(terminal.action, "dispatch error");
        assert_eq!(terminal.source.as_deref(), Some("review"));
        let payload = terminal.payload.as_ref().unwrap();
        assert_eq!(payload["result_class"], "error");
        assert!(payload["error"].as_str().unwrap().contains("connection refused"));
    }

    #[test]
    fn with_dispatch_bookends_panic_still_emits_a_dispatch_error_terminal_via_the_guard() {
        let mut emitter = RecordingEmitter::default();
        let prev_hook = std::panic::take_hook();
        std::panic::set_hook(Box::new(|_| {}));
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            with_dispatch_bookends(&mut emitter, "case-3", "test-crew", None, None, json!({}), |_em| {
                panic!("simulated crash mid-dispatch");
            })
        }));
        std::panic::set_hook(prev_hook);
        assert!(result.is_err());

        let actions: Vec<&str> = emitter.records.iter().map(|r| r.action.as_str()).collect();
        assert_eq!(actions, vec!["dispatch start", "dispatch error"]);
        let terminal = emitter.records.last().unwrap();
        assert_eq!(terminal.source.as_deref(), Some("review"));
        assert!(terminal.payload.as_ref().unwrap()["error"]
            .as_str()
            .unwrap()
            .contains("early return or panic"));
    }

    #[test]
    fn with_dispatch_bookends_stamps_endpoint_and_remote_tokens_for_a_remote_member() {
        let mut emitter = RecordingEmitter::default();
        let result = with_dispatch_bookends(&mut emitter, "case-4", "test-crew", Some("darkmux:judge-model"), None, json!({}), |em| {
            em.emit(fake_review_record("step result"));
            Ok(ReviewEnvelope {
                members: vec![
                    MemberRecord {
                        model: "darkmux:probe-model".into(),
                        seat: "review-probe".into(),
                        draws: 3,
                        wall_ms: 1200,
                        total_tokens: 900,
                        remote: false,
                        endpoint: None,
                        served_model: None,
                    },
                    MemberRecord {
                        model: "gpt-4o".into(),
                        seat: "review-judge".into(),
                        draws: 2,
                        wall_ms: 800,
                        total_tokens: 777,
                        remote: true,
                        endpoint: Some("myorg.cognitiveservices.azure.com".into()),
                        served_model: None,
                    },
                ],
                ..Default::default()
            })
        });
        assert!(result.is_ok(), "{result:?}");

        let terminal = emitter.records.iter().find(|r| r.action == "dispatch complete").unwrap();
        let payload = terminal.payload.as_ref().unwrap();
        assert_eq!(payload["endpoint"], "azure:myorg.cognitiveservices.azure.com/gpt-4o", "{payload}");
        assert_eq!(payload["remote_tokens"], 777, "{payload}");
    }

    #[test]
    fn with_dispatch_bookends_omits_endpoint_and_remote_tokens_when_fully_local() {
        let mut emitter = RecordingEmitter::default();
        let result = with_dispatch_bookends(&mut emitter, "case-5", "test-crew", None, None, json!({}), |_em| {
            Ok(ReviewEnvelope {
                members: vec![MemberRecord {
                    model: "darkmux:probe-model".into(),
                    seat: "review-probe".into(),
                    remote: false,
                    ..Default::default()
                }],
                ..Default::default()
            })
        });
        assert!(result.is_ok());

        let terminal = emitter.records.iter().find(|r| r.action == "dispatch complete").unwrap();
        let payload = terminal.payload.as_ref().unwrap();
        assert!(payload.get("endpoint").is_none());
        assert!(payload.get("remote_tokens").is_none());
    }

    // ── crew_model_summary ──────────────────────────────────────────────

    fn staffing(model_id: &str) -> ResolvedSeatStaffing {
        ResolvedSeatStaffing {
            name: "fast".into(),
            role_id: None,
            pm: ProfileModel { id: model_id.into(), ..Default::default() },
            k: 1,
            passes: 2,
            max_tokens: None,
            selector: None,
            provenance: None,
        }
    }

    fn remote_staffing_url(model_id: &str, url: &str) -> ResolvedSeatStaffing {
        let mut st = staffing(model_id);
        st.pm.endpoint =
            Some(darkmux_types::ModelEndpoint { url: Some(url.to_string()), ..Default::default() });
        st
    }

    fn remote_staffing(model_id: &str, host: &str) -> ResolvedSeatStaffing {
        let mut st = staffing(model_id);
        st.pm.endpoint = Some(darkmux_types::ModelEndpoint {
            url: Some(format!("https://{host}/openai/deployments/{model_id}")),
            ..Default::default()
        });
        st
    }

    #[test]
    fn a_benign_empty_review_is_clean_not_degenerate() {
        // (#1654, operator direction) `Degenerate` is a RETRY SIGNAL — it
        // tells the session waiting on this review that the run is worth
        // repeating. A diff that legitimately contained no reviewable code
        // produces the identical empty result on every repeat, so flagging
        // it degenerate manufactures an unbounded retry loop out of a
        // correct outcome. The posted comment already says "re-running will
        // produce the same result"; the board has to agree, or the operator
        // reads a friendly note on the PR and a red phase for one run.
        let env = ReviewEnvelope {
            degenerate: Some("no bundles produced from the diff — 2 skipped (2 non-code)".into()),
            degenerate_kind: Some(darkmux_lab::lab::review::DegenerateKind::BenignEmpty),
            ..Default::default()
        };
        let out = review_result_to_mission_envelope("m-1", &["p-1"], &Ok(env));
        assert_eq!(
            out.status,
            crew::envelope::MissionOutcomeStatus::Clean,
            "a benign-empty review must not read as a failure the session should retry"
        );
        assert!(
            out.reason.as_deref().unwrap_or_default().contains("no bundles"),
            "the WHY is still recorded — this narrows what gets flagged, not what gets kept: {:?}",
            out.reason
        );
    }

    #[test]
    fn an_unsupported_language_review_is_clean_not_degenerate() {
        // (#1757) Same reasoning as the benign-empty carve-out above,
        // applied to a diff that's real source in a language the built-in
        // bundler can't parse: re-running produces the identical result, so
        // `Degenerate` would manufacture the same unbounded retry loop.
        let env = ReviewEnvelope {
            degenerate: Some("no bundles produced from the diff — 1 skipped (1 real source in an unsupported language)".into()),
            degenerate_kind: Some(darkmux_lab::lab::review::DegenerateKind::UnsupportedLanguage),
            ..Default::default()
        };
        let out = review_result_to_mission_envelope("m-1", &["p-1"], &Ok(env));
        assert_eq!(
            out.status,
            crew::envelope::MissionOutcomeStatus::Clean,
            "an unsupported-language review must not read as a failure the session should retry"
        );
        assert!(
            out.reason.as_deref().unwrap_or_default().contains("unsupported language"),
            "the WHY is still recorded: {:?}",
            out.reason
        );
    }

    #[test]
    fn an_error_empty_review_is_still_degenerate() {
        // The control, and the more important half: a real failure must
        // KEEP prompting a re-run. If the benign carve-out widened to every
        // zero-bundle run, genuine breakage would go quiet — the same
        // silent-miss failure the classifier fails closed to prevent.
        let env = ReviewEnvelope {
            degenerate: Some("no bundles produced from the diff".into()),
            degenerate_kind: Some(darkmux_lab::lab::review::DegenerateKind::Error),
            ..Default::default()
        };
        let out = review_result_to_mission_envelope("m-1", &["p-1"], &Ok(env));
        assert_eq!(out.status, crew::envelope::MissionOutcomeStatus::Degenerate);
    }

    /// (#1876/#1877 QA follow-up) A judge-stage coverage shortfall under
    /// the DEFAULT (non-strict) policy leaves `env.degenerate` unset but
    /// pushes a warning into `env.warnings` (`judge_gate_outcome`'s new
    /// `coverage_warning` field, wired at both call sites in
    /// `crates/darkmux-lab/src/lab/review.rs`) — exactly so this classifier
    /// reads it as `Degraded`, not `Clean`. Before that wiring existed, a
    /// partial-coverage run (real signal, real gap) left BOTH `degenerate`
    /// and `warnings` empty and read `Clean` here, even though the posted
    /// PR comment said "Incomplete review — this is not a clean pass" —
    /// the exact "board and the comment must agree" property this
    /// function's own doc promises, broken by fixing only the render side.
    #[test]
    fn a_partial_coverage_review_is_degraded_not_clean() {
        let env = ReviewEnvelope {
            degenerate: None,
            warnings: vec![
                "remote judge token budget exhausted — 11 judge call(s) skipped after the \
                 per-execution allowance (500000 tokens per stage) ran out — the flags that WERE \
                 judged still render; see the envelope's remote_budgets for the full accounting"
                    .to_string(),
            ],
            ..Default::default()
        };
        let out = review_result_to_mission_envelope("m-1", &["p-1"], &Ok(env));
        assert_eq!(
            out.status,
            crew::envelope::MissionOutcomeStatus::Degraded,
            "a partial-coverage run must not read Clean on the mission board — `mission launch \
             review`'s own CLI exit code is unaffected either way (always Ok(0); CI-facing \
             pass/fail comes from the rendered payload's mode field, per src/cli.rs:552)"
        );
        assert!(
            out.reason.as_deref().unwrap_or_default().contains("remote judge token budget exhausted"),
            "the WHY is recorded: {:?}",
            out.reason
        );
    }

    /// (#1877 item 2) The SAME `a_partial_coverage_review_is_degraded_not_clean`
    /// shape, now also asserting the typed `outcome` field: `status` alone
    /// tells you "Degraded", but `outcome` is what carries the REAL number
    /// (11 skipped judge calls) as a typed `Partial { reasons }` instead of
    /// only a free-text warning string. This is the "Partial becomes typed
    /// rather than conventional" claim from #1877's own tracking issue,
    /// proven on the actual review->envelope path, not just on
    /// `MissionEnvelope::from_outcome` in isolation.
    ///
    /// (#1877 QA ALSO FIX 2) Also pins the deliberate duplication this
    /// function's own doc names: `envelope.warnings` and
    /// `envelope.outcome`'s `Partial.reasons` are the SAME `Vec` assigned
    /// twice, not two independently-derived values that happen to agree.
    /// If a future change drops one copy without updating the other, THIS
    /// assertion is what catches the drift — not the shape of the JSON,
    /// which would still parse fine either way.
    #[test]
    fn a_partial_coverage_review_round_trips_as_a_typed_partial_outcome_with_real_numbers() {
        let env = ReviewEnvelope {
            degenerate: None,
            warnings: vec![
                "11 of 134 flags went unjudged on the `judge-pass1` stage — it exceeded its 500000-token \
                 allowance (500497 used)"
                    .to_string(),
            ],
            ..Default::default()
        };
        let out = review_result_to_mission_envelope("m-1", &["p-1"], &Ok(env));
        assert_eq!(out.status, crew::envelope::MissionOutcomeStatus::Degraded);
        match &out.outcome {
            Some(darkmux_crew::run_outcome::RunOutcome::Partial { reasons }) => {
                assert_eq!(reasons.len(), 1);
                assert!(reasons[0].contains("11 of 134 flags went unjudged"), "got: {}", reasons[0]);
                assert!(reasons[0].contains("500497"), "the used-token count is the real number: {}", reasons[0]);
                assert_eq!(
                    &out.warnings, reasons,
                    "envelope.warnings must stay in lockstep with outcome's Partial.reasons — \
                     they are the same Vec assigned twice, per this function's own doc"
                );
            }
            other => panic!("expected Some(Partial), got {other:?}"),
        }

        // Round-trip the whole envelope through JSON (what actually gets
        // persisted as `envelope.json`) — the typed reasons must survive
        // serde, not just live as an in-memory value.
        let json = serde_json::to_string(&out).unwrap();
        let back: crew::envelope::MissionEnvelope = serde_json::from_str(&json).unwrap();
        assert_eq!(back.status, crew::envelope::MissionOutcomeStatus::Degraded);
        assert_eq!(back.outcome, out.outcome);
        match &back.outcome {
            Some(darkmux_crew::run_outcome::RunOutcome::Partial { reasons }) => {
                assert_eq!(&back.warnings, reasons, "the two copies survive the round-trip in lockstep too");
            }
            other => panic!("expected Some(Partial) after round-trip, got {other:?}"),
        }
    }

    /// The `Clean`/`Degenerate` cases produce `outcome: Some(Complete)` /
    /// `Some(Empty { .. })` too — `outcome` is populated on EVERY `Ok(env)`
    /// path through this function now, not only the `Partial` one.
    #[test]
    fn a_clean_review_carries_outcome_complete_and_a_degenerate_one_carries_outcome_empty() {
        let clean = review_result_to_mission_envelope("m-1", &["p-1"], &Ok(ReviewEnvelope::default()));
        assert_eq!(clean.outcome, Some(darkmux_crew::run_outcome::RunOutcome::Complete));

        let degenerate_env = ReviewEnvelope {
            degenerate: Some("no bundles produced from the diff".to_string()),
            degenerate_kind: Some(darkmux_lab::lab::review::DegenerateKind::Error),
            ..Default::default()
        };
        let degenerate = review_result_to_mission_envelope("m-1", &["p-1"], &Ok(degenerate_env));
        assert_eq!(
            degenerate.outcome,
            Some(darkmux_crew::run_outcome::RunOutcome::Empty {
                reason: "no bundles produced from the diff".to_string()
            })
        );
    }

    /// An `Err` result (a hard failure before any envelope was produced)
    /// still carries `outcome: None` — there was no run to summarize into a
    /// `RunOutcome`, and fabricating one would misrepresent "no run
    /// happened" as "a run happened and covered nothing," which is what
    /// `RunOutcome::Empty` actually means.
    #[test]
    fn an_error_result_carries_no_outcome() {
        let err = review_result_to_mission_envelope("m-1", &["p-1"], &Err(anyhow!("probe dispatch failed")));
        assert_eq!(err.status, crew::envelope::MissionOutcomeStatus::Error);
        assert!(err.outcome.is_none());
    }

    #[test]
    fn crew_route_label_never_leaks_url_userinfo() {
        // (#1660) A profile url may legitimately carry userinfo — some
        // LiteLLM/proxy setups document `https://tok@host/v1` — and this
        // label rides into flow records and the run envelope, which CI
        // uploads as a PUBLIC artifact. The terminal side has stripped it
        // since #1530 with a comment explaining exactly this; the start side
        // did not, because every fixture used clean hosts. Sanitizing now
        // happens at the shared `remote_route_label` chokepoint, so this
        // holds for every caller, not just this one.
        let roles = ResolvedReviewRoles {
            probes: vec![],
            judge: remote_staffing_url(
                "gpt-4o",
                "https://sk-secret-token@myorg.cognitiveservices.azure.com/v1",
            ),
            verify: None,
            request_changes: false,
            warnings: Vec::new(),
        };
        let label = crew_route_label(&roles).expect("a remote judge yields a label");
        assert!(
            !label.contains("sk-secret-token") && !label.contains('@'),
            "the route label must never carry URL userinfo — it rides to public artifacts: {label}"
        );
        assert_eq!(label, "azure:myorg.cognitiveservices.azure.com/gpt-4o");
    }

    #[test]
    fn crew_route_label_ignores_an_endpoint_that_is_not_actually_remote() {
        // (#1660) `is_remote()` is just `url.is_some()`, so a bare
        // `"endpoint": {}` — legal under the lenient schema — makes
        // `base_url()` fall back to the LOCAL LMStudio url. Without the
        // filter this stamped a CLOUD label on a fully-local run: the exact
        // inverse of the bug #1520 fixed, and invisible to the
        // `endpoint: None` fixture that claimed to cover it.
        let mut judge = staffing("darkmux:local-judge");
        judge.pm.endpoint = Some(darkmux_types::ModelEndpoint::default());
        let roles = ResolvedReviewRoles {
            probes: vec![],
            judge,
            verify: None,
            request_changes: false,
            warnings: Vec::new(),
        };
        assert_eq!(
            crew_route_label(&roles),
            None,
            "an endpoint with no url is not remote — stamping a cloud route here is a lie"
        );
    }

    #[test]
    fn crew_route_label_is_none_when_every_seat_is_local() {
        // (#1520) A fully-local review must NOT stamp `payload.endpoint` —
        // that field is the viewer's cloud-vs-local switch, so inventing one
        // here would misclassify a local run as remote. The bug being fixed
        // is the absence of a TRUE label, never the presence of a false one.
        let roles = ResolvedReviewRoles {
            probes: vec![staffing("darkmux:probe")],
            judge: staffing("darkmux:judge"),
            verify: None,
            request_changes: false,
            warnings: Vec::new(),
        };
        assert_eq!(crew_route_label(&roles), None);
    }

    #[test]
    fn crew_route_label_prefers_the_judge_and_matches_the_terminal_shape() {
        // (#1520) Byte-identical to what the TERMINAL bookend stamps — the
        // viewer resolves `payload.endpoint` from start OR complete (#1518),
        // so if the two bookends disagreed the classification would flip
        // mid-run. Judge-first because it is the load-bearing seat.
        let roles = ResolvedReviewRoles {
            probes: vec![remote_staffing("gpt-4o-mini", "probes.cognitiveservices.azure.com")],
            judge: remote_staffing("gpt-4o", "myorg.cognitiveservices.azure.com"),
            verify: None,
            request_changes: false,
            warnings: Vec::new(),
        };
        assert_eq!(
            crew_route_label(&roles).as_deref(),
            Some("azure:myorg.cognitiveservices.azure.com/gpt-4o"),
            "the judge's route wins, in the same shape `dispatch complete` stamps"
        );
    }

    #[test]
    fn crew_route_label_falls_back_to_a_remote_probe_when_the_judge_is_local() {
        // A mixed crew is still a remote run — the operator is spending
        // tokens off-machine and the live view must say so.
        let roles = ResolvedReviewRoles {
            probes: vec![
                staffing("darkmux:local-probe"),
                remote_staffing("gpt-4o-mini", "probes.cognitiveservices.azure.com"),
            ],
            judge: staffing("darkmux:local-judge"),
            verify: None,
            request_changes: false,
            warnings: Vec::new(),
        };
        assert_eq!(
            crew_route_label(&roles).as_deref(),
            Some("azure:probes.cognitiveservices.azure.com/gpt-4o-mini")
        );
    }

    #[test]
    fn crew_model_summary_dedupes_and_sorts_every_seat_model() {
        // (#1512, #1513 review) `ResolvedReviewRoles::judge` is required
        // (never absent), so "an empty crew has no summary" is no longer a
        // reachable state to test — a `ResolvedReviewRoles` always names at
        // least the judge's model.
        let roles = ResolvedReviewRoles {
            probes: vec![staffing("zzz-probe"), staffing("aaa-probe")],
            judge: staffing("zzz-probe"),
            verify: None,
            request_changes: false,
            warnings: Vec::new(),
        };

        let summary = crew_model_summary(&roles).expect("non-empty roles have a summary");
        assert_eq!(summary, "darkmux:aaa-probe, darkmux:zzz-probe", "sorted + deduped: {summary}");
    }

    // ── #1365-equivalent: mission/phase terminal-status finalization,
    // reusing the GENERIC `mission_launch::ensure_mission_and_phases`
    // instance-minting primitive instead of the retired bespoke
    // `build_mission_for_review` (#1284 Packet 4b) ─────────────────────────

    /// RAII guard: points `DARKMUX_CREW_DIR`/`DARKMUX_FLOWS_DIR` at fresh
    /// `TempDir`s for the test's duration. Mirrors the retired `pr_review.rs`
    /// test module's own `CrewDirGuard`.
    struct CrewDirGuard {
        prev_crew: Option<String>,
        prev_flows: Option<String>,
        _tmp_crew: tempfile::TempDir,
        _tmp_flows: tempfile::TempDir,
    }
    impl CrewDirGuard {
        fn new() -> Self {
            let tmp_crew = tempfile::TempDir::new().unwrap();
            let tmp_flows = tempfile::TempDir::new().unwrap();
            let prev_crew = std::env::var("DARKMUX_CREW_DIR").ok();
            let prev_flows = std::env::var("DARKMUX_FLOWS_DIR").ok();
            // SAFETY: every caller is `#[serial_test::serial]`.
            unsafe {
                std::env::set_var("DARKMUX_CREW_DIR", tmp_crew.path());
                std::env::set_var("DARKMUX_FLOWS_DIR", tmp_flows.path());
            }
            Self { prev_crew, prev_flows, _tmp_crew: tmp_crew, _tmp_flows: tmp_flows }
        }
    }
    impl Drop for CrewDirGuard {
        fn drop(&mut self) {
            // SAFETY: every caller is `#[serial_test::serial]`.
            unsafe {
                match &self.prev_crew {
                    Some(v) => std::env::set_var("DARKMUX_CREW_DIR", v),
                    None => std::env::remove_var("DARKMUX_CREW_DIR"),
                }
                match &self.prev_flows {
                    Some(v) => std::env::set_var("DARKMUX_FLOWS_DIR", v),
                    None => std::env::remove_var("DARKMUX_FLOWS_DIR"),
                }
            }
        }
    }

    fn phase_status(mission_id: &str, phase_id: &str) -> String {
        let path = crew::lifecycle::phase_path(mission_id, phase_id);
        let text = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("reading {}: {e}", path.display()));
        let v: Value = serde_json::from_str(&text).unwrap();
        v["status"].as_str().unwrap().to_string()
    }

    fn mission_status_str(mission_id: &str) -> String {
        let path = crew::lifecycle::mission_path(mission_id);
        let text = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("reading {}: {e}", path.display()));
        let v: Value = serde_json::from_str(&text).unwrap();
        v["status"].as_str().unwrap().to_string()
    }

    /// Mint a FRESH review-config run via the SAME provenance-bearing
    /// primitive `run_dispatch` uses, returning
    /// `(mission_id, [investigate, adjudicate, report])`. (#1503) Every
    /// call mints a NEW run — never reuses or reopens a prior one, even
    /// for the same `case_id`; `spec` is what still groups same-case runs.
    fn mint_review_instance(case_id: &str) -> (String, [String; 3]) {
        let config = crew::mission_config::load("review").expect("review is embedded").config;
        let mut id_input: BTreeMap<String, Value> = BTreeMap::new();
        id_input.insert("case_id".to_string(), Value::String(case_id.to_string()));
        let mission_id = mission_launch::mint_run_id("review").unwrap();
        let spec = crew::types::MissionSpec {
            config_id: "review".to_string(),
            inputs_fingerprint: mission_launch::spec_fingerprint(&id_input).unwrap(),
            origin: Some(crew::types::MissionSpecOrigin::Builtin),
        };
        // Same call shape as `run_dispatch` (must-fix 2 provenance).
        let description = format!("PR review — {case_id} (crew `test-crew`)");
        let real_phase_ids = mission_launch::ensure_mission_and_phases_with_provenance(
            &mission_id,
            &config,
            Some(&description),
            Some(spec),
        )
        .unwrap();
        // `ensure_mission_and_phases_with_provenance` mints phases Planned.
        // `run_dispatch`'s REAL flow no longer flips all three eagerly
        // (#1400 — that was the bug: every phase pulsed "running" from
        // second zero); each starts LAZILY instead, via
        // `mission_launch::lazy_start_phase_for_step` fired from the
        // `persist` closure as the graph actually reaches it (see
        // `review_phases_start_lazily_not_all_at_mint`, right below this
        // fixture, for a direct test of that). This fixture still starts
        // every phase up front because the tests THAT USE IT
        // (`finalize_review_mission_*`) are exercising *finalization*
        // behavior, which needs "every phase already Running" as its
        // precondition, not the start-timing #1400 is about. Every mint is
        // fresh (#1503), so `phase_start` always succeeds — no reuse
        // collision to silently skip anymore.
        for real_id in real_phase_ids.values() {
            crew::lifecycle::phase_start(real_id).unwrap();
        }
        (
            mission_id,
            [
                real_phase_ids["investigate"].clone(),
                real_phase_ids["adjudicate"].clone(),
                real_phase_ids["report"].clone(),
            ],
        )
    }

    /// (#1400) A fresh review mint leaves all three phases `Planned`, and
    /// each starts ONLY when `mission_launch::lazy_start_phase_for_step`
    /// is called for a step belonging to it — mirroring EXACTLY what
    /// `run_dispatch`'s `persist` closure does as `run_review_graph`
    /// actually reaches each phase's steps (bundle/probe/dedup under
    /// `investigate`, judge under `adjudicate`, verify/synthesis under
    /// `report`). This is the direct regression test for the live-observed
    /// bug: "all three phases pulse running from second zero."
    #[test]
    #[serial_test::serial]
    fn review_phases_start_lazily_not_all_at_mint() {
        let _guard = CrewDirGuard::new();
        let config = crew::mission_config::load("review").expect("review is embedded").config;
        let mission_id = mission_launch::mint_run_id("review").unwrap();
        let real_phase_ids = mission_launch::ensure_mission_and_phases_with_provenance(
            &mission_id,
            &config,
            Some("PR review test — lazy start"),
            None,
        )
        .unwrap();

        // Fresh mint: every phase starts Planned, none pulse Running.
        for real_id in real_phase_ids.values() {
            assert_eq!(phase_status(&mission_id, real_id), "planned");
        }

        // Simulate the real dispatch order — investigate's bundle step is
        // always the graph's first ready step, adjudicate's judge step
        // only becomes ready once investigate's dedup completes, report's
        // verify step only once adjudicate's judge completes.
        let mut started: std::collections::HashSet<String> = std::collections::HashSet::new();
        mission_launch::lazy_start_phase_for_step(
            &mission_id,
            &real_phase_ids["investigate"],
            crew::types::NodeStatus::Running,
            &mut started,
        );
        assert_eq!(phase_status(&mission_id, &real_phase_ids["investigate"]), "running");
        assert_eq!(
            phase_status(&mission_id, &real_phase_ids["adjudicate"]),
            "planned",
            "adjudicate hasn't been reached yet — must not pulse alongside investigate"
        );
        assert_eq!(
            phase_status(&mission_id, &real_phase_ids["report"]),
            "planned",
            "report hasn't been reached yet — must not pulse alongside investigate"
        );

        mission_launch::lazy_start_phase_for_step(
            &mission_id,
            &real_phase_ids["adjudicate"],
            crew::types::NodeStatus::Running,
            &mut started,
        );
        assert_eq!(phase_status(&mission_id, &real_phase_ids["adjudicate"]), "running");
        assert_eq!(
            phase_status(&mission_id, &real_phase_ids["report"]),
            "planned",
            "report still hasn't been reached"
        );

        mission_launch::lazy_start_phase_for_step(
            &mission_id,
            &real_phase_ids["report"],
            crew::types::NodeStatus::Running,
            &mut started,
        );
        assert_eq!(phase_status(&mission_id, &real_phase_ids["report"]), "running");
    }

    /// (#1620) Starting a phase CLOSES the ones before it, at their earned
    /// outcome — the counterpart the lazy-start path never had.
    ///
    /// Without it, every touched phase sat `Running` until the whole mission
    /// finalized and reconciled them in bulk, so the board read `0/3` for the
    /// entire run and then jumped to `3/3`. On the live board a review whose
    /// judge was working showed `0/3` with investigate AND adjudicate both
    /// `Running` — two phases live at once, contradicting the strictly-linear
    /// model (#1341), and a progress column that could not distinguish
    /// two-thirds-done from never-started.
    ///
    /// Every existing test asserted the FINALIZED state (all phases complete),
    /// which is why a whole run's worth of missing intermediate state passed
    /// 2684 green tests. This one asserts the middle.
    #[test]
    #[serial_test::serial]
    fn advancing_a_phase_closes_the_ones_before_it() {
        let _guard = CrewDirGuard::new();
        let config = crew::mission_config::load("review").expect("review is embedded").config;
        let mission_id = mission_launch::mint_run_id("review").unwrap();
        let real_phase_ids = mission_launch::ensure_mission_and_phases_with_provenance(
            &mission_id,
            &config,
            Some("PR review test — advance closes prior"),
            None,
        )
        .unwrap();
        let order: Vec<String> = ["investigate", "adjudicate", "report"]
            .iter()
            .map(|k| real_phase_ids[*k].clone())
            .collect();
        let mut started = std::collections::HashSet::new();
        let mut closed = std::collections::HashSet::new();

        // Investigate goes live. Nothing precedes it, so nothing closes.
        assert!(mission_launch::lazy_start_phase_for_step(
            &mission_id,
            &order[0],
            crew::types::NodeStatus::Running,
            &mut started,
        ));
        mission_launch::lazy_close_prior_phases(&mission_id, &order, &order[0], &mut closed);
        assert_eq!(phase_status(&mission_id, &order[0]), "running");

        // Adjudicate goes live. Investigate has no persisted steps here, so the
        // conservative branch leaves it alone rather than inventing a verdict —
        // an early WRONG terminal is worse than a late correct one.
        assert!(mission_launch::lazy_start_phase_for_step(
            &mission_id,
            &order[1],
            crew::types::NodeStatus::Running,
            &mut started,
        ));
        mission_launch::lazy_close_prior_phases(&mission_id, &order, &order[1], &mut closed);

        // Now give investigate a genuinely finished step and advance again.
        // (Re-running the close is the real-world shape: the persist closure
        // fires per step, so the same advance is re-evaluated.)
        let step = crew::types::Step {
            id: format!("{}-s1", order[0]),
            task_id: format!("{}-t1", order[0]),
            gate: None,
            kind: "procedural.noop".to_string(),
            status: crew::types::NodeStatus::Complete,
            config: serde_json::Value::Null,
            started_ts: None,
            completed_ts: None,
            output: None,
        };
        crew::lifecycle::save_step(&mission_id, &order[0], &step).unwrap();
        closed.remove(&order[0]);
        mission_launch::lazy_close_prior_phases(&mission_id, &order, &order[1], &mut closed);

        assert_eq!(
            phase_status(&mission_id, &order[0]),
            "complete",
            "investigate must close when adjudicate is live — phases are strictly linear (#1341), \
             so reaching one means the earlier ones are over"
        );
        assert_eq!(
            phase_status(&mission_id, &order[1]),
            "running",
            "and exactly ONE phase is live at a time"
        );
        assert_eq!(phase_status(&mission_id, &order[2]), "planned");
    }

    /// (#1432 item 2) The review launcher's mint path threads each phase's
    /// `display_name` from the embedded review config onto the persisted
    /// `Phase` — so the most-viewed mission kind shows "Investigate" /
    /// "Adjudicate" / "Report" in the timeline header, not the raw
    /// `review-<case>-investigate` id. Regression guard: the operator saw
    /// `display_name: None` live (phone repro #2), and this pins the fix so
    /// a future mint-path refactor cannot silently drop the threading again.
    #[test]
    #[serial_test::serial]
    fn review_mint_threads_phase_display_names_from_config() {
        let _guard = CrewDirGuard::new();
        let (mission_id, phase_ids) = mint_review_instance("owner/repo@dispnames");
        let expected = ["Investigate", "Adjudicate", "Report"];
        for (phase_id, want) in phase_ids.iter().zip(expected) {
            let path = crew::lifecycle::phase_path(&mission_id, phase_id);
            let text = std::fs::read_to_string(&path).unwrap();
            let v: Value = serde_json::from_str(&text).unwrap();
            assert_eq!(
                v["display_name"].as_str(),
                Some(want),
                "phase {phase_id} must carry its config display_name, not None"
            );
        }
    }

    #[test]
    #[serial_test::serial]
    fn finalize_review_mission_completes_phases_and_closes_mission_on_clean_success() {
        let _guard = CrewDirGuard::new();
        let (mission_id, phase_ids) = mint_review_instance("owner/repo@deadbeef");
        for phase_id in &phase_ids {
            assert_eq!(phase_status(&mission_id, phase_id), "running", "fresh phases start Running");
        }

        let result: Result<ReviewEnvelope> = Ok(ReviewEnvelope { degenerate: None, ..Default::default() });
        let ids: [&str; 3] = [&phase_ids[0], &phase_ids[1], &phase_ids[2]];
        finalize_review_mission(&mission_id, &ids, &result);

        for phase_id in &phase_ids {
            assert_eq!(phase_status(&mission_id, phase_id), "complete", "clean run completes every phase");
        }
        assert_eq!(mission_status_str(&mission_id), "finalized", "clean run finalizes the mission");
    }

    #[test]
    #[serial_test::serial]
    fn finalize_review_mission_abandons_phases_on_degenerate_ok_result() {
        let _guard = CrewDirGuard::new();
        let (mission_id, phase_ids) = mint_review_instance("owner/repo@degenerate");

        let result: Result<ReviewEnvelope> = Ok(ReviewEnvelope {
            degenerate: Some("no bundles produced from the diff".to_string()),
            ..Default::default()
        });
        let ids: [&str; 3] = [&phase_ids[0], &phase_ids[1], &phase_ids[2]];
        finalize_review_mission(&mission_id, &ids, &result);

        for phase_id in &phase_ids {
            let status = phase_status(&mission_id, phase_id);
            assert_eq!(status, "abandoned", "a degenerate Ok(env) must abandon, never complete: {status}");
        }
        assert_eq!(mission_status_str(&mission_id), "finalized");
    }

    #[test]
    #[serial_test::serial]
    fn finalize_review_mission_abandons_phases_on_hard_error() {
        let _guard = CrewDirGuard::new();
        let (mission_id, phase_ids) = mint_review_instance("owner/repo@harderr");

        let result: Result<ReviewEnvelope> = Err(anyhow!("probe dispatch failed: connection refused"));
        let ids: [&str; 3] = [&phase_ids[0], &phase_ids[1], &phase_ids[2]];
        finalize_review_mission(&mission_id, &ids, &result);

        for phase_id in &phase_ids {
            assert_eq!(phase_status(&mission_id, phase_id), "abandoned");
        }
        assert_eq!(mission_status_str(&mission_id), "finalized");
    }

    // ── #2131 review round 2, MUST-FIX 3 — ported from the retired ──
    // `review_finalize_guard.rs`'s own test module (deleted, unported, by
    // #2131's extraction). The three tests directly above already pin
    // `finalize_review_mission`'s OWN behavior (called bare); these three
    // pin the SAME behavior through `LaunchFinalizeGuard::close`/`Drop` —
    // i.e., that `launch()`'s real construction (`guard.close(|| {
    // finalize_review_mission(...) })`, the abort-writer closure built at
    // `LaunchFinalizeGuard::new`) actually reaches that function, and that
    // `close` truly disarms so `Drop` never runs it twice. A future
    // refactor that rewires the guard's closures without touching
    // `finalize_review_mission` itself would sail through the three tests
    // above while still shipping the #2124 regression this guard exists to
    // prevent.

    /// (#2124, unit test 1a, ported) `close()` on a synthesized error
    /// (mirroring a caught worker-thread panic, no signal involved) writes
    /// the mission's terminal record and abandons every phase — through
    /// `LaunchFinalizeGuard::close`, not the bare function.
    #[test]
    #[serial_test::serial]
    fn guard_close_writes_the_terminal_record_after_a_simulated_error() {
        let _guard = CrewDirGuard::new();
        let (mission_id, phase_ids) = mint_review_instance("owner/repo@guard-close-error");
        let abort_mission_id = mission_id.clone();
        let abort_phase_ids = phase_ids.clone();
        let mut guard = LaunchFinalizeGuard::new(move || {
            let refs: Vec<&str> = abort_phase_ids.iter().map(String::as_str).collect();
            let err: Result<ReviewEnvelope> = Err(anyhow!("mission aborted — unreachable in this test"));
            finalize_review_mission(&abort_mission_id, &refs, &err);
        });

        let result: Result<ReviewEnvelope> = Err(anyhow!("simulated worker-thread panic"));
        let refs: Vec<&str> = phase_ids.iter().map(String::as_str).collect();
        guard.close(|| finalize_review_mission(&mission_id, &refs, &result));

        assert_eq!(mission_status_str(&mission_id), "finalized", "close() must finalize the mission");
        for phase_id in &phase_ids {
            assert_eq!(phase_status(&mission_id, phase_id), "abandoned");
        }
    }

    /// (#2124, unit test 1b, ported) `Drop` — the ABORT path, never
    /// explicitly closed — writes the SAME terminal record. Simulates a
    /// SIGTERM arriving mid-run (`darkmux_types::interrupt::
    /// simulate_sigterm_for_test`, the real handler's exact code path
    /// minus an actual OS signal) and then drops the guard without ever
    /// calling `close()` — mirroring `run_dispatch`'s supervisor loop
    /// abandoning an interrupted worker thread's handle. This is the
    /// #2124 regression proof: without a working `Drop` fallback, a
    /// SIGTERM caught mid-review leaves the mission stuck Active forever.
    #[test]
    #[serial_test::serial]
    fn guard_drop_writes_the_terminal_record_after_a_simulated_signal() {
        let _guard = CrewDirGuard::new();
        darkmux_types::interrupt::reset_for_test();
        darkmux_types::child_registry::reset_for_test();
        let (mission_id, phase_ids) = mint_review_instance("owner/repo@guard-drop-signal");

        {
            let abort_mission_id = mission_id.clone();
            let abort_phase_ids = phase_ids.clone();
            let guard = LaunchFinalizeGuard::new(move || {
                let refs: Vec<&str> = abort_phase_ids.iter().map(String::as_str).collect();
                let err: Result<ReviewEnvelope> = Err(anyhow!(
                    "mission launch review: mission aborted — the launcher exited before a \
                     terminal outcome was recorded (a signal, a panic, or an early return this \
                     guard did not expect)"
                ));
                finalize_review_mission(&abort_mission_id, &refs, &err);
            });
            darkmux_types::interrupt::simulate_sigterm_for_test();
            assert!(darkmux_types::interrupt::is_set(), "the simulated SIGTERM must set the flag");
            // Deliberately no `guard.close(...)` call — the guard goes out
            // of scope here still armed, exercising `Drop`. The child
            // registry is empty (reset just above), so `Drop`'s
            // `kill_all` call is a documented no-op — this proves the
            // RECORD-WRITING half of the fallback without needing a real
            // child pid or a subprocess to observe anything being killed.
            drop(guard);
        }

        assert_eq!(
            mission_status_str(&mission_id),
            "finalized",
            "Drop's fallback must finalize the mission even though close() was never called"
        );
        for phase_id in &phase_ids {
            assert_eq!(
                phase_status(&mission_id, phase_id),
                "abandoned",
                "an abort-path finalize must abandon every phase, never complete one"
            );
        }
        darkmux_types::interrupt::reset_for_test();
        darkmux_types::child_registry::reset_for_test();
    }

    /// (#2124, ported) `close()` disarms the guard — a normal completion
    /// followed by the guard going out of scope must NOT double-finalize
    /// (which `finalize_mission`'s own idempotence would mask as a silent
    /// no-op, not a crash, so this proves the disarm rather than assuming
    /// it).
    #[test]
    #[serial_test::serial]
    fn guard_close_disarms_so_drop_never_double_finalizes() {
        let _guard = CrewDirGuard::new();
        let (mission_id, phase_ids) = mint_review_instance("owner/repo@guard-disarm");
        let abort_mission_id = mission_id.clone();
        let abort_phase_ids = phase_ids.clone();
        let mut guard = LaunchFinalizeGuard::new(move || {
            let refs: Vec<&str> = abort_phase_ids.iter().map(String::as_str).collect();
            let err: Result<ReviewEnvelope> = Err(anyhow!("mission aborted — unreachable in this test"));
            finalize_review_mission(&abort_mission_id, &refs, &err);
        });
        let result: Result<ReviewEnvelope> = Ok(ReviewEnvelope { degenerate: None, ..Default::default() });
        let refs: Vec<&str> = phase_ids.iter().map(String::as_str).collect();
        guard.close(|| finalize_review_mission(&mission_id, &refs, &result));
        assert_eq!(mission_status_str(&mission_id), "finalized");
        drop(guard);
        // No panic, no second write to observe here beyond the same
        // terminal status persisting — `finalize_mission`'s own refusal
        // classification (Benign on an already-Finalized mission) would
        // make a stray second call quiet either way, so the REAL proof
        // this test wants is that `armed` is false, checked structurally
        // (`launch_guard.rs`'s own generic close/drop unit tests already
        // pin the disarm mechanically; this pins that THIS closure's
        // wiring benefits from it).
        assert_eq!(mission_status_str(&mission_id), "finalized");
    }

    /// (#1503) The review-shaped RERUN path: a re-run of the SAME case now
    /// mints a genuinely NEW run — never reopens the prior terminal
    /// record — since AI work is non-deterministic and two dispatches of
    /// the same case are two different runs. Replaces the pre-#1503
    /// `review_rerun_of_the_same_case_reopens_the_terminal_instance`
    /// (which asserted the removed reopen behavior: same case -> same
    /// mission id, reopened rather than double-minted). The two runs still
    /// GROUP via `Mission.spec` (same case -> same fingerprint), and each
    /// carries the same case-bearing provenance (must-fix 2) independently.
    #[test]
    #[serial_test::serial]
    fn review_rerun_of_the_same_case_mints_a_new_run_that_groups_with_the_prior_one() {
        let _guard = CrewDirGuard::new();
        let case = "owner/repo@rerun4b";

        let (mission_id, phase_ids) = mint_review_instance(case);
        let clean: Result<ReviewEnvelope> = Ok(ReviewEnvelope { degenerate: None, ..Default::default() });
        let ids: [&str; 3] = [&phase_ids[0], &phase_ids[1], &phase_ids[2]];
        finalize_review_mission(&mission_id, &ids, &clean);
        assert_eq!(mission_status_str(&mission_id), "finalized");

        // Re-run the SAME case through the same mint path.
        let (mission_id2, _) = mint_review_instance(case);
        assert_ne!(mission_id2, mission_id, "same case must mint a NEW run id, never reuse the prior one (#1503)");
        assert_eq!(mission_status_str(&mission_id2), "active", "the new run starts Active");
        // The prior (finalized) run is left completely untouched.
        assert_eq!(mission_status_str(&mission_id), "finalized", "a rerun must never mutate the prior terminal run");

        // Case-bearing provenance (must-fix 2): the board row names the
        // case + crew, never the config's transcription paragraph — on
        // BOTH runs, independently.
        let describe = |id: &str| -> String {
            let text = std::fs::read_to_string(crew::lifecycle::mission_path(id)).unwrap();
            let v: Value = serde_json::from_str(&text).unwrap();
            v["description"].as_str().unwrap().to_string()
        };
        let expected_description = format!("PR review — {case} (crew `test-crew`)");
        assert_eq!(describe(&mission_id), expected_description);
        assert_eq!(describe(&mission_id2), expected_description);

        // (#1503) The two runs still GROUP: same case -> same `spec`.
        let spec_of = |id: &str| -> Value {
            let text = std::fs::read_to_string(crew::lifecycle::mission_path(id)).unwrap();
            serde_json::from_str::<Value>(&text).unwrap()["spec"].clone()
        };
        assert_eq!(spec_of(&mission_id), spec_of(&mission_id2), "same-case reruns must group via `spec`");
    }

    /// (#1417) Direct regression test for the mint-then-strand bug: a
    /// user-tier `review.json` whose declared phase ids don't include
    /// `investigate`/`adjudicate`/`report` (the exact repro in the issue —
    /// a renamed phase id still passes `MissionConfig::validate`, since
    /// that only checks for empty/duplicate ids, never the review
    /// launcher's specific phase vocabulary) must fail `run_dispatch`
    /// WITHOUT ever minting the Mission. Before the fix,
    /// `ensure_mission_and_phases_with_provenance` ran first and minted an
    /// Active mission with 3 Planned phases, then `real_phase("investigate")`
    /// errored and `run_dispatch` returned — leaving that mission
    /// permanently Active (`darkmux mission status` never recovers it short
    /// of the 14-day stale-active drift rule).
    #[test]
    #[serial_test::serial]
    fn run_dispatch_with_a_renamed_phase_id_fails_before_minting_the_mission() {
        let _guard = CrewDirGuard::new();

        // A minimal profiles.json that lets `resolve_review_roles` (#1512,
        // #1513 review) resolve every role the embedded review.json
        // declares — enough to get `run_dispatch` past staffing resolution
        // and bundle-building (an empty worktree + empty diff yields an
        // empty, valid bundle set) and up to the phase-id validation this
        // test targets. No model dispatch is ever reached.
        let profiles_dir = tempfile::TempDir::new().unwrap();
        let profiles_path = profiles_dir.path().join("profiles.json");
        std::fs::write(
            &profiles_path,
            r#"{
                "schema_version": "1.5",
                "profiles": {
                    "test-profile": { "models": [ { "id": "test-model", "n_ctx": 8000 } ] }
                },
                "default_profile": "test-profile",
                "crews": {
                    "test-crew": {
                        "seats": {
                            "review-probe": [ { "profile": "test-profile", "k": 1 } ],
                            "review-judge": [ { "profile": "test-profile", "passes": 1 } ]
                        }
                    }
                }
            }"#,
        )
        .unwrap();

        let worktree_dir = tempfile::TempDir::new().unwrap();

        // The embedded `review` config, with its first declared phase id
        // renamed — the exact user-tier-typo repro from #1417.
        let mut config = crew::mission_config::load("review").expect("review is embedded").config;
        assert_eq!(
            config.phases[0].id, "investigate",
            "test assumes the embedded review.json's phase order"
        );
        config.phases[0].id = "investigate-renamed".to_string();

        let case_id = "owner/repo@renamed1417";
        let mut collected: BTreeMap<String, Value> = BTreeMap::new();
        collected.insert("case_id".to_string(), Value::String(case_id.to_string()));
        collected.insert("crew".to_string(), Value::String("test-crew".to_string()));
        collected.insert("worktree".to_string(), Value::String(worktree_dir.path().display().to_string()));
        collected.insert("profiles".to_string(), Value::String(profiles_path.display().to_string()));

        let diff_file = worktree_dir.path().join("unused.diff");
        let result = run_dispatch(&config, &collected, &diff_file, "", 60, crew::types::MissionSpecOrigin::Builtin);

        let err = result.expect_err("a renamed phase id must fail, not silently interpret");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("does not declare a phase with id `investigate`"),
            "expected the phase-id validation error, got: {msg}"
        );

        // The regression: the Mission must NEVER have been minted. Before
        // the #1417 fix, `ensure_mission_and_phases_with_provenance` ran
        // BEFORE this validation and would have already written
        // mission.json (Active, 3 Planned phases) to disk. (#1503) A run id
        // is minted, not derived, so this can no longer re-derive the id to
        // check — instead assert NOTHING landed under the isolated
        // missions dir at all.
        let missions_dir = crew::loader::missions_dir();
        assert!(
            !missions_dir.is_dir() || std::fs::read_dir(&missions_dir).unwrap().next().is_none(),
            "no mission.json may exist — a failed pre-graph validation must never strand a minted mission"
        );
    }

    /// (#1538 follow-up) THE regression guard: a differently-NAMED variant
    /// must execute ITS OWN document, not the built-in.
    ///
    /// #1538 made routing structural, so a variant reaches this launcher —
    /// but `run_dispatch` still resolved the built-in by id
    /// (`mission_config::load("review")`). A `review-lean` launch therefore
    /// routed correctly and then ran the built-in THREE-probe graph, with the
    /// variant's own probe tasks ignored and no warning anywhere.
    ///
    /// This must exercise `run_dispatch` — the function that CHOOSES the
    /// document. An earlier version of this guard called the graph builder
    /// directly with the renamed doc, which cannot fail: the builder never
    /// reads `config.id`, so it passed against the buggy code too. Testing
    /// the choice means calling the chooser.
    ///
    /// Pruned to ONE probe task, so the seat count in the envelope's own
    /// staffing snapshot distinguishes the documents: 1 (the variant, correct)
    /// vs 3 (the built-in, the bug).
    #[test]
    #[serial_test::serial]
    fn run_dispatch_uses_the_launched_variant_document_not_the_builtin() {
        let _guard = CrewDirGuard::new();

        let profiles_dir = tempfile::TempDir::new().unwrap();
        let profiles_path = profiles_dir.path().join("profiles.json");
        std::fs::write(
            &profiles_path,
            r#"{
                "schema_version": "1.5",
                "profiles": {
                    "test-profile": { "models": [ { "id": "test-model", "n_ctx": 8000 } ] }
                },
                "default_profile": "test-profile"
            }"#,
        )
        .unwrap();

        let worktree_dir = tempfile::TempDir::new().unwrap();

        // A stored variant: the embedded document under a DIFFERENT id, with
        // two of its three probe tasks removed.
        let mut config = crew::mission_config::load("review").expect("review is embedded").config;
        config.id = "review-lean".to_string();
        let investigate = &mut config.phases[0];
        assert_eq!(investigate.id, "investigate", "test assumes the embedded phase order");
        let keep = "review-probe-high-task";
        investigate
            .tasks
            .retain(|t| !t.id.starts_with("review-probe-") || t.id == keep);
        // The dedup task fans in from the probe tasks; drop the pruned edges
        // so the document stays internally consistent.
        for t in investigate.tasks.iter_mut() {
            if t.id == "review-dedup-task" {
                t.depends_on.retain(|d| !d.starts_with("review-probe-") || d == keep);
            }
        }
        assert_eq!(
            investigate.tasks.iter().filter(|t| t.id.starts_with("review-probe-")).count(),
            1,
            "the variant must declare exactly one probe task"
        );

        let mut collected: BTreeMap<String, Value> = BTreeMap::new();
        collected.insert("case_id".to_string(), Value::String("owner/repo@lean".to_string()));
        collected.insert("worktree".to_string(), Value::String(worktree_dir.path().display().to_string()));
        collected.insert("profiles".to_string(), Value::String(profiles_path.display().to_string()));

        let diff_file = worktree_dir.path().join("unused.diff");
        // Empty diff -> empty bundle set -> a degenerate run with zero model
        // dispatches. The staffing snapshot is still resolved and recorded,
        // which is all this assertion needs.
        let env = run_dispatch(&config, &collected, &diff_file, "", 60, crew::types::MissionSpecOrigin::Builtin)
            .expect("an empty-diff variant run still produces an envelope");

        let staffing = env.staffing.as_ref().expect("a dispatching run records its staffing");
        assert_eq!(
            staffing.probes.len(),
            1,
            "the LAUNCHED variant declares one probe seat; resolving three means `run_dispatch` \
             loaded the built-in document instead of the one that was launched (#1538). Seats: {:?}",
            staffing.probes.iter().map(|p| &p.name).collect::<Vec<_>>()
        );
    }

    /// (#1504) The COMPLEMENTARY strand window #1417 didn't close: a
    /// failure AFTER `ensure_mission_and_phases_with_provenance` has
    /// already written `mission.json` (a partial mint — a later
    /// `save_phase` call fails) used to leave a bare `?` with no reconcile,
    /// stranding a fresh, permanently-Active mission. Since #1503 mints a
    /// UNIQUE run id per launch, a repeated failing launch no longer
    /// converges onto one reused instance the way the old derive-from-inputs
    /// id used to — each failure needs its own reconcile or it strands its
    /// OWN mission. Forces the failure deterministically (no OS-permission
    /// tricks): pre-occupies the `phases/` subdir path with a plain file so
    /// `fs::create_dir_all` can't create a directory there, failing the
    /// very first `save_phase` call right after `save_mission` already
    /// succeeded.
    #[test]
    #[serial_test::serial]
    fn post_mint_phase_write_failure_reconciles_via_mint_failure_helper_not_stranded_active() {
        let _guard = CrewDirGuard::new();
        let config = crew::mission_config::load("review").expect("review is embedded").config;
        let mission_id = "review-post-mint-strand";

        let mission_dir = crew::lifecycle::mission_path(mission_id).parent().unwrap().to_path_buf();
        std::fs::create_dir_all(&mission_dir).unwrap();
        std::fs::write(mission_dir.join("phases"), b"blocks phase dir creation").unwrap();

        let err = mission_launch::ensure_mission_and_phases_with_provenance(mission_id, &config, None, None)
            .unwrap_err();
        assert_eq!(
            mission_status_str(mission_id),
            "active",
            "sanity: mission.json WAS written before the phase save failed"
        );

        // Exactly what `run_dispatch`'s error arm now does on this failure.
        crew::lifecycle::reconcile_mint_failure(
            mission_id,
            &format!("mission launch review errored during mint: {err:#}"),
        );

        assert_eq!(
            mission_status_str(mission_id),
            "finalized",
            "a partial mint must reconcile to terminal — one fresh mission, terminal, never an \
             accumulating Active row (#1504)"
        );
    }

    // ── structural guard (ported from the retired `pr_review.rs` test,
    // adapted: the shared launcher's TWO callers are now `review.rs`'s own
    // `build_review_graph`/`run_review_graph` callers — this module and
    // `review_bench.rs` — never a bespoke third graph-builder) ───────────

    #[test]
    fn mission_launch_review_and_review_bench_construct_graphs_through_the_same_launcher() {
        const THIS_SRC: &str = include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/src/mission_launch_review.rs"));
        const REVIEW_BENCH_SRC: &str = include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/crates/darkmux-lab/src/lab/review_bench.rs"
        ));

        fn defined_fn_name(line: &str) -> Option<&str> {
            let t = line.trim_start();
            let t = t.strip_prefix("pub(crate) ").or_else(|| t.strip_prefix("pub ")).unwrap_or(t);
            let t = t.strip_prefix("async ").unwrap_or(t);
            let rest = t.strip_prefix("fn ")?;
            let end = rest.find(['(', '<', ' '])?;
            Some(&rest[..end])
        }

        // (#1538 follow-up) TWO accepted entry points, one shared core.
        // `build_review_graph` is the convenience wrapper that resolves the
        // BUILT-IN document by id and then calls `build_review_graph_from_
        // config`; the bench uses it because it always benches the built-in.
        // Production cannot: it must build from the document the operator
        // actually LAUNCHED (a `review-lean` variant must run its own graph,
        // not the built-in's), so it calls the shared core directly. Both
        // still reach the same builder — which is the invariant this test
        // exists for. The real teeth are the `bespoke_graph_fns` check below:
        // neither file may define a graph-builder of its own.
        let build_needles =
            [concat!("build_review", "_graph("), concat!("build_review", "_graph_from_config(")];
        let run_needle = concat!("run_review", "_graph(");
        for (label, src) in [("mission_launch_review.rs", THIS_SRC), ("review_bench.rs", REVIEW_BENCH_SRC)] {
            assert!(
                build_needles.iter().any(|n| src.contains(n)),
                "{label} must dispatch through review::build_review_graph or its \
                 build_review_graph_from_config core"
            );
            assert!(src.contains(run_needle), "{label} must dispatch through review::run_review_graph");
            let bespoke_graph_fns: Vec<&str> = src
                .lines()
                .filter_map(defined_fn_name)
                .filter(|name| (name.starts_with("build_") || name.starts_with("default_")) && name.ends_with("_graph"))
                .collect();
            assert!(
                bespoke_graph_fns.is_empty(),
                "{label} defines its own graph-building fn(s) {bespoke_graph_fns:?} — the \
                 config-driven launcher lives in ONE place (darkmux_lab::lab::review)"
            );
        }
    }
}
