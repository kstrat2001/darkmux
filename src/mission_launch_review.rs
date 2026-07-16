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
//! used to own, wired through the same generic instance-minting primitives
//! (`mission_launch::derive_mission_id`, `mission_launch::
//! ensure_mission_and_phases`) every other config-launched mission uses —
//! `build_review_graph` still calls `mission_config::interpret` itself
//! internally (it needs the crew-resolved `probe_seats` expansion this
//! module's caller doesn't have until AFTER crew resolution), so no double
//! interpretation happens.
//!
//! **Gate semantics.** Review has no operator sign-off gate — unlike
//! `coder-phase`, there is nothing for `mission ship`/`mission abort` to
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
use anyhow::{anyhow, bail, Context, Result};
use crew::mission_config::MissionConfig;
use darkmux_crew::dispatch::build_dispatch_record_with_payload;
use darkmux_crew::single_shot::{
    single_shot_chat, single_shot_chat_hosted, HostedSingleShotRequest, SingleShotReply,
    SingleShotRequest,
};
use darkmux_lab::lab::bundle::{
    build_bundles, external_bundles, slice_code, slice_code_probe, BundleSet, FileSource,
};
use darkmux_lab::lab::review::{
    build_review_graph, fingerprint, run_judge_only, run_review_graph, seat_identifier,
    staffing_snapshot, validate_review_crew, BundleInput, ChatCall, ExecMode, LmsCycler,
    ProbeFlag, ReviewEmitter, ReviewEnvelope, ReviewInputs, ReviewStepContext,
};
use darkmux_profiles::crews::{resolve_crew, ResolvedCrew};
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

/// `Bundle` (`darkmux_lab::lab::bundle`) -> `BundleInput` (`darkmux_lab::
/// lab::review`'s shape). Each bundle's line-span pointers are rendered PER
/// SEAT (#1256): `slice_code` (the judge's `// path` raw format) into
/// `code`, `slice_code_probe` (the probe's fenced-code format) into
/// `probe_code`.
fn bundle_inputs_from_set(set: &BundleSet, source: &FileSource) -> Result<Vec<BundleInput>> {
    set.bundles
        .iter()
        .map(|b| {
            let code = slice_code(source, &b.code)
                .with_context(|| format!("slicing code for bundle \"{}\"", b.id))?;
            let probe_code = slice_code_probe(source, &b.code)
                .with_context(|| format!("probe-slicing code for bundle \"{}\"", b.id))?;
            Ok(BundleInput {
                id: b.id.clone(),
                fact_family: b.fact_family.clone(),
                code,
                probe_code,
                facts: b.facts.clone(),
                manifest: b.manifest.clone(),
            })
        })
        .collect()
}

/// (#1247 Part 1) Production wiring of `review::ReviewEmitter` — writes
/// through the real darkmux-flow machinery, the same engagement-scoped
/// stream `crew dispatch`/`phase review` write through. This is the FLEET
/// sink: `mission launch review` drives ONE case per invocation (a real PR
/// review), so its run/step/ruling records belong on the operator's real
/// engagement stream — contrast `darkmux lab review-bench --funnel`'s
/// per-run-local JSONL sink (lab-vs-fleet scope boundary). Failure to
/// record is swallowed (`let _ =`) — same discipline as every other flow
/// emit site; a flow-record write failure must never abort a review.
struct FleetFlowEmitter;

impl ReviewEmitter for FleetFlowEmitter {
    fn emit(&mut self, record: darkmux_flow::FlowRecord) {
        let _ = darkmux_flow::record(record);
    }
}

/// Every distinct model this crew's resolved seats will dispatch,
/// darkmux-namespaced and deduped/sorted for a stable display string. A
/// review run is inherently multi-model (>=1 probe seat plus the judge
/// seat) — unlike a single-model `crew dispatch`, there's no one "the
/// model" for the dispatch bookend's `model` field, so this is the closest
/// honest equivalent: every model actually in play, comma-joined.
fn crew_model_summary(crew: &ResolvedCrew) -> Option<String> {
    let mut ids: Vec<String> = crew
        .seats
        .values()
        .flatten()
        .map(|s| {
            if s.pm.is_remote() {
                s.pm.id.clone()
            } else {
                swap::namespaced_identifier(&s.pm)
            }
        })
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
/// `crew dispatch` uses, with ONE deliberate difference: `source` is
/// overridden from the builder's hardcoded `"crew_dispatch"` to `"review"`
/// — the SAME provenance tag every sibling `review.task/step/ruling` record
/// in this session carries.
fn review_bookend_record(
    level: darkmux_flow::Level,
    action: &str,
    crew_name: &str,
    case_id: &str,
    model: Option<&str>,
    payload: serde_json::Value,
) -> darkmux_flow::FlowRecord {
    let mut record = build_dispatch_record_with_payload(
        level,
        action,
        crew_name,
        case_id,
        model,
        None,
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
/// text) on `Err`. Same shape `crew dispatch` emits around every dispatch,
/// so a production review dispatch opens/closes the SAME liveness edge the
/// viewer's fleet/machine surfaces key on.
fn with_dispatch_bookends(
    emitter: &mut dyn ReviewEmitter,
    case_id: &str,
    crew_name: &str,
    model: Option<&str>,
    start_extra: serde_json::Value,
    f: impl FnOnce(&mut dyn ReviewEmitter) -> Result<ReviewEnvelope>,
) -> Result<ReviewEnvelope> {
    let mut sink = EmitterSink(emitter);
    let case_id_owned = case_id.to_string();
    let crew_owned = crew_name.to_string();
    let model_owned = model.map(str::to_string);
    let on_abort = move |_id: &str, _kind: &str| {
        review_bookend_record(
            darkmux_flow::Level::Error,
            "dispatch error",
            &crew_owned,
            &case_id_owned,
            model_owned.as_deref(),
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

/// NON-SECRET liveness detail for `crew-resolved` — crew name, seat count,
/// and the distinct endpoint HOSTS of the remote seats. HOST ONLY: never
/// the full URL, never any credential.
fn crew_detail(crew: &ResolvedCrew) -> String {
    let seat_count: usize = crew.seats.values().map(|v| v.len()).sum();
    let mut hosts: Vec<String> = crew
        .seats
        .values()
        .flatten()
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
    format!("crew={} seats={seat_count} remote_hosts={hosts_str}", crew.name)
}

/// (#1284 Packet 2) Map a review dispatch's `Result<ReviewEnvelope>` onto
/// the generalized [`crew::envelope::MissionEnvelope`] contract:
///
/// - `Err` -> `Error` (a hard failure before any envelope was produced).
/// - `Ok(env)` with `env.degenerate.is_some()` -> `Degenerate` (the
///   operator-decision "no usable signal" gate fired).
/// - `Ok(env)` with `env.degenerate.is_none()` but `!env.warnings.is_empty()`
///   -> `Degraded` (real, postable output, but some sub-stage was
///   constrained).
/// - `Ok(env)` with `env.degenerate.is_none()` and `env.warnings.is_empty()`
///   -> `Clean`.
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
            let status = if env.degenerate.is_some() {
                MissionOutcomeStatus::Degenerate
            } else if !env.warnings.is_empty() {
                MissionOutcomeStatus::Degraded
            } else {
                MissionOutcomeStatus::Clean
            };
            let reason = env
                .degenerate
                .as_deref()
                .map(|r| format!("review degenerate: {r}"))
                .or_else(|| (status == MissionOutcomeStatus::Degraded).then(|| env.warnings.join("; ")));
            let mut envelope = MissionEnvelope::new(mission_id, status, phase_ids);
            envelope.reason = reason;
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
fn finalize_review_mission(mission_id: &str, phase_ids: &[&str], result: &Result<ReviewEnvelope>) {
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
/// trail up with `run_dispatch`'s own `config-resolved`/`crew-resolved`/
/// bundling markers and closes it with `synthesis`/`done` below.
pub(crate) fn launch(
    config: &MissionConfig,
    input_file: Option<&Path>,
    params: &[String],
    timeout_seconds: Option<u32>,
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

    let envelope_out = path_input(&collected, "envelope_out");
    let emit = path_input(&collected, "emit");
    let attribution = str_input(&collected, "attribution").map(str::to_string);

    let env: ReviewEnvelope = match path_input(&collected, "from_envelope") {
        Some(path) => {
            // Synthesis-only: dispatch-shaping inputs have nothing to
            // shape. Warn (don't error) — operator sovereignty: surface,
            // never silently ignore.
            let mut ignored: Vec<&str> = Vec::new();
            for key in ["crew", "worktree", "github", "head_sha", "bundler", "k"] {
                if collected.contains_key(key) {
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
        None => run_dispatch(config, &collected, &diff_file, &diff_text, timeout_seconds)?,
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

/// Everything but `from_envelope`: resolve the source + crew, build real
/// bundles, and dispatch either the review graph (`build_review_graph` +
/// `run_review_graph`) or `run_judge_only` (`charges_file` — re-judge a
/// saved flag list without re-running the probe).
fn run_dispatch(
    config: &MissionConfig,
    collected: &BTreeMap<String, Value>,
    diff_file: &Path,
    diff_text: &str,
    timeout_seconds: u32,
) -> Result<ReviewEnvelope> {
    let case = derive_case_id(collected);
    let crew_name = match str_input(collected, "crew").map(str::trim).filter(|s| !s.is_empty()) {
        Some(c) => c.to_string(),
        None => {
            let available = load_registry(str_input(collected, "profiles"))
                .map(|l| l.registry.crews.keys().map(String::as_str).collect::<Vec<_>>().join(", "))
                .unwrap_or_default();
            bail!(
                "mission launch review: input `crew` is required (unless `from_envelope`) — name \
                 a crew from your profiles.json's \"crews\" map via --param crew=<name>. \
                 Available: {}",
                if available.is_empty() { "(none)".to_string() } else { available }
            );
        }
    };

    let source = resolve_source(collected)?;
    liveness_detail("config-resolved", &case, &config_detail());
    let loaded = load_registry(str_input(collected, "profiles"))?;
    let mut crew = resolve_crew(&loaded.registry, &crew_name)?;
    if let Some(k) = u32_input(collected, "k")? {
        if let Some(staffings) = crew.seats.get_mut("review-probe") {
            for s in staffings.iter_mut() {
                s.k = k;
            }
        }
    }
    liveness_detail("crew-resolved", &case, &crew_detail(&crew));

    liveness_case("bundling-start", &case);
    let worktree = path_input(collected, "worktree");
    let bundle_set = match str_input(collected, "bundler") {
        Some(cmd) => external_bundles(cmd, worktree.as_deref(), diff_file)?,
        None => build_bundles(&source, diff_text)?,
    };
    let bundles = bundle_inputs_from_set(&bundle_set, &source)?;
    liveness_detail("bundling-done", &case, &format!("bundles={}", bundles.len()));
    let bundle_count = bundles.len();

    let intent = match path_input(collected, "intent_file") {
        Some(p) => std::fs::read_to_string(&p).with_context(|| format!("reading intent_file {}", p.display()))?,
        None => String::new(),
    };

    let mode_str = str_input(collected, "mode").unwrap_or("auto").to_string();
    let mode = parse_exec_mode(&mode_str)?;
    let dispatch_start_extra = json!({ "exec_mode": mode_str, "bundles": bundle_count });

    let probe_system = darkmux_crew::loader::role_prompt("review-probe").ok_or_else(|| {
        anyhow!(
            "darkmux: role \"review-probe\" has no system prompt — reinstall darkmux or check \
             <crew_root>/roles/review-probe.md"
        )
    })?;
    let judge_system = darkmux_crew::loader::role_prompt("review-judge").ok_or_else(|| {
        anyhow!(
            "darkmux: role \"review-judge\" has no system prompt — reinstall darkmux or check \
             <crew_root>/roles/review-judge.md"
        )
    })?;
    let verify_system = darkmux_crew::loader::role_prompt("review-verify").ok_or_else(|| {
        anyhow!(
            "darkmux: role \"review-verify\" has no system prompt — reinstall darkmux or check \
             <crew_root>/roles/review-verify.md"
        )
    })?;

    let case_id_for_bookends = case.clone();
    let crew_name_for_bookends = crew.name.clone();
    let model_for_bookends = crew_model_summary(&crew);
    let remote_max_tokens_per_execution = darkmux_types::config_access::remote_max_tokens_per_execution();

    let mut emitter = FleetFlowEmitter;

    liveness_case("flow-sinks-up", &case);
    liveness_detail(
        "dispatch-start",
        &case,
        &format!(
            "crew={crew_name_for_bookends} models={}",
            model_for_bookends.as_deref().unwrap_or("(none)")
        ),
    );

    if let Some(charges_path) = path_input(collected, "charges_file") {
        let inputs = ReviewInputs {
            case_id: case.clone(),
            crew: &crew,
            intent_title: "",
            intent_body: &intent,
            diff: diff_text,
            mode,
            probe_system: &probe_system,
            judge_system: &judge_system,
            verify_system: &verify_system,
            bundles: Some(bundles),
            remote_max_tokens_per_execution,
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
            dispatch_start_extra,
            move |emitter| run_judge_only(flags, &inputs, &mut chat, &mut cycler, emitter),
        )
    } else {
        let seats = validate_review_crew(&crew)?;
        let probes: Vec<_> = seats.probes.clone();
        let judge = seats.judge.clone();
        let verify = seats.verify.cloned();
        let judge_identifier = seat_identifier(&judge.pm);
        let request_changes = crew.request_changes;

        let ctx = Arc::new(ReviewStepContext {
            case_id: case_id_for_bookends.clone(),
            crew: crew.clone(),
            intent_title: String::new(),
            intent_body: intent,
            diff: diff_text.to_string(),
            probe_system,
            judge_system,
            verify_system,
            bundles,
            remote_max_tokens_per_execution,
            timeout_seconds,
            chat_override: None,
        });

        let mut id_input: BTreeMap<String, Value> = BTreeMap::new();
        id_input.insert("case_id".to_string(), Value::String(case_id_for_bookends.clone()));
        let mission_id = mission_launch::derive_mission_id("review", &id_input)?;

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

        let graph = build_review_graph(
            ctx.clone(),
            judge.clone(),
            verify.clone(),
            &probes,
            &investigate_phase_id,
            &adjudicate_phase_id,
            &report_phase_id,
            judge_concurrency,
        )?;

        // (#1417) Mint (or idempotently reuse/reopen) the Mission + the SAME
        // three phases review.json declares (investigate/adjudicate/report)
        // — the GENERIC instance-minting primitive every config-launched
        // mission uses, replacing the retired `build_mission_for_review`/
        // `reopen_terminal_mission_for_rerun` bespoke pair (their reopen
        // semantics are already implemented once, generically, here). Moved
        // to run AFTER `build_review_graph` succeeds (see the comment
        // above) — everything the graph needed (the phase id strings) was
        // already derived without touching the Mission, so nothing here
        // needs to run earlier.
        //
        // (must-fix 2, #1284 Packet 4b review gate) Case-bearing provenance,
        // matching the retired `build_mission_for_review`'s board-facing
        // strings: a fresh mint's description names the case + crew (N CI
        // reviews must be distinguishable on `mission status`/the viewer —
        // the config's own description is an 800-char transcription
        // paragraph, useless as a board row), and a reopen's reasoning
        // names the case being re-run.
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
        let reopen_reasoning = format!("review re-run for case `{case_id_for_bookends}`");
        let (_, _reused) = mission_launch::ensure_mission_and_phases_with_provenance(
            &mission_id,
            config,
            Some(&description),
            Some(&reopen_reasoning),
        )?;
        crew::lifecycle::save_config_snapshot(&mission_id, config)
            .context("persisting config-snapshot.json")?;

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
        let mission_id_for_status = mission_id.clone();
        let mission_id_for_steps = mission_id.clone();
        let mission_id_for_persist = mission_id.clone();
        let crew_name_for_closure = crew_name_for_bookends.clone();
        let report_phase_id_for_closure = report_phase_id.clone();
        let report_phase_id_for_persist = report_phase_id.clone();
        let mut started_phases: std::collections::HashSet<String> = std::collections::HashSet::new();

        let result = with_dispatch_bookends(
            &mut emitter,
            &case_id_for_bookends,
            &crew_name_for_bookends,
            model_for_bookends.as_deref(),
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
                        mission_launch::lazy_start_phase_for_step(
                            &mission_id_for_persist,
                            phase_id,
                            step.status,
                            &mut started_phases,
                        );
                        // (F2, gate remediation) Warn, never silently
                        // swallow — same dim-warning parity as
                        // `mission_launch.rs`/`mission_run.rs`'s persist
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
        );

        // The Mission/Phases minted above start life Active/Running and,
        // without this, never reach a terminal status regardless of
        // outcome — every review, clean or errored, would be left
        // permanently "stuck" in `darkmux mission status`.
        let phase_ids = [
            investigate_phase_id.as_str(),
            adjudicate_phase_id.as_str(),
            report_phase_id.as_str(),
        ];
        finalize_review_mission(&mission_id_for_status, &phase_ids, &result);

        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use darkmux_lab::lab::review::MemberRecord;
    use darkmux_profiles::crews::ResolvedSeatStaffing;
    use darkmux_types::ProfileModel;

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
            orchestrator: None,
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
        let result = with_dispatch_bookends(&mut emitter, "case-1", "test-crew", Some("darkmux:judge-model"), json!({}), |em| {
            em.emit(fake_review_record("review.task"));
            em.emit(fake_review_record("review.step"));
            em.emit(fake_review_record("review.ruling"));
            Ok(ReviewEnvelope::default())
        });
        assert!(result.is_ok());

        let actions: Vec<&str> = emitter.records.iter().map(|r| r.action.as_str()).collect();
        assert_eq!(actions, vec!["dispatch start", "review.task", "review.step", "review.ruling", "dispatch complete"]);

        let terminals = actions.iter().filter(|a| **a == "dispatch complete" || **a == "dispatch error").count();
        assert_eq!(terminals, 1, "exactly one terminal dispatch record: {actions:?}");

        let start = &emitter.records[0];
        assert_eq!(start.session_id.as_deref(), Some("case-1"));
        assert_eq!(start.handle, "test-crew");
        assert_eq!(start.model.as_deref(), Some("darkmux:judge-model"));
        assert_eq!(start.source.as_deref(), Some("review"));

        let terminal = emitter.records.last().unwrap();
        assert_eq!(terminal.action, "dispatch complete");
        assert_eq!(terminal.source.as_deref(), Some("review"));
        assert_eq!(terminal.payload.as_ref().unwrap()["result_class"], "ok");
    }

    #[test]
    fn with_dispatch_bookends_error_path_emits_dispatch_error_terminal_with_the_real_message() {
        let mut emitter = RecordingEmitter::default();
        let result = with_dispatch_bookends(&mut emitter, "case-2", "test-crew", None, json!({}), |em| {
            em.emit(fake_review_record("review.task"));
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
            with_dispatch_bookends(&mut emitter, "case-3", "test-crew", None, json!({}), |_em| {
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
        let result = with_dispatch_bookends(&mut emitter, "case-4", "test-crew", Some("darkmux:judge-model"), json!({}), |em| {
            em.emit(fake_review_record("review.task"));
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
        let result = with_dispatch_bookends(&mut emitter, "case-5", "test-crew", None, json!({}), |_em| {
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
            pm: ProfileModel { id: model_id.into(), ..Default::default() },
            k: 1,
            passes: 2,
            max_tokens: None,
            selector: None,
        }
    }

    #[test]
    fn crew_model_summary_dedupes_and_sorts_every_seat_model() {
        let mut seats = BTreeMap::new();
        seats.insert("review-probe".to_string(), vec![staffing("zzz-probe"), staffing("aaa-probe")]);
        seats.insert("review-judge".to_string(), vec![staffing("zzz-probe")]);
        let crew = ResolvedCrew { name: "test-crew".into(), seats, request_changes: false };

        let summary = crew_model_summary(&crew).expect("non-empty crew has a summary");
        assert_eq!(summary, "darkmux:aaa-probe, darkmux:zzz-probe", "sorted + deduped: {summary}");
    }

    #[test]
    fn crew_model_summary_none_for_a_crew_with_no_staffed_seats() {
        let crew = ResolvedCrew { name: "empty-crew".into(), seats: BTreeMap::new(), request_changes: false };
        assert_eq!(crew_model_summary(&crew), None);
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

    /// Mint (or, on a rerun, reopen) a review-config instance via the SAME
    /// provenance-bearing primitive `run_dispatch` uses, returning
    /// `(mission_id, [investigate, adjudicate, report])`.
    fn mint_review_instance(case_id: &str) -> (String, [String; 3]) {
        let config = crew::mission_config::load("review").expect("review is embedded").config;
        let mut id_input: BTreeMap<String, Value> = BTreeMap::new();
        id_input.insert("case_id".to_string(), Value::String(case_id.to_string()));
        let mission_id = mission_launch::derive_mission_id("review", &id_input).unwrap();
        // Same call shape as `run_dispatch` (must-fix 2 provenance).
        let description = format!("PR review — {case_id} (crew `test-crew`)");
        let reopen_reasoning = format!("review re-run for case `{case_id}`");
        let (real_phase_ids, _) = mission_launch::ensure_mission_and_phases_with_provenance(
            &mission_id,
            &config,
            Some(&description),
            Some(&reopen_reasoning),
        )
        .unwrap();
        // `ensure_mission_and_phases` mints phases Planned. `run_dispatch`'s
        // REAL flow no longer flips all three eagerly (#1400 — that was the
        // bug: every phase pulsed "running" from second zero); each starts
        // LAZILY instead, via `mission_launch::lazy_start_phase_for_step`
        // fired from the `persist` closure as the graph actually reaches
        // it (see `review_phases_start_lazily_not_all_at_mint`, right below
        // this fixture, for a direct test of that). This fixture still
        // starts every phase up front because the tests THAT USE IT
        // (`finalize_review_mission_*`) are exercising *finalization*
        // behavior, which needs "every phase already Running" as its
        // precondition, not the start-timing #1400 is about. `let _ =`
        // (not unwrap): on a REUSE of a previously-CLEAN instance a phase
        // is already terminal Complete and `phase_start` correctly
        // refuses — same silent skip the production lazy-start path
        // performs.
        for real_id in real_phase_ids.values() {
            let _ = crew::lifecycle::phase_start(real_id);
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
        let case_id = "owner/repo@lazy1400";
        let mut id_input: BTreeMap<String, Value> = BTreeMap::new();
        id_input.insert("case_id".to_string(), Value::String(case_id.to_string()));
        let mission_id = mission_launch::derive_mission_id("review", &id_input).unwrap();
        let (real_phase_ids, _reused) = mission_launch::ensure_mission_and_phases_with_provenance(
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
        assert_eq!(mission_status_str(&mission_id), "closed", "clean run closes the mission");
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
        assert_eq!(mission_status_str(&mission_id), "closed");
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
        assert_eq!(mission_status_str(&mission_id), "closed");
    }

    /// (C5, #1284 Packet 4b review gate) The review-shaped RERUN path,
    /// covering what the retired `reopen_terminal_mission_for_rerun` tests
    /// covered: a re-run of the SAME case must REOPEN the prior terminal
    /// instance — never error, never double-mint — with case-bearing
    /// provenance on the reopened record (must-fix 2).
    #[test]
    #[serial_test::serial]
    fn review_rerun_of_the_same_case_reopens_the_terminal_instance() {
        let _guard = CrewDirGuard::new();
        let case = "owner/repo@rerun4b";

        let (mission_id, phase_ids) = mint_review_instance(case);
        let clean: Result<ReviewEnvelope> = Ok(ReviewEnvelope { degenerate: None, ..Default::default() });
        let ids: [&str; 3] = [&phase_ids[0], &phase_ids[1], &phase_ids[2]];
        finalize_review_mission(&mission_id, &ids, &clean);
        assert_eq!(mission_status_str(&mission_id), "closed");

        // Re-run the SAME case through the same mint path.
        let (mission_id2, _) = mint_review_instance(case);
        assert_eq!(mission_id2, mission_id, "same case -> same instance id, reused not double-minted");
        assert_eq!(mission_status_str(&mission_id), "active", "the terminal instance was reopened");

        // Case-bearing provenance (must-fix 2): the board row names the
        // case + crew, never the config's transcription paragraph.
        let text = std::fs::read_to_string(crew::lifecycle::mission_path(&mission_id)).unwrap();
        let v: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(
            v["description"].as_str().unwrap(),
            format!("PR review — {case} (crew `test-crew`)"),
            "fresh-mint description must be case-bearing"
        );
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

        // A minimal profiles.json with a crew that satisfies
        // `validate_review_crew` (>= 1 review-probe staffing, exactly 1
        // review-judge staffing) — enough to get `run_dispatch` past crew
        // resolution and bundle-building (an empty worktree + empty diff
        // yields an empty, valid bundle set) and up to the phase-id
        // validation this test targets. No model dispatch is ever reached.
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
        let result = run_dispatch(&config, &collected, &diff_file, "", 60);

        let err = result.expect_err("a renamed phase id must fail, not silently interpret");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("does not declare a phase with id `investigate`"),
            "expected the phase-id validation error, got: {msg}"
        );

        // The regression: the Mission must NEVER have been minted. Before
        // the #1417 fix, `ensure_mission_and_phases_with_provenance` ran
        // BEFORE this validation and would have already written
        // mission.json (Active, 3 Planned phases) to disk.
        let mut id_input: BTreeMap<String, Value> = BTreeMap::new();
        id_input.insert("case_id".to_string(), Value::String(case_id.to_string()));
        let mission_id = mission_launch::derive_mission_id("review", &id_input).unwrap();
        assert!(
            !crew::lifecycle::mission_path(&mission_id).exists(),
            "mission.json must not exist — a failed pre-graph validation must never strand a minted mission"
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

        let build_needle = concat!("build_review", "_graph(");
        let run_needle = concat!("run_review", "_graph(");
        for (label, src) in [("mission_launch_review.rs", THIS_SRC), ("review_bench.rs", REVIEW_BENCH_SRC)] {
            assert!(src.contains(build_needle), "{label} must dispatch through review::build_review_graph");
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
