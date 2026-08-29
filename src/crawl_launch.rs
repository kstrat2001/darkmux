//! `darkmux mission launch crawl` — the crawl LAUNCHER (#1959).
//!
//! `crates/darkmux-lab/src/crawl/plan.rs` is the mechanical, free-to-compute
//! half: it turns a MATERIALIZED workspace (`darkmux_crew::workspace_spec`
//! — a generic mission input, not crawl-specific) + a set of rules
//! (`darkmux_crew::rules`, likewise generic) into a deterministic
//! [`plan::Plan`] of work [`plan::Unit`]s with token estimates. NO model
//! dispatch happens there. This module is the other half — it resolves
//! the `workspace`/`rules` inputs (or synthesizes a one-shot
//! `source`+`rule` spec), plans, then walks that plan SEQUENTIALLY,
//! dispatching each unit to the `crawler` role with the materialized tree
//! mounted read-only, and records what came back.
//!
//! **Tier 3, bespoke, lives here rather than in `darkmux-crew`
//! (`CLAUDE.md`'s StepKind tiering).** A crawl's Task/Step graph is not
//! declared in a JSON mission config ahead of time — the units, and
//! therefore the Task/Step shape, are computed at RUN TIME from the
//! resolved plan. That is genuinely new control flow (a sequential loop
//! over a runtime-computed unit list, with kill-file / SIGINT / per-unit
//! error handling none of the generic `Step`/`StepKind` machinery has a
//! seam for), not a config the generic `mission_config::interpret` +
//! `crew::scheduler::run_step_graph` path could execute — so this module
//! mints its own Mission/Phase/Task/Step records directly via
//! `crew::lifecycle`, the same primitives `ensure_mission_and_phases_with_
//! provenance` (`src/mission_launch.rs`) itself calls, and drives the
//! actual work with a plain sequential loop, mirroring `mission_launch_
//! review.rs`'s reasoning for taking the same dedicated-launcher route.
//!
//! **Routed by literal config id** (`config_id == "crawl"`), checked in
//! `mission_launch::launch` BEFORE `mission_config::load` — there is no
//! `templates/builtin/mission-configs/crawl.json` to load structurally
//! against (unlike `review`/`coder-phase`, which route on which STEP KINDS
//! a document declares). A crawl launch has no document at all; the
//! literal id is the only thing to route on, and there is exactly one
//! crawl entry point today.
//!
//! **Dispatch liveness (`CLAUDE.md` cross-system contract 2).** This
//! module never dispatches model work directly — every unit's model work
//! goes through `crew::dispatch::dispatch`, which already emits the
//! `dispatch start` / `dispatch complete` / `dispatch error` bookends on
//! every exit path. This launcher's OWN records (`mission start`/
//! `mission close`, `step start`/`step complete`/`step error` — the
//! generic lifecycle vocabulary every mission uses, carrying this
//! launcher's own numbers in `payload`; see the "flow records" section
//! below) are descriptive scaffolding around those bookends, not a
//! replacement for them. (#1959, revised: a bespoke `crawl.*` action
//! family — `crawl.mission.started/completed`, `crawl.unit.started/
//! completed`, `crawl.finding` — lived here through an earlier packet
//! and is now retired; see the flow schema's own changelog entry.)
//!
//! **Testing (no real model, no container).** The per-unit loop takes an
//! injectable `dispatch_fn: &mut dyn FnMut(DispatchOpts) -> Result<
//! DispatchResult>` — production passes `crew::dispatch::dispatch`
//! directly; tests inject a closure that emits the SAME `dispatch start`/
//! `dispatch complete` flow records `crew::dispatch::dispatch` would (via
//! `darkmux_crew::dispatch::build_dispatch_record_with_payload`, the same
//! builder that path already uses) and returns a scripted `DispatchResult`
//! pointing at a tempdir the test pre-seeded with `.darkmux-runtime/
//! findings.jsonl`. This proves the launcher's own orchestration (unit
//! selection, message building, path rewriting, the ledger, kill-file /
//! interrupt / error handling, the envelope) without spawning Docker or
//! touching LMStudio — live verification against a real dispatch is the
//! orchestrator's job after this ships (per this task's own instruction).
//!
//! **The kill file is honored BETWEEN units, not mid-unit.** The loop
//! checks `STOP` before dispatching each unit; a unit already dispatched
//! runs to its own completion or its own `--timeout`, never torn down
//! partway through — there is no mechanism here that could interrupt a
//! live container mid-dispatch, and this launcher doesn't try to build
//! one. Same shape for SIGINT.

use crate::crew;
use crate::mission_launch;
use anyhow::{anyhow, bail, Context, Result};
use crew::dispatch::{CompactionDispatchArgs, DispatchOpts, DispatchResult};
use crew::types::{
    Mission, MissionSpec, MissionSpecOrigin, MissionStatus, NodeStatus, Phase, PhaseStatus, Step, Task,
};
use darkmux_lab::crawl::plan::{self, Plan, ReadFileEntry, Site, Unit};
use darkmux_crew::rules::{self, Rule};
use darkmux_crew::workspace_spec::{self, MaterializeOptions, SourceSpec, WorkspaceSpec};
use darkmux_types::style;
use serde_json::{json, Value};
use std::cell::RefCell;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// Every `report_finding` field the runtime's `execute_report_finding`
/// persists (`runtime/src/tools/mod.rs`) — used to strip the container
/// path off `file` without disturbing any other field, whatever the
/// runtime's own record shape carries today or adds later (a finding's
/// fields are copied through VERBATIM per this launcher's own spec; this
/// list exists only to name the one field that gets rewritten).
const FINDING_FILE_KEY: &str = "file";

/// Production entry point — called from `mission_launch::launch` when
/// `config_id == "crawl"`. `--dry-run` (#1959, `darkmux mission launch
/// crawl --dry-run`) reaches this the SAME way every other input does:
/// `mission_launch::launch`'s CLI dispatch injects a synthetic `--param
/// dry_run=true` onto `params` before routing here — no separate `bool`
/// parameter on this function. `run` reads it via `bool_param` exactly
/// like `no_fetch`/`plan_out`, so every existing test calling `run`
/// directly (with no `"dry_run"` key at all) keeps its implicit "not a
/// dry run" behavior unchanged.
pub fn launch(input_file: Option<&Path>, params: &[String], timeout_seconds: Option<u32>) -> Result<i32> {
    let collected = mission_launch::collect_inputs(input_file, params)?;
    run(&collected, timeout_seconds, &mut |opts| crew::dispatch::dispatch(opts))
}

// ── param accessors ─────────────────────────────────────────────────────

fn str_param<'a>(collected: &'a BTreeMap<String, Value>, key: &str) -> Option<&'a str> {
    collected.get(key).and_then(Value::as_str)
}

fn bool_param(collected: &BTreeMap<String, Value>, key: &str) -> bool {
    match collected.get(key) {
        Some(Value::Bool(b)) => *b,
        Some(Value::String(s)) => matches!(s.trim(), "true" | "1" | "yes" | "on"),
        _ => false,
    }
}

fn usize_param(collected: &BTreeMap<String, Value>, key: &str) -> Result<Option<usize>> {
    match collected.get(key) {
        None => Ok(None),
        Some(Value::Number(n)) => n
            .as_u64()
            .map(|v| Some(v as usize))
            .ok_or_else(|| anyhow!("--param {key} must be a non-negative integer")),
        Some(Value::String(s)) => s
            .trim()
            .parse::<usize>()
            .map(Some)
            .with_context(|| format!("--param {key} must be a non-negative integer")),
        Some(other) => bail!("--param {key} must be a non-negative integer, got {other}"),
    }
}

fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

// ── unit accessors (Unit has no shared trait — small helpers instead) ───

fn unit_source(u: &Unit) -> &str {
    match u {
        Unit::Site { source, .. } | Unit::Read { source, .. } | Unit::Edge { source, .. } => source,
    }
}

fn unit_rules(u: &Unit) -> Vec<String> {
    match u {
        Unit::Site { rule, .. } | Unit::Edge { rule, .. } => vec![rule.clone()],
        Unit::Read { rules, .. } => rules.clone(),
    }
}

fn unit_kind(u: &Unit) -> &'static str {
    match u {
        Unit::Site { .. } => "site",
        Unit::Read { .. } => "read",
        Unit::Edge { .. } => "edge",
    }
}

/// Grouping key for the (source, rule) Task partition — a Read unit's
/// multiple rules are joined sorted+deduped so two Read units bound to the
/// exact same ruleset land in the same Task, matching `plan::plan`'s own
/// "combined read pass" grouping.
fn group_key(u: &Unit) -> (String, String) {
    let mut rule_ids = unit_rules(u);
    rule_ids.sort();
    rule_ids.dedup();
    (unit_source(u).to_string(), rule_ids.join("+"))
}

/// `Some(<warning text>)` when `loaded`'s major version component differs
/// from `expected`'s — a pure function (no I/O, no `eprintln!`) so the
/// decision is directly unit-testable; the call site is the only thing
/// that prints. `None` for any non-major difference (a minor/patch bump is
/// additive, per this project's own semver discipline) or a malformed
/// version string on the `expected` side (defensive; `expected` is always
/// `plan::PLAN_SCHEMA_VERSION` in production).
fn plan_schema_major_mismatch_warning(loaded: &str, expected: &str) -> Option<String> {
    let loaded_major = loaded.split('.').next().unwrap_or(loaded);
    let expected_major = expected.split('.').next().unwrap_or(expected);
    if loaded_major == expected_major {
        return None;
    }
    Some(format!(
        "schema_version {loaded} (this darkmux understands {expected}) — a MAJOR version \
         difference may mean fields this launcher relies on are missing or shaped differently"
    ))
}

// ── one-shot spec synthesis (#1959) ──────────────────────────────────────

/// A stable-ish workspace name derived from a one-shot `--param source=`
/// value, so two different one-shot sources don't collide on the same
/// `<darkmux root>/workspaces/<name>` materialization root. Strips a
/// trailing `.git`, keeps only `[A-Za-z0-9._-]`, and falls back to the
/// bare `"one-shot"` name when the source's basename sanitizes to
/// nothing (e.g. a bare `.` or an all-symbol string).
fn one_shot_workspace_name(source: &str) -> String {
    let base = source.trim_end_matches('/').rsplit(['/', '\\']).next().unwrap_or(source);
    let base = base.strip_suffix(".git").unwrap_or(base);
    let sanitized: String = base
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.') { c } else { '-' })
        .collect();
    let sanitized = sanitized.trim_matches('-');
    if sanitized.is_empty() {
        "one-shot".to_string()
    } else {
        format!("one-shot-{sanitized}")
    }
}

/// Synthesize a one-source `WorkspaceSpec` in memory from `--param
/// source=<path|url> --param rule=<id>` — the "just crawl this one thing
/// with this one rule" shortcut that needs no spec file on disk. `source`
/// is treated as a local `path` when it exists on disk as of this call,
/// else as a `git` origin (a URL, or a not-yet-cloned path — the same
/// either/or every `SourceSpec` already declares). Still runs through
/// `WorkspaceSpec::validate()` — loud validation at the one place every
/// spec (file-loaded or synthesized) passes through, never skipped for
/// the in-memory path.
fn synthesize_one_shot_spec(source: &str, rule_id: &str) -> Result<WorkspaceSpec> {
    let is_local_path = Path::new(source).exists();
    let source_spec = SourceSpec {
        id: "source".to_string(),
        git: if is_local_path { None } else { Some(source.to_string()) },
        path: if is_local_path { Some(source.to_string()) } else { None },
        git_ref: None,
        extras: Default::default(),
    };
    let spec = WorkspaceSpec {
        schema_version: None,
        name: Some(one_shot_workspace_name(source)),
        root: None,
        sources: vec![source_spec],
        include: None,
        exclude: None,
        edges: Vec::new(),
        rules: vec![rule_id.to_string()],
        extras: Default::default(),
    };
    spec.validate().context("validating the synthesized one-shot workspace spec")?;
    Ok(spec)
}

// ── unit selection ──────────────────────────────────────────────────────

/// Select units from the plan: an explicit `--param units=<csv>` filters by
/// id (in PLAN order, bailing loudly on any id the plan doesn't have),
/// else every unit; then an optional `--param limit=<n>` truncates. Returns
/// the selected units plus whether `limit` actually cut anything (the
/// `stopped_by: "limit"` signal — a `limit` >= the selection's length
/// changes nothing and reads as `"done"`).
fn select_units<'a>(the_plan: &'a Plan, units_filter: Option<&str>, limit: Option<usize>) -> Result<(Vec<&'a Unit>, bool)> {
    let mut selected: Vec<&Unit> = if let Some(csv) = units_filter {
        let ids: Vec<&str> = csv.split(',').map(str::trim).filter(|s| !s.is_empty()).collect();
        if ids.is_empty() {
            // (#1959 merge-gate finding 7) `--param units=` naming no ids
            // after parsing (e.g. an empty string, or all-commas) used to
            // fall straight through to the `.filter(...)` below and select
            // NOTHING — silently. A crawl that dispatches zero units and
            // reports success is a worse failure mode than a loud bail.
            bail!(
                "darkmux mission launch crawl: --param units=`{csv}` named no unit ids after \
                 parsing — pass a comma-separated list of unit ids (e.g. \
                 `--param units=u-0001,u-0002`), or drop --param units to select every unit in \
                 the plan"
            );
        }
        for id in &ids {
            if !the_plan.units.iter().any(|u| u.id() == *id) {
                let known: Vec<&str> = the_plan.units.iter().map(|u| u.id()).collect();
                bail!(
                    "darkmux mission launch crawl: --param units names unknown unit id `{id}` — \
                     the plan has {} unit(s): {}",
                    the_plan.units.len(),
                    known.join(", ")
                );
            }
        }
        the_plan.units.iter().filter(|u| ids.contains(&u.id())).collect()
    } else {
        the_plan.units.iter().collect()
    };
    let mut truncated = false;
    if let Some(n) = limit {
        if selected.len() > n {
            truncated = true;
        }
        selected.truncate(n);
    }
    Ok((selected, truncated))
}

// ── message building (model-facing — AI-convention terms, no darkmux
//    vocabulary; CLAUDE.md's "Model-facing prompt construction") ─────────

/// One rule's prose, verbatim from its title/match/no_match/evidence/
/// why_hint fields, wrapped so a Read unit binding several rules can carry
/// more than one without ambiguity about which sentence belongs to which
/// pattern.
fn pattern_block(rule: &Rule) -> String {
    format!(
        "<pattern name=\"{id}\">\nTitle: {title}\n\nReport a match when: {matches}\n\nDo NOT report when: {no_match}\n\nWhat evidence to cite: {evidence}\n\nHow to explain why: {why_hint}\n</pattern>\n\n",
        id = rule.id,
        title = rule.title.as_deref().unwrap_or(&rule.id),
        matches = rule.matches.as_deref().unwrap_or(""),
        no_match = rule.no_match.as_deref().unwrap_or(""),
        evidence = rule.evidence.as_deref().unwrap_or(""),
        why_hint = rule.why_hint.as_deref().unwrap_or(""),
    )
}

fn render_sites(source: &str, sites: &[Site]) -> String {
    // Full container paths: the workspace root is the MATERIALIZED tree, so a path
    // relative to the source (`ui/src/x.ts`) does not resolve and the tool
    // boundary rejects it (observed on the first live mission, #1959).
    sites
        .iter()
        .map(|s| format!("- /workspace/{source}/{}:{} (read lines {}-{})", s.file, s.line, s.start, s.end))
        .collect::<Vec<_>>()
        .join("\n")
}

fn render_files(source: &str, files: &[ReadFileEntry]) -> String {
    files
        .iter()
        .map(|f| match f {
            ReadFileEntry::Whole(path) => format!("- /workspace/{source}/{path}"),
            ReadFileEntry::Range { file, start, end } => format!("- /workspace/{source}/{file} (lines {start}-{end})"),
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// This block ends every dispatch message regardless of unit kind —
/// deliberately reusing the wording from the first crawler workload
/// (`templates/builtin/workloads/crawl-error-discard.json`) for the two
/// load-bearing sentences (the tool's exact five keys; the coverage
/// request), so a model already tuned against that workload sees familiar
/// phrasing here.
const REPORT_FINDING_INSTRUCTIONS: &str = "\nFor each match, call `report_finding` with these five keys exactly: `file`, `line`, `pattern`, `evidence`, `why`. `file` must be the full path exactly as listed above, starting with `/workspace/`. `evidence` must be the source line copied verbatim, and `line` must be where it appears.\n\nWhen you are done, say which files or sites you examined, which you did not get to, and whether you covered the whole scope.\n";

/// Build the dispatch message for one unit. Model-facing (AI-convention
/// terms; the words `unit`/`ledger`/`corpus`/`packet` never appear —
/// darkmux-internal vocabulary a clean-context model can't ground).
fn build_message(rules_by_id: &BTreeMap<String, Rule>, unit: &Unit) -> Result<String> {
    let mut out = String::new();
    match unit {
        Unit::Site { rule, sites, source, .. } => {
            let r = rules_by_id.get(rule).ok_or_else(|| {
                anyhow!(
                    "crawl launcher: no rule resolved for id `{rule}` — re-run `darkmux crawl plan` \
                     for this manifest, or drop `--param plan=`"
                )
            })?;
            out.push_str(&pattern_block(r));
            out.push_str(&format!(
                "Your scope is these sites in `/workspace/{source}`. For each, read lines noted below and decide whether the cited line matches the pattern. Sites:\n{}\n",
                render_sites(source, sites)
            ));
        }
        Unit::Read { rules: rule_ids, files, source, .. } => {
            for rid in rule_ids {
                let r = rules_by_id.get(rid).ok_or_else(|| {
                    anyhow!(
                        "crawl launcher: no rule resolved for id `{rid}` — re-run `darkmux crawl plan` \
                         for this manifest, or drop `--param plan=`"
                    )
                })?;
                out.push_str(&pattern_block(r));
            }
            out.push_str(&format!(
                "Your scope is these files in `/workspace/{source}`. Read each one in full and apply every pattern above:\n{}\n",
                render_files(source, files)
            ));
        }
        Unit::Edge {
            rule,
            sites,
            source,
            library,
            package,
            pinned,
            library_version,
            library_surface,
            ..
        } => {
            let r = rules_by_id.get(rule).ok_or_else(|| {
                anyhow!(
                    "crawl launcher: no rule resolved for id `{rule}` — re-run `darkmux crawl plan` \
                     for this manifest, or drop `--param plan=`"
                )
            })?;
            out.push_str(&pattern_block(r));
            out.push_str(&format!(
                "Your scope is these import sites in `/workspace/{source}`:\n{}\n\n",
                render_sites(source, sites)
            ));
            out.push_str(&format!(
                "The library `{package}` at the version being examined is at `/workspace/{library}`; its entry files and changelog are: {}. The consumer pins `{pinned}`; the library version is `{library_version}`.\n",
                if library_surface.is_empty() { "(none)".to_string() } else { library_surface.join(", ") }
            ));
        }
    }
    out.push_str(REPORT_FINDING_INSTRUCTIONS);
    Ok(out)
}

/// Whether `stderr` carries the host watchdog's structured inactivity-
/// timeout marker (`darkmux_crew::dispatch_internal::INACTIVITY_TIMEOUT_
/// MARKER`, #363) — the one reliable `DispatchResult`-level signal that a
/// non-clean exit was specifically the watchdog hard-killing the
/// container, rather than any other failure shape (#1959 merge-gate
/// finding 9).
fn watchdog_timeout_fired(stderr: &str) -> bool {
    stderr.contains(crew::dispatch_internal::INACTIVITY_TIMEOUT_MARKER)
}

/// Pull `(result, wall_ms, prompt_tokens, completion_tokens, model,
/// detections, rest_ms)` out of a dispatch's `--json` envelope
/// (`res.stdout`). `result` is `"stop"` on a clean finish, `"timeout"`
/// when `stderr` carries the host watchdog's marker (see
/// [`watchdog_timeout_fired`]), else `"error"` (a hard-to-parse envelope,
/// `max_turns`, an escalation variant, or a bare non-zero exit with
/// neither signal). When `stdout` is non-empty but doesn't parse as the
/// expected JSON envelope, this prints a warning naming the unit and the
/// first 120 chars — silent swallowing here previously meant a model that
/// broke the `--json` contract read as an ordinary clean "stop" whenever
/// `exit_code == 0`. `rest_ms` (#1959) is the global inter-turn rest this
/// dispatch actually took (`metrics.rest_ms`, #2094) — surfaced beside
/// `wall_ms` on the `step complete`/`step error` payload so a rested
/// unit's wall clock is never misread as a slow model.
fn interpret_dispatch_result(unit_id: &str, res: &DispatchResult) -> (String, u64, u64, u64, Option<String>, Option<Value>, u64) {
    let envelope: Option<Value> = if res.stdout.trim().starts_with('{') {
        serde_json::from_str(&res.stdout).ok()
    } else {
        None
    };
    if envelope.is_none() && !res.stdout.trim().is_empty() {
        let excerpt: String = res.stdout.chars().take(120).collect();
        eprintln!(
            "{}",
            style::warn(&format!(
                "darkmux mission launch crawl: unit `{unit_id}` produced non-JSON stdout \
                 (expected a `--json` envelope) — first 120 chars: {excerpt:?}"
            ))
        );
    }
    let timed_out = watchdog_timeout_fired(&res.stderr);
    let result_label = match envelope.as_ref().and_then(|e| e.get("result")).and_then(Value::as_str) {
        Some("stop") => "stop".to_string(),
        Some(_) if timed_out => "timeout".to_string(),
        Some(_) => "error".to_string(),
        None if timed_out => "timeout".to_string(),
        None => if res.exit_code == 0 { "stop".to_string() } else { "error".to_string() },
    };
    let model = envelope.as_ref().and_then(|e| e.pointer("/metrics/model")).and_then(Value::as_str).map(String::from);
    let wall_ms = envelope.as_ref().and_then(|e| e.pointer("/metrics/wall_ms")).and_then(Value::as_u64).unwrap_or(0);
    let prompt_tok = envelope.as_ref().and_then(|e| e.pointer("/metrics/prompt_tokens")).and_then(Value::as_u64).unwrap_or(0);
    let completion_tok =
        envelope.as_ref().and_then(|e| e.pointer("/metrics/completion_tokens")).and_then(Value::as_u64).unwrap_or(0);
    let detections = envelope.as_ref().and_then(|e| e.get("detections")).cloned();
    let rest_ms = envelope.as_ref().and_then(|e| e.pointer("/metrics/rest_ms")).and_then(Value::as_u64).unwrap_or(0);
    (result_label, wall_ms, prompt_tok, completion_tok, model, detections, rest_ms)
}

// ── flow records (#1959, revised — no bespoke `crawl.*` vocabulary) ──────
//
// The crawl launcher mints a real Mission/Phase/Task/Step (it always
// has), so it uses the SAME generic lifecycle actions every other
// mission uses — `mission start`/`mission close` (via
// `crew::lifecycle::mission_start_with_reasoning_and_payload`/
// `mission_terminal_with_reasoning_and_payload`) and `step start`/`step
// complete`/`step error` (via this module's own `unit_step_record`,
// wrapping `darkmux_crew::scheduler::step_lifecycle_record_with_payload`)
// — with the crawl-specific numbers riding in `payload`. There is no
// `crawl.finding` and no replacement for it: the runtime classifies a
// REJECTED/NOT-RECORDED `report_finding` reply as a FAILED tool call, so
// `payload.ok` on the ordinary `dispatch.tool` record already tells an
// external tracker whether a finding was accepted — see
// `crew::dispatch::DispatchOpts::record_context`, set per unit on the
// dispatch below, which merges this unit's `workspace`/`source`/`sha`/
// `rule`/`unit` under `payload.context` on every record that dispatch's
// flow-record surface emits.

/// `step_lifecycle_record_with_payload` stamps `session_id` from
/// `session_id::task(&step.task_id)` — the convention every
/// `run_step_graph`-driven mission uses. This launcher drives its own
/// sequential loop instead and has its OWN established per-unit session
/// id (`crawl-{mission_id}-{unit_id}`, shared with that unit's dispatch
/// and findings) — this wrapper builds the record via the shared
/// builder (so the canonical action/shape is never hand-duplicated) then
/// overrides `session_id`/`mission_id` to this launcher's own
/// convention, keeping a unit's `step start`/`step complete` correlated
/// with its dispatch by session id, same as before this packet.
fn unit_step_record(
    step: &crew::types::Step,
    action: &str,
    mission_id: &str,
    session_id: &str,
    payload: Value,
    model: Option<&str>,
) -> darkmux_flow::FlowRecord {
    let mut rec = darkmux_crew::scheduler::step_lifecycle_record_with_payload(step, action, Some(payload));
    rec.mission_id = Some(mission_id.to_string());
    rec.session_id = Some(session_id.to_string());
    rec.model = model.map(String::from);
    rec
}

/// Strip a container-path prefix off a finding's `file` field — either the
/// absolute form (`/workspace/<source-id>/<rel>`, the literal contract
/// this launcher's spec names) or the bare relative form
/// (`<source-id>/<rel>`, since a model may have copied the exact scope
/// listing this launcher itself handed it). Falls through unchanged when
/// neither prefix matches (a model that ignored the given source id
/// entirely — rare, but the raw value survives in `file_raw` either way).
/// (#1959) Pick the ONE rule id a finding belongs to. A single-rule unit needs
/// no disambiguation; a multi-rule (read) unit uses the `pattern` the model
/// reported, matched case-insensitively against the unit's rule ids. When the
/// model's pattern names none of them, the first rule stands in and the
/// pattern is returned so the record can say so (`rule_unmatched_pattern`).
pub(crate) fn finding_rule_for(pattern: Option<&str>, rule_ids: &[String]) -> (String, Option<String>) {
    if let [only] = rule_ids {
        return (only.clone(), None);
    }
    let wanted = pattern.map(str::trim).unwrap_or("");
    if let Some(hit) = rule_ids.iter().find(|r| r.eq_ignore_ascii_case(wanted)) {
        return (hit.clone(), None);
    }
    (
        rule_ids.first().cloned().unwrap_or_default(),
        if wanted.is_empty() { None } else { Some(wanted.to_string()) },
    )
}

fn strip_source_prefix(source_id: &str, raw: &str) -> String {
    let abs_prefix = format!("/workspace/{source_id}/");
    if let Some(rel) = raw.strip_prefix(&abs_prefix) {
        return rel.to_string();
    }
    let rel_prefix = format!("{source_id}/");
    if let Some(rel) = raw.strip_prefix(&rel_prefix) {
        return rel.to_string();
    }
    raw.to_string()
}

fn append_file(path: &Path, text: &str) -> Result<()> {
    use std::io::Write;
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .with_context(|| format!("opening {}", path.display()))?;
    f.write_all(text.as_bytes())
        .with_context(|| format!("appending to {}", path.display()))?;
    Ok(())
}

/// (#1959) Count `report_finding` tool calls THIS unit's dispatch made
/// that the runtime rejected (`tool.completed` events with
/// `tool_name == "report_finding"` and `ok == false` — see
/// `runtime::failure_rate::classify_outcome`'s REJECTED/NOT-RECORDED
/// classification). Reads `out_dir/.darkmux-runtime/trajectory.jsonl`,
/// the same file the host tailer streams live; a missing/unreadable file
/// or a line that doesn't parse is silently skipped (this is a
/// best-effort "exclusions" count for the operator-facing payload/table,
/// never a correctness-bearing value — the ledger and the accepted-
/// findings count are unaffected by anything this function returns).
fn count_rejected_report_findings(out_dir: &Path) -> usize {
    let traj_path = out_dir.join(".darkmux-runtime").join("trajectory.jsonl");
    let Ok(body) = std::fs::read_to_string(&traj_path) else {
        return 0;
    };
    body.lines()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .filter(|v| {
            v.get("type").and_then(Value::as_str) == Some("tool.completed")
                && v.get("tool_name").and_then(Value::as_str) == Some("report_finding")
                && v.get("ok").and_then(Value::as_bool) == Some(false)
        })
        .count()
}

/// One (source, rule-group) Task, and the ordered Step ids of the units
/// assigned to it.
struct TaskGroup {
    id: String,
    source: String,
    rules_label: String,
    unit_indices: Vec<usize>,
}

// ── finalize guard (#1959 merge-gate finding 1b) ────────────────────────

/// Mutable accumulators for one crawl run, shared between the per-unit
/// loop and [`CrawlFinalizeGuard`] via a `RefCell` — the guard needs to
/// read them from a `Drop` context that can't hold a scoped `&mut` across
/// the whole loop. Everything here is single-threaded, single-function
/// state; a `RefCell` is the plain tool for that, not a concurrency
/// primitive doing double duty.
struct CrawlStats {
    stopped_by: &'static str,
    units_completed: usize,
    units_errored: usize,
    units_skipped: usize,
    findings_total: usize,
    prompt_tokens_total: u64,
    completion_tokens_total: u64,
    wall_ms_total: u64,
    per_unit_rows: Vec<Value>,
    /// The first non-`None` model any unit's envelope reported, in unit
    /// order — the envelope header's `model` field (finding 8).
    first_model: Option<String>,
}

/// Everything [`finalize_crawl`] needs that doesn't change once the loop
/// starts — split out from [`CrawlStats`] so the guard can hold it by
/// value alongside a `&RefCell<CrawlStats>` without the two fighting the
/// borrow checker.
struct FinalizeCtx {
    mission_id: String,
    phase_id: String,
    workspace_name: String,
    units_in_plan: usize,
    units_selected: usize,
    runs_dir: PathBuf,
    ledger_path: PathBuf,
    timeout_secs: u32,
    limit: Option<usize>,
    plan_path: Option<PathBuf>,
    units_filter: Option<String>,
}

/// RAII guard mirroring `darkmux-crew`'s `DispatchBookendGuard`: armed for
/// the duration of the per-unit loop, so an early `?`-return (today, only
/// `build_message`'s rule lookup — see finding 1a for why that's
/// unreachable in production, and this module's tests for a panic-
/// injection proof of the same safety net) or a panic still leaves a
/// matching `mission close` record, a written envelope, and a
/// non-`Active` mission behind — never a mission stuck `Active` with the
/// counts accumulated so far silently lost. `close()` (the normal
/// end-of-loop path) disarms the guard so `Drop` never double-finalizes.
struct CrawlFinalizeGuard<'a> {
    armed: bool,
    stats: &'a RefCell<CrawlStats>,
    ctx: FinalizeCtx,
}

impl<'a> CrawlFinalizeGuard<'a> {
    fn new(stats: &'a RefCell<CrawlStats>, ctx: FinalizeCtx) -> Self {
        Self { armed: true, stats, ctx }
    }

    /// The normal end-of-loop path.
    fn close(&mut self) -> Result<i32> {
        self.armed = false;
        finalize_crawl(self.stats, &self.ctx)
    }
}

impl Drop for CrawlFinalizeGuard<'_> {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        self.stats.borrow_mut().stopped_by = "error";
        // Best-effort, same as every other lifecycle call in this module: a
        // `Drop` can't propagate a `Result`, and this guard IS the last-
        // resort finalize — there's nowhere further to report a failure
        // to. (CONSIDER 4) Not a silent `let _ = ...` though: the table
        // above already reached the operator regardless, but a swallowed
        // envelope-write failure here would otherwise leave NO trace at
        // all that the write itself never landed — name the path so it's
        // at least visible on stderr.
        if let Err(e) = finalize_crawl(self.stats, &self.ctx) {
            eprintln!(
                "{}",
                style::warn(&format!(
                    "darkmux mission launch crawl: finalize on the abort path failed writing {} — {e:#}",
                    self.ctx.runs_dir.join("envelope.json").display()
                ))
            );
        }
    }
}

/// Shared finalize logic for both the normal end-of-loop path
/// ([`CrawlFinalizeGuard::close`]) and the abort path
/// ([`CrawlFinalizeGuard`]'s `Drop`): transitions the phase to its correct
/// terminal (finding 3), closes the mission (the generic `mission close`
/// record, carrying this run's own numbers in its payload — see this
/// module's "flow records" section), prints the summary table, and
/// writes the envelope.
fn finalize_crawl(stats: &RefCell<CrawlStats>, ctx: &FinalizeCtx) -> Result<i32> {
    let s = stats.borrow();

    // (finding 3) A deliberate stop is not a completion. `done`/`limit`
    // are honest completions of the SELECTED work; a kill file, an
    // interrupt, or an early error/panic mean the phase's own work was
    // cut short, not finished — abandon it, don't complete it.
    if matches!(s.stopped_by, "done" | "limit") {
        let _ = crew::lifecycle::phase_complete(&ctx.phase_id);
    } else {
        let _ = crew::lifecycle::phase_abandon(&ctx.phase_id);
    }

    let total_tokens = s.prompt_tokens_total + s.completion_tokens_total;
    let wall_hours = (s.wall_ms_total as f64) / 1000.0 / 3600.0;
    let tokens_per_hour = if wall_hours > 0.0 { (total_tokens as f64 / wall_hours).round() as u64 } else { 0 };
    // (finding 2) Plan-level, not selection-level: covers a unit excluded
    // by `--param units=`, cut by `--param limit=`, AND a selected unit
    // never reached because the loop stopped early — every reason a
    // plan's unit could go un-attempted, in one number.
    let units_not_run = ctx.units_in_plan.saturating_sub(s.units_completed + s.units_errored);

    let summary = json!({
        "mission_id": ctx.mission_id,
        "workspace": ctx.workspace_name,
        "units_in_plan": ctx.units_in_plan,
        "units_selected": ctx.units_selected,
        "units_not_run": units_not_run,
        "units_completed": s.units_completed,
        "units_errored": s.units_errored,
        "units_skipped": s.units_skipped,
        "findings": s.findings_total,
        "prompt_tokens": s.prompt_tokens_total,
        "completion_tokens": s.completion_tokens_total,
        "wall_ms": s.wall_ms_total,
        "tokens_per_hour": tokens_per_hour,
        "stopped_by": s.stopped_by,
        // (finding 8) Self-describing envelope header.
        "model": s.first_model,
        // Not resolvable from this launcher's DispatchOpts today — crawl
        // always dispatches with `profile_name: None` (default routing),
        // so there is no profile name to report. Present (not omitted) so
        // a reader can tell "checked, and there isn't one" apart from
        // "this launcher never learned to report it."
        "profile": Value::Null,
        "timeout_secs": ctx.timeout_secs,
        "limit": ctx.limit,
        "plan_path": ctx
            .plan_path
            .as_ref()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| "planned in-process".to_string()),
        "units_filter": ctx.units_filter,
    });
    let _ = crew::lifecycle::mission_terminal_with_reasoning_and_payload(
        &ctx.mission_id,
        MissionStatus::Finalized,
        Some(&format!("crawl stopped_by={}", s.stopped_by)),
        Some(summary.clone()),
    );

    // (CONSIDER 4) Printed BEFORE the envelope write below, not after: the
    // Drop path's `let _ = finalize_crawl(...)` swallows this function's
    // `Result`, so an envelope-write failure used to mean the operator saw
    // NEITHER the table nor a written envelope — the one signal that
    // finalize even ran at all was silently lost. Printing first means the
    // table always reaches the operator, even when the write after it
    // fails.
    print_summary_table(
        &ctx.mission_id,
        &ctx.workspace_name,
        s.units_completed,
        s.units_errored,
        s.units_skipped,
        units_not_run,
        s.findings_total,
        total_tokens,
        s.wall_ms_total,
        s.stopped_by,
        &ctx.ledger_path,
    );

    let mut envelope = summary.clone();
    if let Some(obj) = envelope.as_object_mut() {
        obj.insert("units".to_string(), json!(s.per_unit_rows));
    }
    let envelope_path = ctx.runs_dir.join("envelope.json");
    std::fs::write(&envelope_path, serde_json::to_string_pretty(&envelope)?)
        .with_context(|| format!("writing {}", envelope_path.display()))?;

    Ok(match s.stopped_by {
        "kill_file" => 3,
        "interrupted" => 130,
        "error" => 1,
        _ => 0,
    })
}

// ── the launcher itself ─────────────────────────────────────────────────

/// The testable core. `dispatch_fn` is `crew::dispatch::dispatch` in
/// production; tests inject a scripted closure — see this module's doc.
pub(crate) fn run(
    collected: &BTreeMap<String, Value>,
    timeout_seconds: Option<u32>,
    dispatch_fn: &mut dyn FnMut(DispatchOpts) -> Result<DispatchResult>,
) -> Result<i32> {
    // (#1959) `workspace` (a spec path) is required unless `source` is
    // given for a one-shot crawl. `rules` (csv) is required unless the
    // spec carries its own `rules` array (the default binding) or a
    // one-shot `rule` was given.
    let workspace_path = str_param(collected, "workspace").map(PathBuf::from);
    let source_one_shot = str_param(collected, "source").map(str::to_string);
    let rule_one_shot = str_param(collected, "rule").map(str::to_string);
    let rules_csv = str_param(collected, "rules").map(str::to_string);
    let plan_path = str_param(collected, "plan").map(PathBuf::from);
    let units_filter = str_param(collected, "units").map(str::to_string);
    let limit = usize_param(collected, "limit")?;
    let no_fetch = bool_param(collected, "no_fetch");
    let timeout = timeout_seconds.unwrap_or(600);

    let (wspec, wspec_warnings): (WorkspaceSpec, Vec<String>) = match (&workspace_path, &source_one_shot) {
        (Some(wp), _) => WorkspaceSpec::load(wp)
            .with_context(|| format!("loading workspace spec {}", wp.display()))?,
        (None, Some(src)) => {
            let rule_id = rule_one_shot.clone().ok_or_else(|| {
                anyhow!(
                    "darkmux mission launch crawl: --param source=<path|url> requires --param \
                     rule=<id> (a one-shot crawl runs exactly one rule)"
                )
            })?;
            (synthesize_one_shot_spec(src, &rule_id)?, Vec::new())
        }
        (None, None) => bail!(
            "darkmux mission launch crawl: --param workspace=<spec.json> is required (or \
             --param source=<path|url> --param rule=<id> for a one-shot crawl with no spec file)"
        ),
    };
    for w in &wspec_warnings {
        eprintln!("{}", style::warn(w));
    }
    let manifest_name = wspec.effective_name().to_string();

    // (#1959) `--param rules=<csv>` wins when given; else the spec's own
    // `rules` array is the default binding (documented on the crawl
    // config's own inputs); else (the one-shot path already set it) the
    // one-shot `--param rule=`. A workspace spec's `rules` field has no
    // None-vs-empty distinction (it's a plain `Vec<String>`, not an
    // `Option`), so an explicitly-empty `"rules": []` and an ABSENT
    // `rules` key are indistinguishable here — both mean "nothing bound
    // yet." Rather than guess which one the operator meant, an empty
    // `rule_ids` is NOT an error: it plans cleanly to zero units (the
    // "0 units" table below says so loudly), matching the pre-#1959
    // CorpusManifest's own tested behavior for an explicitly-empty
    // `rules: []` — this launcher never second-guesses a resolved-empty
    // rule set into a hard failure.
    let rule_ids: Vec<String> = match &rules_csv {
        Some(csv) => csv.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect(),
        None if !wspec.rules.is_empty() => wspec.rules.clone(),
        None => rule_one_shot.clone().into_iter().collect(),
    };

    let (rules_vec, rule_warnings) = rules::resolve_default(&rule_ids)?;
    for w in &rule_warnings {
        eprintln!("{}", style::warn(w));
    }
    let rules_by_id: BTreeMap<String, Rule> = rules_vec.iter().map(|r| (r.id.clone(), r.clone())).collect();

    let materialized = workspace_spec::materialize(
        &wspec,
        MaterializeOptions { fetch: !no_fetch, read_only: true },
    )
    .with_context(|| format!("materializing workspace '{manifest_name}'"))?;

    let the_plan: Plan = match &plan_path {
        Some(pp) => {
            let text = std::fs::read_to_string(pp).with_context(|| format!("reading plan {}", pp.display()))?;
            let loaded_plan: Plan =
                serde_json::from_str(&text).with_context(|| format!("parsing plan {} as JSON", pp.display()))?;

            // (#1959 merge-gate finding 6a) A plan carries `workspace` (the
            // spec name it was planned from) — bail loudly rather than
            // let a plan minted for a DIFFERENT workspace silently drive
            // this crawl.
            if loaded_plan.workspace != manifest_name {
                bail!(
                    "darkmux mission launch crawl: plan {} was planned from workspace '{}', not \
                     '{}' — pass the plan that matches --param workspace=, or omit --param plan to \
                     plan fresh",
                    pp.display(),
                    loaded_plan.workspace,
                    manifest_name
                );
            }
            // (#1959 merge-gate finding 6b) A schema MAJOR mismatch means
            // this binary's `Plan`/`Unit` shape may not agree with what the
            // file actually holds — non-fatal (lenient-on-read, per
            // CLAUDE.md's config-leniency contract), but the operator
            // should know before trusting the run.
            if let Some(w) = plan_schema_major_mismatch_warning(&loaded_plan.schema_version, plan::PLAN_SCHEMA_VERSION) {
                eprintln!("{}", style::warn(&format!("darkmux mission launch crawl: plan {} — {w}", pp.display())));
            }

            loaded_plan
        }
        None => plan::plan(&materialized, &rules_vec)
            .with_context(|| format!("planning workspace '{manifest_name}'"))?,
    };

    let (selected, truncated) = select_units(&the_plan, units_filter.as_deref(), limit)?;

    // ── validate every selected unit's rule ids resolve BEFORE minting a
    //    mission (#1959 merge-gate finding 1a). A stale `--param plan=`
    //    file can name a rule id the CURRENT `--param workspace=`/`--param rules=` no
    //    longer declares (renamed/removed since the plan was written) —
    //    the sha check below catches a moved SOURCE tree, but says nothing
    //    about the manifest's `rules` list drifting. Bailing here, before
    //    any Mission/Phase/Task/Step record exists, means an operator who
    //    hits this never has a stranded mission to clean up.
    for u in &selected {
        for rule_id in unit_rules(u) {
            if !rules_by_id.contains_key(&rule_id) {
                bail!(
                    "darkmux mission launch crawl: unit `{}` names rule `{rule_id}`, which the \
                     current --param rules=/workspace spec's resolved rule set does not declare \
                     — re-run planning fresh (drop `--param plan=`)",
                    u.id()
                );
            }
        }
    }

    // ── validate every selected unit's SOURCE resolves BEFORE minting a
    //    mission (#1959 merge-gate finding 3). Driven from the sources the
    //    SELECTED UNITS actually name — not from `plan.sources` — because a
    //    plan whose `sources` list is empty (or simply missing an entry a
    //    unit references) would otherwise sail through unvalidated: the
    //    per-unit dispatch loop's `the_plan.sources.iter().find(...)`
    //    falls back to an empty sha (`.unwrap_or_default()`), and every
    //    `crawl.*` record / ledger line for that unit would silently carry
    //    `sha: ""` with no signal anything was ever wrong. Only a LOADED
    //    plan (`--param plan=`) can go stale between planning and launch —
    //    a freshly-built plan's `sources` are the resolved sources by
    //    construction, so the sha/tree freshness half only applies there.
    for u in &selected {
        let source_id = unit_source(u);
        let Some(ps) = the_plan.sources.iter().find(|s| s.id == source_id) else {
            bail!(
                "darkmux mission launch crawl: unit `{}` names source `{source_id}`, which the \
                 plan's `sources` list does not declare — re-run planning fresh (drop \
                 `--param plan=`)",
                u.id()
            );
        };
        if let Some(pp) = &plan_path {
            let Some(rs) = materialized.sources.iter().find(|r| r.id == source_id) else {
                bail!(
                    "darkmux mission launch crawl: plan {} names source '{source_id}', which the \
                     workspace spec no longer declares — re-run planning against the current \
                     spec and pass the fresh plan.json",
                    pp.display()
                );
            };
            if rs.sha != ps.sha {
                bail!(
                    "darkmux mission launch crawl: source '{source_id}' has moved since {} was \
                     written (plan sha {}, resolved tree sha {}) — re-run `darkmux crawl plan` and \
                     pass the fresh plan.json, or omit --param plan to plan fresh",
                    pp.display(),
                    ps.sha,
                    rs.sha
                );
            }
            // (#1959 merge-gate finding 6c) Same tree the sha already
            // vouches for, checked directly — a sha match with a
            // relocated tree path would still dispatch against the
            // wrong on-disk directory.
            if rs.tree != ps.tree {
                bail!(
                    "darkmux mission launch crawl: source '{source_id}' resolves to a different \
                     tree than plan {} recorded ({} vs {}) — re-run `darkmux crawl plan` and pass \
                     the fresh plan.json, or omit --param plan to plan fresh",
                    pp.display(),
                    ps.tree.display(),
                    rs.tree.display()
                );
            }
        }
    }

    // (#1959) `--dry-run`: resolve + plan (everything above this point),
    // print the plan table, mint NOTHING, emit NO flow records, dispatch
    // NOTHING. Writes nothing to disk unless `--param plan_out=<path>` is
    // given — a dry run is read-only by default. Deliberately checked
    // AFTER every validation above (rule ids resolve, plan sha/tree match
    // the resolved sources) so a dry run surfaces the SAME loud failures a
    // real run would, rather than optimistically skipping them.
    if bool_param(collected, "dry_run") {
        let plan_out = str_param(collected, "plan_out").map(PathBuf::from);
        if let Some(op) = &plan_out {
            if let Some(parent) = op.parent() {
                std::fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
            }
            let plan_json = serde_json::to_string_pretty(&the_plan)?;
            std::fs::write(op, &plan_json).with_context(|| format!("writing plan to {}", op.display()))?;
        }
        print_plan_table(&the_plan, plan_out.as_deref());
        return Ok(0);
    }

    // (#1959 merge-gate finding 12) A zero-unit selection is easy to
    // produce by accident (a `--param units=` typo that happens to still
    // parse, an over-narrow filter) and easy to miss buried in a summary
    // table nobody scrolled to — say it loudly, up front.
    if selected.is_empty() {
        eprintln!(
            "{}",
            style::warn(&format!(
                "darkmux mission launch crawl: 0 units selected for workspace '{}' — nothing to \
                 crawl (check --param units=/--param limit= against the resolved plan)",
                manifest_name
            ))
        );
    }

    // ── mint the mission ─────────────────────────────────────────────
    let mission_id = mission_launch::mint_run_id("crawl")?;
    let phase_id = format!("{mission_id}-crawl");
    let now = now_unix();

    // ── kill file / per-run artifact directory (path only — the actual
    //    `create_dir_all` is the LAST fallible call in the guarded mint
    //    window below, since it's the last thing the original code did
    //    before the per-unit loop started). (#1959) Crawl-program state
    //    (the kill file, per-mission runs + ledger) is this launcher's
    //    own concern, distinct from the materialized workspace tree
    //    (`materialized.root`) — with one exception: when the spec sets
    //    an EXPLICIT `root:` override, `materialized.root` IS that
    //    override verbatim (see `WorkspaceSpec::resolved_root()`), and
    //    an operator naming an explicit root is naming "put everything
    //    for this workspace here" — so crawl state reuses the same
    //    directory rather than reaching past it into the real darkmux
    //    root (this also keeps a one-shot / explicitly-rooted spec
    //    self-contained under the root the operator or a test fixture
    //    chose). Only the DEFAULT (no override) case gets the separate
    //    `<darkmux root>/crawl/<name>/` home, so a workspace spec shared
    //    with another mission kind (e.g. review) doesn't collide with
    //    another mission's default-rooted state.
    let crawl_root = match wspec.root.as_deref() {
        Some(r) if !r.trim().is_empty() => materialized.root.clone(),
        _ => darkmux_types::paths::resolve(darkmux_types::paths::ResolveScope::Auto)
            .root
            .join("crawl")
            .join(&manifest_name),
    };
    let kill_file = crawl_root.join("STOP");
    let runs_dir = crawl_root.join("runs").join(&mission_id);
    let ledger_path = runs_dir.join("ledger.jsonl");

    let spec = MissionSpec {
        config_id: "crawl".to_string(),
        inputs_fingerprint: mission_launch::spec_fingerprint(collected)?,
        origin: Some(MissionSpecOrigin::Builtin),
    };
    let mission = Mission {
        id: mission_id.clone(),
        description: format!("Crawl — {} ({} unit(s) selected)", manifest_name, selected.len()),
        status: MissionStatus::Active,
        phase_ids: vec![phase_id.clone()],
        created_ts: now,
        started_ts: None,
        finalized_ts: None,
        paused_ts: None,
        source_input: None,
        ticket: None,
        spec: Some(spec),
    };
    crew::lifecycle::save_mission(&mission).context("persisting mission.json")?;

    let sources_summary: Vec<Value> = the_plan.sources.iter().map(|s| json!({"id": s.id, "sha": s.sha})).collect();
    let est_tokens_total: usize = selected.iter().map(|u| u.est_tokens()).sum();
    // (#1959 merge-gate finding 2) `units_planned` renamed to
    // `units_selected` (the plan may hold more units than were actually
    // selected for THIS run — `units_in_plan` is that pre-selection
    // count). No `units_not_run` here yet — nothing has run at start.
    let units_in_plan = the_plan.units.len();
    let units_selected = selected.len();
    let mut step_id_by_index: Vec<String> = vec![String::new(); selected.len()];

    // ── guarded mint window (#1959 merge-gate finding 1) ─────────────────
    // `mission.json` now exists on disk, so every fallible call from here
    // through the mission-specific runs-dir creation below is a strand
    // window: a bare `?` on any of these would leave a partially-minted
    // Active mission behind with no reconcile. Route every failure through
    // `reconcile_mint_failure` (closes the mission terminal via the SAME
    // generic `mission close`/`mission abort` record every other mission
    // uses, cascading any partially-minted Planned/Running phase — and its
    // steps — to Abandoned, before propagating) — mirroring
    // `dispatch_as_crew_of_one`'s identical guarding of its own post-mint
    // setup calls. Unlike the retired bespoke `crawl.mission.*` pairing,
    // there is nothing extra to track here: `reconcile_mint_failure` is a
    // no-op when `mission.json` was never written, and always emits the
    // paired terminal record when it was — whether or not `mission start`
    // itself got as far as running.
    let tree_root = materialized.root.join("tree");
    let mint_result: Result<()> = (|| {
        let phase = Phase {
            id: phase_id.clone(),
            mission_id: mission_id.clone(),
            description: format!(
                "Sequential crawl of {} unit(s) across {} source(s)",
                selected.len(),
                the_plan.sources.len()
            ),
            display_name: Some("Crawl".to_string()),
            status: PhaseStatus::Planned,
            created_ts: now,
            started_ts: None,
            completed_ts: None,
            abandoned_ts: None,
            task_ids: Vec::new(),
        };
        crew::lifecycle::save_phase(&phase).context("persisting phase")?;

        // ── group selected units into Tasks by (source, rule); one Step
        //    per unit, in plan order, so `mission status` / the viewer show
        //    real structure instead of one flat task per unit ───────────
        let mut groups: Vec<TaskGroup> = Vec::new();
        let mut group_index: BTreeMap<(String, String), usize> = BTreeMap::new();
        for (i, u) in selected.iter().enumerate() {
            let key = group_key(u);
            let gi = *group_index.entry(key.clone()).or_insert_with(|| {
                let idx = groups.len();
                groups.push(TaskGroup {
                    id: format!("{phase_id}-task-{:03}", idx + 1),
                    source: key.0.clone(),
                    rules_label: key.1.clone(),
                    unit_indices: Vec::new(),
                });
                idx
            });
            groups[gi].unit_indices.push(i);
        }

        let mut task_ids: Vec<String> = Vec::new();
        for g in &groups {
            let mut step_ids_for_task: Vec<String> = Vec::new();
            for &i in &g.unit_indices {
                let unit = selected[i];
                let step_id = format!("{}-step-{:04}", g.id, step_ids_for_task.len() + 1);
                step_id_by_index[i] = step_id.clone();
                step_ids_for_task.push(step_id.clone());
                let step = Step {
                    id: step_id,
                    task_id: g.id.clone(),
                    // A data label only — never looked up in the StepKind
                    // registry. This module drives execution with a plain
                    // sequential loop, not `run_step_graph`; see the module doc.
                    kind: "crawl.unit".to_string(),
                    gate: None,
                    status: NodeStatus::Planned,
                    config: json!({ "unit": unit.id(), "kind": unit_kind(unit), "source": unit_source(unit) }),
                    started_ts: None,
                    completed_ts: None,
                    output: None,
                };
                crew::lifecycle::save_step(&mission_id, &phase_id, &step).context("persisting step")?;
            }
            let task = Task {
                id: g.id.clone(),
                phase_id: phase_id.clone(),
                description: format!("Crawl `{}` against source '{}'", g.rules_label, g.source),
                display_name: Some(format!("{} · {}", g.source, g.rules_label)),
                step_ids: step_ids_for_task,
                depends_on: Vec::new(),
                reads: Vec::new(),
                role_id: Some("crawler".to_string()),
                profile_name: None,
                workdir: Some(tree_root.clone()),
                image: None,
            };
            crew::lifecycle::save_task(&mission_id, &task).context("persisting task")?;
            task_ids.push(g.id.clone());
        }

        let mut phase = phase;
        phase.task_ids = task_ids;
        crew::lifecycle::save_phase(&phase).context("persisting phase task_ids")?;

        crew::lifecycle::mission_start_with_reasoning_and_payload(
            &mission_id,
            Some("launched from `darkmux mission launch crawl`"),
            Some(json!({
                "workspace": manifest_name,
                "units_in_plan": units_in_plan,
                "units_selected": units_selected,
                "est_tokens": est_tokens_total,
                "sources": sources_summary,
            })),
        )
        .context("starting the newly-minted mission")?;
        crew::lifecycle::phase_start(&phase_id).context("starting the crawl phase")?;

        // (#1959 merge-gate finding 1) The last fallible step of the mint
        // window — a failure here (e.g. `<darkmux root>/crawl/<name>/runs` already exists
        // as a regular file) is the one the operator is most likely to hit
        // in practice, and it now unwinds through `reconcile_mint_failure`'s
        // own paired `mission close`/`mission abort` instead of leaving a
        // stranded Active mission behind.
        std::fs::create_dir_all(&runs_dir).with_context(|| format!("creating {}", runs_dir.display()))?;

        Ok(())
    })();

    if let Err(e) = mint_result {
        // (#1959) A best-effort payload — every count is honestly zero
        // (nothing ran before the mint window itself failed), but
        // `workspace`/`units_in_plan`/`units_selected` are already known
        // and `stopped_by: "error"` is the one signal a reader actually
        // needs from this record.
        let payload = json!({
            "workspace": manifest_name,
            "units_in_plan": units_in_plan,
            "units_selected": units_selected,
            "units_not_run": units_in_plan,
            "units_completed": 0,
            "units_errored": 0,
            "units_skipped": 0,
            "findings": 0,
            "prompt_tokens": 0,
            "completion_tokens": 0,
            "wall_ms": 0,
            "tokens_per_hour": 0,
            "stopped_by": "error",
            "model": Value::Null,
            "profile": Value::Null,
            "timeout_secs": timeout,
            "limit": limit,
            "plan_path": plan_path.as_ref().map(|p| p.display().to_string()).unwrap_or_else(|| "planned in-process".to_string()),
            "units_filter": units_filter,
        });
        crew::lifecycle::reconcile_mint_failure_with_payload(
            &mission_id,
            &format!("mission launch crawl errored during mint: {e:#}"),
            Some(payload),
        );
        return Err(e);
    }

    #[cfg(unix)]
    darkmux_types::interrupt::install();

    let stats = RefCell::new(CrawlStats {
        stopped_by: if truncated { "limit" } else { "done" },
        units_completed: 0,
        units_errored: 0,
        units_skipped: 0,
        findings_total: 0,
        prompt_tokens_total: 0,
        completion_tokens_total: 0,
        wall_ms_total: 0,
        per_unit_rows: Vec::new(),
        first_model: None,
    });
    // (finding 1b) Armed here, before the loop starts — see the guard's
    // own doc for what "armed" guarantees on an early return or panic.
    let mut guard = CrawlFinalizeGuard::new(
        &stats,
        FinalizeCtx {
            mission_id: mission_id.clone(),
            phase_id: phase_id.clone(),
            workspace_name: manifest_name.clone(),
            units_in_plan,
            units_selected,
            runs_dir: runs_dir.clone(),
            ledger_path: ledger_path.clone(),
            timeout_secs: timeout,
            limit,
            plan_path: plan_path.clone(),
            units_filter: units_filter.clone(),
        },
    );

    for (i, unit) in selected.iter().enumerate() {
        if kill_file.exists() {
            let mut s = stats.borrow_mut();
            s.stopped_by = "kill_file";
            s.units_skipped = selected.len() - i;
            break;
        }
        #[cfg(unix)]
        if darkmux_types::interrupt::is_set() {
            let mut s = stats.borrow_mut();
            s.stopped_by = "interrupted";
            s.units_skipped = selected.len() - i;
            break;
        }

        let source = unit_source(unit).to_string();
        let sha = the_plan.sources.iter().find(|s| s.id == source).map(|s| s.sha.clone()).unwrap_or_default();
        let rule_ids = unit_rules(unit);
        let kind = unit_kind(unit);

        let step_id = step_id_by_index[i].clone();

        // (#1959 merge-gate finding 5) Computed BEFORE `step start` is
        // emitted, so that record's `session_id` carries the UNIT's own
        // session — it used to carry the mission id (the same value every
        // OTHER record for this mission already carries), which made this
        // the one record in the family you couldn't correlate to its
        // matching completion by session id alone.
        let session_id = format!("crawl-{mission_id}-{}", unit.id());

        let started_payload = match unit {
            Unit::Site { sites, .. } => json!({
                "workspace": manifest_name, "unit": unit.id(), "source": source, "sha": sha,
                "rule": rule_ids, "kind": kind, "est_tokens": unit.est_tokens(), "sites": sites.len(),
            }),
            Unit::Read { files, .. } => json!({
                "workspace": manifest_name, "unit": unit.id(), "source": source, "sha": sha,
                "rule": rule_ids, "kind": kind, "est_tokens": unit.est_tokens(), "files": files.len(),
            }),
            Unit::Edge { sites, .. } => json!({
                "workspace": manifest_name, "unit": unit.id(), "source": source, "sha": sha,
                "rule": rule_ids, "kind": kind, "est_tokens": unit.est_tokens(), "sites": sites.len(),
            }),
        };
        if let Ok(mut step) = crew::lifecycle::load_step(&mission_id, &phase_id, &step_id) {
            step.status = NodeStatus::Running;
            step.started_ts = Some(now_unix());
            let _ = crew::lifecycle::save_step(&mission_id, &phase_id, &step);
            let _ = darkmux_flow::record(unit_step_record(
                &step,
                "step start",
                &mission_id,
                &session_id,
                started_payload,
                None,
            ));
        }

        let message = build_message(&rules_by_id, unit)?;
        let opts = DispatchOpts {
            role_id: "crawler".to_string(),
            message,
            session_id: Some(session_id.clone()),
            timeout_seconds: timeout,
            skip_preflight: false,
            json: true,
            workdir: Some(tree_root.clone()),
            phase_id: Some(phase_id.clone()),
            machine: None,
            wait: true,
            compaction: CompactionDispatchArgs::default(),
            profile_name: None,
            config_path: None,
            force_container: false,
            max_completion_tokens: None,
            image: None,
            model_base_url_override: None,
            step_id: Some(step_id.clone()),
            system_prompt_override: None,
            workspace_read_only: true,
            // (#1959 flow-record vocabulary retirement) Provenance the
            // runtime cannot know — merged by the host tailer under
            // `payload.context` on every record this unit's dispatch
            // produces (`dispatch.tool`, the bookends, …; see
            // `dispatch_internal.rs`'s `merge_record_context`).
            record_context: Some(json!({
                "workspace": manifest_name,
                "source": source,
                "sha": sha,
                "rule": rule_ids,
                "unit": unit.id(),
            })),
        };

        let dispatch_outcome = dispatch_fn(opts);

        let (mut result_label, wall_ms, prompt_tok, completion_tok, model, detections, rest_ms) = match &dispatch_outcome {
            Err(_) => ("error".to_string(), 0u64, 0u64, 0u64, None, None, 0u64),
            Ok(res) => interpret_dispatch_result(unit.id(), res),
        };
        // (#1959 merge-gate finding 13) SIGINT may have arrived WHILE this
        // unit's dispatch was in flight — the container often comes back
        // as a non-clean exit in that case, which would otherwise read as
        // an ordinary per-unit "error". Read at THIS point (right after
        // the dispatch returns, before the next unit's own kill-file/
        // interrupt check), so it names this exact unit's own outcome.
        // Checked here, not in `is_error` alone, because the payload/step
        // output text itself needs to say "interrupted", not "error".
        #[cfg(unix)]
        let interrupted_at_readback = darkmux_types::interrupt::is_set();
        #[cfg(not(unix))]
        let interrupted_at_readback = false;
        if interrupted_at_readback {
            result_label = "interrupted".to_string();
        }

        if let Some(m) = &model {
            let mut s = stats.borrow_mut();
            if s.first_model.is_none() {
                s.first_model = Some(m.clone());
            }
        }

        let mut findings_n = 0usize;
        // (#1959) `exclusions` — rejected `report_finding` calls this unit
        // made, read from the SAME `trajectory.jsonl` the tailer streams
        // live (see runtime::failure_rate::classify_outcome's REJECTED/
        // NOT-RECORDED classification): a cheap sibling scan to the
        // findings.jsonl read just below, giving an accurate count rather
        // than inferring it from the accepted-findings count alone.
        let exclusions_n = dispatch_outcome
            .as_ref()
            .ok()
            .and_then(|res| res.out_dir.as_deref())
            .map(count_rejected_report_findings)
            .unwrap_or(0);
        if let Ok(res) = &dispatch_outcome {
            if let Some(out_dir) = &res.out_dir {
                let findings_path = out_dir.join(".darkmux-runtime").join("findings.jsonl");
                if let Ok(body) = std::fs::read_to_string(&findings_path) {
                    let mut ledger_buf = String::new();
                    for line in body.lines().filter(|l| !l.trim().is_empty()) {
                        let Ok(mut rec) = serde_json::from_str::<Value>(line) else { continue };
                        findings_n += 1;
                        if let Some(obj) = rec.as_object_mut() {
                            let raw_file = obj.get(FINDING_FILE_KEY).and_then(Value::as_str).unwrap_or("").to_string();
                            let rel = strip_source_prefix(&source, &raw_file);
                            obj.insert("file_raw".to_string(), json!(raw_file));
                            obj.insert(FINDING_FILE_KEY.to_string(), json!(rel));
                            obj.insert("workspace".to_string(), json!(manifest_name));
                            obj.insert("unit".to_string(), json!(unit.id()));
                            obj.insert("source".to_string(), json!(source));
                            obj.insert("sha".to_string(), json!(sha));
                            // (#1959) `rule` is ONE id — the pattern the model reported this
                            // finding under — never the unit's whole list: the hook receiver
                            // keys finding identity on it and refuses an array. The unit's full
                            // list rides alongside as `rules`.
                            let pattern = obj.get("pattern").and_then(Value::as_str).map(str::to_string);
                            let (rule_id, unmatched) = finding_rule_for(pattern.as_deref(), &rule_ids);
                            obj.insert("rule".to_string(), json!(rule_id));
                            obj.insert("rules".to_string(), json!(rule_ids));
                            if let Some(u) = unmatched {
                                obj.insert("rule_unmatched_pattern".to_string(), json!(u));
                            }
                            obj.insert("session_id".to_string(), json!(session_id));
                            if let Some(m) = &model {
                                obj.insert("model".to_string(), json!(m));
                            }
                        }
                        let line_out = serde_json::to_string(&rec).unwrap_or_default();
                        ledger_buf.push_str(&line_out);
                        ledger_buf.push('\n');
                        // (#1959, revised) No flow record here — a finding
                        // is never a special record. The runtime already
                        // classified this `report_finding` call's outcome
                        // on the `dispatch.tool` record its own tailer
                        // emitted (`payload.ok: true` for an accepted
                        // finding), and `DispatchOpts::record_context`
                        // (set on this unit's dispatch, above)
                        // carried this launcher's provenance under
                        // `payload.context` on that SAME record. The
                        // ledger line just written IS the durable,
                        // harness-verified record of this finding.
                    }
                    if !ledger_buf.is_empty() {
                        let _ = append_file(&ledger_path, &ledger_buf);
                    }
                    let _ = std::fs::copy(&findings_path, runs_dir.join(format!("{}.findings.jsonl", unit.id())));
                }
            }
        }

        // (#1959 merge-gate finding 9) "timeout" is also a failure for
        // accounting purposes — anything other than a clean "stop" means
        // this unit did not complete cleanly. (finding 13) An interrupted
        // unit is neither a completion nor a per-unit failure — it's
        // counted in neither bucket, which means it flows into `units_
        // not_run` (finding 2's `units_in_plan - completed - errored`)
        // automatically rather than needing a THIRD counter.
        let is_error = !interrupted_at_readback && (dispatch_outcome.is_err() || result_label != "stop");
        {
            let mut s = stats.borrow_mut();
            if interrupted_at_readback {
                // neither bucket — see the comment above.
            } else if is_error {
                s.units_errored += 1;
            } else {
                s.units_completed += 1;
            }
            s.findings_total += findings_n;
            s.prompt_tokens_total += prompt_tok;
            s.completion_tokens_total += completion_tok;
            s.wall_ms_total += wall_ms;
        }

        let mut completed_payload = json!({
            "workspace": manifest_name,
            "unit": unit.id(),
            "source": source,
            "sha": sha,
            "rule": rule_ids,
            "result": result_label,
            "findings": findings_n,
            "exclusions": exclusions_n,
            "prompt_tokens": prompt_tok,
            "completion_tokens": completion_tok,
            "wall_ms": wall_ms,
            "rest_ms": rest_ms,
        });
        if let Some(d) = &detections {
            completed_payload["detections"] = d.clone();
        }
        if let Some(m) = &model {
            completed_payload["model"] = json!(m);
        }
        if let Ok(mut step) = crew::lifecycle::load_step(&mission_id, &phase_id, &step_id) {
            step.status = if interrupted_at_readback {
                NodeStatus::Abandoned
            } else if is_error {
                NodeStatus::Error
            } else {
                NodeStatus::Complete
            };
            step.completed_ts = Some(now_unix());
            step.output = Some(result_label.clone());
            let _ = crew::lifecycle::save_step(&mission_id, &phase_id, &step);
            // (#1959, revised) `STEP_LIFECYCLE_ACTIONS` has no dedicated
            // "abandon" action — a kill-file/interrupted unit's step
            // reaches `NodeStatus::Abandoned` on disk but is reported
            // through the SAME `"step error"` action every genuine error
            // uses; `payload.result` (already `"interrupted"` vs
            // `"error"`/`"timeout"`) is where the real nuance lives, the
            // same design the retired `crawl.unit.completed` action used
            // for all four outcomes under one action string.
            let action = match step.status {
                NodeStatus::Complete => "step complete",
                _ => "step error",
            };
            let _ = darkmux_flow::record(unit_step_record(
                &step,
                action,
                &mission_id,
                &session_id,
                completed_payload,
                model.as_deref(),
            ));
        }

        stats.borrow_mut().per_unit_rows.push(json!({
            "unit": unit.id(),
            "source": source,
            "rule": rule_ids,
            "result": result_label,
            "findings": findings_n,
            "prompt_tokens": prompt_tok,
            "completion_tokens": completion_tok,
            "wall_ms": wall_ms,
            "model": model,
        }));

        #[cfg(unix)]
        if darkmux_types::interrupt::is_set() {
            let mut s = stats.borrow_mut();
            s.stopped_by = "interrupted";
            s.units_skipped = selected.len() - (i + 1);
            break;
        }
    }

    // ── finalize (normal path) — disarms the guard ──────────────────────
    guard.close()
}

/// (#1959) Ported from the retired `darkmux crawl plan` CLI's own
/// `print_plan_table` (deleted alongside the standalone verb — see
/// `src/cli.rs`'s Crawl retirement commit) — renders `--dry-run`'s plan
/// table. `out_path` is `Some` only when `--param plan_out=<path>` named a
/// destination; `None` means the dry run wrote nothing (the default).
fn print_plan_table(the_plan: &plan::Plan, out_path: Option<&Path>) {
    println!("{}", style::header(&format!("darkmux mission launch crawl --dry-run — {}", the_plan.workspace)));
    println!("{}", style::dim(&format!("planned_at: {}", the_plan.planned_at)));
    match out_path {
        Some(p) => println!("{}", style::dim(&format!("written to: {}", p.display()))),
        None => println!("{}", style::dim("written to: (not written — pass --param plan_out=<path> to write)")),
    }
    println!();

    println!("{}", style::header("sources"));
    if the_plan.sources.is_empty() {
        println!("  (no sources)");
    } else {
        for s in &the_plan.sources {
            let short_sha = &s.sha[..s.sha.len().min(8)];
            println!("  {:<16} {:<10} files_walked={}", s.id, short_sha, s.files_walked);
        }
    }
    println!();

    println!("{}", style::header("by rule"));
    if the_plan.totals.by_rule.is_empty() {
        println!("  (no rules matched anything)");
    } else {
        for (rule_id, t) in &the_plan.totals.by_rule {
            let extent = match (t.sites, t.files) {
                (Some(n), _) => format!("sites={n}"),
                (_, Some(n)) => format!("files={n}"),
                _ => "extent=0".to_string(),
            };
            // #1959 finding 17 (carried over from the retired CLI): a read
            // unit shared with another active read rule contributes its
            // est_tokens to EVERY rule sharing it — flag it so the
            // per-rule sums visibly overlap totals.est_tokens instead of
            // silently outrunning it.
            let shared_marker = if t.shared { " (shared read pass)" } else { "" };
            println!(
                "  {:<24} units={:<4} {:<14} est_tokens={}{shared_marker}",
                rule_id, t.units, extent, t.est_tokens
            );
        }
    }
    println!();

    // The load-bearing line: a plan that matched nothing must say so
    // loudly, not print an empty section that reads as success-by-silence.
    println!(
        "{}",
        style::header(&format!("totals: {} units, {} est_tokens", the_plan.totals.units, the_plan.totals.est_tokens))
    );

    if the_plan.totals.skipped.is_empty() {
        println!("  skipped: (none)");
    } else {
        let n = the_plan.totals.skipped.len();
        let noun = if n == 1 { "file" } else { "files" };
        println!("  skipped: {n} {noun}");
        for s in &the_plan.totals.skipped {
            println!("    {} — {}", s.file, s.reason);
        }
    }

    if the_plan.totals.edges.is_empty() {
        println!("  edges: (none)");
    } else {
        println!("  edges: {} checked", the_plan.totals.edges.len());
        for e in &the_plan.totals.edges {
            let admits = match e.range_admits {
                Some(true) => "admits",
                Some(false) => "STALE",
                None => "unknown",
            };
            println!(
                "    {} -> {} ({}) [{admits}]{}",
                e.consumer,
                e.library,
                e.package,
                e.note.as_ref().map(|n| format!(" — {n}")).unwrap_or_default()
            );
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn print_summary_table(
    mission_id: &str,
    workspace: &str,
    units_completed: usize,
    units_errored: usize,
    units_skipped: usize,
    units_not_run: usize,
    findings: usize,
    total_tokens: u64,
    wall_ms: u64,
    stopped_by: &str,
    ledger_path: &Path,
) {
    println!("{}", style::header(&format!("darkmux mission launch crawl — {workspace} ({mission_id})")));
    println!(
        "  units: {units_completed} completed, {units_errored} errored, {units_skipped} skipped, \
         {units_not_run} not run"
    );
    println!("  findings: {findings}");
    println!("  tokens: {total_tokens}   wall: {wall_ms}ms");
    println!("  stopped_by: {stopped_by}");
    // (#1959 merge-gate finding 12) No point pointing at a ledger file
    // that was never written — a run with zero findings never appends to
    // it, so the path would just dangle.
    if findings > 0 {
        println!("  ledger: {}", ledger_path.display());
    }
}

#[cfg(test)]
#[path = "crawl_launch_tests.rs"]
mod tests;
