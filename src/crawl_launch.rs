//! `darkmux mission launch crawl` — the crawl LAUNCHER (#1959 packet 2).
//!
//! Packet 1 (`crates/darkmux-lab/src/crawl/`, `darkmux crawl plan`) is the
//! mechanical, free-to-compute half: it turns a corpus manifest into a
//! deterministic [`plan::Plan`] of work [`plan::Unit`]s with token
//! estimates. NO model dispatch happens there. This module is the other
//! half — it walks that plan SEQUENTIALLY, dispatching each unit to the
//! `crawler` role with the corpus tree mounted read-only, and records what
//! came back.
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
//! every exit path. The `crawl.*` records this module emits (`crawl.
//! mission.started/completed`, `crawl.unit.started/completed`, `crawl.
//! finding`) are descriptive scaffolding around those bookends, not a
//! replacement for them.
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
use darkmux_lab::crawl::manifest::CorpusManifest;
use darkmux_lab::crawl::plan::{self, Plan, ReadFileEntry, Site, Unit};
use darkmux_lab::crawl::rules::{self, Rule};
use darkmux_lab::crawl::sources;
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
/// `config_id == "crawl"`.
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
    // Full container paths: the workspace root is the CORPUS tree, so a path
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
/// detections)` out of a dispatch's `--json` envelope (`res.stdout`).
/// `result` is `"stop"` on a clean finish, `"timeout"` when `stderr`
/// carries the host watchdog's marker (see [`watchdog_timeout_fired`]),
/// else `"error"` (a hard-to-parse envelope, `max_turns`, an escalation
/// variant, or a bare non-zero exit with neither signal). When `stdout`
/// is non-empty but doesn't parse as the expected JSON envelope, this
/// prints a warning naming the unit and the first 120 chars — silent
/// swallowing here previously meant a model that broke the `--json`
/// contract read as an ordinary clean "stop" whenever `exit_code == 0`.
fn interpret_dispatch_result(unit_id: &str, res: &DispatchResult) -> (String, u64, u64, u64, Option<String>, Option<Value>) {
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
    (result_label, wall_ms, prompt_tok, completion_tok, model, detections)
}

// ── flow records ─────────────────────────────────────────────────────────

fn crawl_record(
    action: &str,
    mission_id: &str,
    session_id: Option<&str>,
    payload: Value,
    model: Option<&str>,
) -> darkmux_flow::FlowRecord {
    darkmux_flow::FlowRecord {
        ts: darkmux_flow::ts_utc_now(),
        level: darkmux_flow::Level::Info,
        category: darkmux_flow::Category::Work,
        tier: darkmux_flow::Tier::Local,
        stage: darkmux_flow::Stage::Dispatch,
        action: action.to_string(),
        handle: "crawler".to_string(),
        phase_id: None,
        session_id: session_id.map(String::from),
        source: Some("crawl".to_string()),
        model: model.map(String::from),
        reasoning: None,
        mission_id: Some(mission_id.to_string()),
        machine_id: None,
        machine_uid: None,
        prev_hash: None,
        hash: None,
        payload: Some(payload),
        work_id: None,
        attempt: None,
    }
}

/// Strip a container-path prefix off a finding's `file` field — either the
/// absolute form (`/workspace/<source-id>/<rel>`, the literal contract
/// this launcher's spec names) or the bare relative form
/// (`<source-id>/<rel>`, since a model may have copied the exact scope
/// listing this launcher itself handed it). Falls through unchanged when
/// neither prefix matches (a model that ignored the given source id
/// entirely — rare, but the raw value survives in `file_raw` either way).
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
    corpus_name: String,
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
/// matching `crawl.mission.completed` record, a written envelope, and a
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
        // Best-effort, same as every other lifecycle call in this module
        // (`let _ = ...`): a `Drop` can't propagate a `Result`, and this
        // guard IS the last-resort finalize — there's nowhere further to
        // report a failure to.
        let _ = finalize_crawl(self.stats, &self.ctx);
    }
}

/// Shared finalize logic for both the normal end-of-loop path
/// ([`CrawlFinalizeGuard::close`]) and the abort path
/// ([`CrawlFinalizeGuard`]'s `Drop`): transitions the phase to its correct
/// terminal (finding 3), closes the mission, emits `crawl.mission.
/// completed`, writes the envelope, and prints the summary table.
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
    let _ = crew::lifecycle::mission_close_with_reasoning(
        &ctx.mission_id,
        Some(&format!("crawl stopped_by={}", s.stopped_by)),
    );

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
        "corpus": ctx.corpus_name,
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
    let _ = darkmux_flow::record(crawl_record(
        "crawl.mission.completed",
        &ctx.mission_id,
        Some(&ctx.mission_id),
        summary.clone(),
        s.first_model.as_deref(),
    ));

    let mut envelope = summary.clone();
    if let Some(obj) = envelope.as_object_mut() {
        obj.insert("units".to_string(), json!(s.per_unit_rows));
    }
    let envelope_path = ctx.runs_dir.join("envelope.json");
    std::fs::write(&envelope_path, serde_json::to_string_pretty(&envelope)?)
        .with_context(|| format!("writing {}", envelope_path.display()))?;

    print_summary_table(
        &ctx.mission_id,
        &ctx.corpus_name,
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
    let corpus_path = str_param(collected, "corpus")
        .map(PathBuf::from)
        .ok_or_else(|| anyhow!("darkmux mission launch crawl: --param corpus=<manifest.json> is required"))?;
    let plan_path = str_param(collected, "plan").map(PathBuf::from);
    let units_filter = str_param(collected, "units").map(str::to_string);
    let limit = usize_param(collected, "limit")?;
    let no_fetch = bool_param(collected, "no_fetch");
    let timeout = timeout_seconds.unwrap_or(600);

    let (manifest, manifest_warnings) = CorpusManifest::load(&corpus_path)
        .with_context(|| format!("loading corpus manifest {}", corpus_path.display()))?;
    for w in &manifest_warnings {
        eprintln!("{}", style::warn(w));
    }

    let (rules_vec, rule_warnings) = rules::resolve_default(&manifest.rules)?;
    for w in &rule_warnings {
        eprintln!("{}", style::warn(w));
    }
    let rules_by_id: BTreeMap<String, Rule> = rules_vec.iter().map(|r| (r.id.clone(), r.clone())).collect();

    let resolved_sources = sources::resolve(&manifest, !no_fetch)
        .with_context(|| format!("resolving sources for corpus '{}'", manifest.name))?;

    let the_plan: Plan = match &plan_path {
        Some(pp) => {
            let text = std::fs::read_to_string(pp).with_context(|| format!("reading plan {}", pp.display()))?;
            let loaded_plan: Plan =
                serde_json::from_str(&text).with_context(|| format!("parsing plan {} as JSON", pp.display()))?;

            // (#1959 merge-gate finding 6a) A plan carries `corpus` (the
            // manifest name it was planned from) — bail loudly rather than
            // let a plan minted for a DIFFERENT manifest silently drive
            // this crawl.
            if loaded_plan.corpus != manifest.name {
                bail!(
                    "darkmux mission launch crawl: plan {} was planned from corpus '{}', not \
                     '{}' — pass the plan that matches --param corpus=, or omit --param plan to \
                     plan fresh",
                    pp.display(),
                    loaded_plan.corpus,
                    manifest.name
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

            for ps in &loaded_plan.sources {
                let Some(rs) = resolved_sources.iter().find(|r| r.id == ps.id) else {
                    bail!(
                        "darkmux mission launch crawl: plan {} names source '{}', which the corpus \
                         manifest no longer declares — re-run `darkmux crawl plan` against the current \
                         manifest and pass the fresh plan.json",
                        pp.display(),
                        ps.id
                    );
                };
                if rs.sha != ps.sha {
                    bail!(
                        "darkmux mission launch crawl: source '{}' has moved since {} was written \
                         (plan sha {}, resolved tree sha {}) — re-run `darkmux crawl plan` and pass the \
                         fresh plan.json, or omit --param plan to plan fresh",
                        ps.id,
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
                        "darkmux mission launch crawl: source '{}' resolves to a different tree \
                         than plan {} recorded ({} vs {}) — re-run `darkmux crawl plan` and pass \
                         the fresh plan.json, or omit --param plan to plan fresh",
                        ps.id,
                        pp.display(),
                        ps.tree.display(),
                        rs.tree.display()
                    );
                }
            }
            loaded_plan
        }
        None => plan::plan(&manifest, &rules_vec, &resolved_sources)
            .with_context(|| format!("planning corpus '{}'", manifest.name))?,
    };

    let (selected, truncated) = select_units(&the_plan, units_filter.as_deref(), limit)?;

    // ── validate every selected unit's rule ids resolve BEFORE minting a
    //    mission (#1959 merge-gate finding 1a). A stale `--param plan=`
    //    file can name a rule id the CURRENT `--param corpus=` manifest no
    //    longer declares (renamed/removed since the plan was written) —
    //    the sha check above catches a moved SOURCE tree, but says nothing
    //    about the manifest's `rules` list drifting. Bailing here, before
    //    any Mission/Phase/Task/Step record exists, means an operator who
    //    hits this never has a stranded mission to clean up.
    for u in &selected {
        for rule_id in unit_rules(u) {
            if !rules_by_id.contains_key(&rule_id) {
                bail!(
                    "darkmux mission launch crawl: unit `{}` names rule `{rule_id}`, which the \
                     current corpus manifest's resolved rule set does not declare — re-run \
                     `darkmux crawl plan` for this manifest, or drop `--param plan=` to plan \
                     fresh",
                    u.id()
                );
            }
        }
    }

    // (#1959 merge-gate finding 12) A zero-unit selection is easy to
    // produce by accident (a `--param units=` typo that happens to still
    // parse, an over-narrow filter) and easy to miss buried in a summary
    // table nobody scrolled to — say it loudly, up front.
    if selected.is_empty() {
        eprintln!(
            "{}",
            style::warn(&format!(
                "darkmux mission launch crawl: 0 units selected for corpus '{}' — nothing to \
                 crawl (check --param units=/--param limit= against the resolved plan)",
                manifest.name
            ))
        );
    }

    // ── mint the mission ─────────────────────────────────────────────
    let mission_id = mission_launch::mint_run_id("crawl")?;
    let phase_id = format!("{mission_id}-crawl");
    let now = now_unix();

    let spec = MissionSpec {
        config_id: "crawl".to_string(),
        inputs_fingerprint: mission_launch::spec_fingerprint(collected)?,
        origin: Some(MissionSpecOrigin::Builtin),
    };
    let mission = Mission {
        id: mission_id.clone(),
        description: format!("Crawl — {} ({} unit(s) selected)", manifest.name, selected.len()),
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

    let tree_root = manifest.resolved_root().join("tree");
    let phase = Phase {
        id: phase_id.clone(),
        mission_id: mission_id.clone(),
        description: format!("Sequential crawl of {} unit(s) across {} source(s)", selected.len(), the_plan.sources.len()),
        display_name: Some("Crawl".to_string()),
        status: PhaseStatus::Planned,
        created_ts: now,
        started_ts: None,
        completed_ts: None,
        abandoned_ts: None,
        task_ids: Vec::new(),
    };
    crew::lifecycle::save_phase(&phase).context("persisting phase")?;

    // ── group selected units into Tasks by (source, rule); one Step per
    //    unit, in plan order, so `mission status` / the viewer show real
    //    structure instead of one flat task per unit ──────────────────
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

    let mut step_id_by_index: Vec<String> = vec![String::new(); selected.len()];
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

    crew::lifecycle::mission_start_with_reasoning(&mission_id, Some("launched from `darkmux mission launch crawl`"))
        .context("starting the newly-minted mission")?;
    crew::lifecycle::phase_start(&phase_id).context("starting the crawl phase")?;

    let sources_summary: Vec<Value> = the_plan.sources.iter().map(|s| json!({"id": s.id, "sha": s.sha})).collect();
    let est_tokens_total: usize = selected.iter().map(|u| u.est_tokens()).sum();
    // (#1959 merge-gate finding 2) `units_planned` renamed to
    // `units_selected` (the plan may hold more units than were actually
    // selected for THIS run — `units_in_plan` is that pre-selection
    // count). No `units_not_run` here yet — nothing has run at start.
    let units_in_plan = the_plan.units.len();
    let units_selected = selected.len();
    let _ = darkmux_flow::record(crawl_record(
        "crawl.mission.started",
        &mission_id,
        Some(&mission_id),
        json!({
            "corpus": manifest.name,
            "units_in_plan": units_in_plan,
            "units_selected": units_selected,
            "est_tokens": est_tokens_total,
            "sources": sources_summary,
        }),
        None,
    ));

    // ── kill file / per-run artifacts ──────────────────────────────────
    let root = manifest.resolved_root();
    let kill_file = root.join("STOP");
    let runs_dir = root.join("runs").join(&mission_id);
    std::fs::create_dir_all(&runs_dir).with_context(|| format!("creating {}", runs_dir.display()))?;
    let ledger_path = runs_dir.join("ledger.jsonl");

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
            corpus_name: manifest.name.clone(),
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
        if let Ok(mut step) = crew::lifecycle::load_step(&mission_id, &phase_id, &step_id) {
            step.status = NodeStatus::Running;
            step.started_ts = Some(now_unix());
            let _ = crew::lifecycle::save_step(&mission_id, &phase_id, &step);
        }

        // (#1959 merge-gate finding 5) Computed BEFORE `crawl.unit.started`
        // is emitted, so that record's `session_id` carries the UNIT's own
        // session — it used to carry the mission id (the same value every
        // OTHER `crawl.*` record for this mission already carries), which
        // made `crawl.unit.started` the one record in this family you
        // couldn't correlate to its matching `crawl.unit.completed` by
        // session id alone.
        let session_id = format!("crawl-{mission_id}-{}", unit.id());

        let started_payload = match unit {
            Unit::Site { sites, .. } => json!({
                "corpus": manifest.name, "unit": unit.id(), "source": source, "sha": sha,
                "rule": rule_ids, "kind": kind, "est_tokens": unit.est_tokens(), "sites": sites.len(),
            }),
            Unit::Read { files, .. } => json!({
                "corpus": manifest.name, "unit": unit.id(), "source": source, "sha": sha,
                "rule": rule_ids, "kind": kind, "est_tokens": unit.est_tokens(), "files": files.len(),
            }),
            Unit::Edge { sites, .. } => json!({
                "corpus": manifest.name, "unit": unit.id(), "source": source, "sha": sha,
                "rule": rule_ids, "kind": kind, "est_tokens": unit.est_tokens(), "sites": sites.len(),
            }),
        };
        let _ = darkmux_flow::record(crawl_record(
            "crawl.unit.started",
            &mission_id,
            Some(&session_id),
            started_payload,
            None,
        ));

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
        };

        let dispatch_outcome = dispatch_fn(opts);

        let (mut result_label, wall_ms, prompt_tok, completion_tok, model, detections) = match &dispatch_outcome {
            Err(_) => ("error".to_string(), 0u64, 0u64, 0u64, None, None),
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
                            obj.insert("corpus".to_string(), json!(manifest.name));
                            obj.insert("unit".to_string(), json!(unit.id()));
                            obj.insert("source".to_string(), json!(source));
                            obj.insert("sha".to_string(), json!(sha));
                            obj.insert("rule".to_string(), json!(rule_ids));
                            obj.insert("session_id".to_string(), json!(session_id));
                            if let Some(m) = &model {
                                obj.insert("model".to_string(), json!(m));
                            }
                        }
                        let line_out = serde_json::to_string(&rec).unwrap_or_default();
                        ledger_buf.push_str(&line_out);
                        ledger_buf.push('\n');
                        let _ = darkmux_flow::record(crawl_record(
                            "crawl.finding",
                            &mission_id,
                            Some(&session_id),
                            rec,
                            model.as_deref(),
                        ));
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
        }

        let mut completed_payload = json!({
            "corpus": manifest.name,
            "unit": unit.id(),
            "source": source,
            "sha": sha,
            "rule": rule_ids,
            "result": result_label,
            "findings": findings_n,
            "prompt_tokens": prompt_tok,
            "completion_tokens": completion_tok,
            "wall_ms": wall_ms,
        });
        if let Some(d) = &detections {
            completed_payload["detections"] = d.clone();
        }
        let _ = darkmux_flow::record(crawl_record(
            "crawl.unit.completed",
            &mission_id,
            Some(&session_id),
            completed_payload,
            model.as_deref(),
        ));

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

#[allow(clippy::too_many_arguments)]
fn print_summary_table(
    mission_id: &str,
    corpus: &str,
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
    println!("{}", style::header(&format!("darkmux mission launch crawl — {corpus} ({mission_id})")));
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
