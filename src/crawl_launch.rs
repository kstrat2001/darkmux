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

fn render_sites(sites: &[Site]) -> String {
    sites
        .iter()
        .map(|s| format!("- {}:{} (read lines {}-{})", s.file, s.line, s.start, s.end))
        .collect::<Vec<_>>()
        .join("\n")
}

fn render_files(files: &[ReadFileEntry]) -> String {
    files
        .iter()
        .map(|f| match f {
            ReadFileEntry::Whole(path) => format!("- {path}"),
            ReadFileEntry::Range { file, start, end } => format!("- {file} (lines {start}-{end})"),
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
const REPORT_FINDING_INSTRUCTIONS: &str = "\nFor each match, call `report_finding` with these five keys exactly: `file`, `line`, `pattern`, `evidence`, `why`. `evidence` must be the source line copied verbatim, and `line` must be where it appears.\n\nWhen you are done, say which files or sites you examined, which you did not get to, and whether you covered the whole scope.\n";

/// Build the dispatch message for one unit. Model-facing (AI-convention
/// terms; the words `unit`/`ledger`/`corpus`/`packet` never appear —
/// darkmux-internal vocabulary a clean-context model can't ground).
fn build_message(rules_by_id: &BTreeMap<String, Rule>, unit: &Unit) -> Result<String> {
    let mut out = String::new();
    match unit {
        Unit::Site { rule, sites, source, .. } => {
            let r = rules_by_id
                .get(rule)
                .ok_or_else(|| anyhow!("crawl launcher: no rule resolved for id `{rule}` (this is a bug — every unit's rule id comes from the same resolved rule set the plan was built from)"))?;
            out.push_str(&pattern_block(r));
            out.push_str(&format!(
                "Your scope is these sites in `/workspace/{source}`. For each, read lines noted below and decide whether the cited line matches the pattern. Sites:\n{}\n",
                render_sites(sites)
            ));
        }
        Unit::Read { rules: rule_ids, files, source, .. } => {
            for rid in rule_ids {
                let r = rules_by_id
                    .get(rid)
                    .ok_or_else(|| anyhow!("crawl launcher: no rule resolved for id `{rid}` (this is a bug)"))?;
                out.push_str(&pattern_block(r));
            }
            out.push_str(&format!(
                "Your scope is these files in `/workspace/{source}`. Read each one in full and apply every pattern above:\n{}\n",
                render_files(files)
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
            let r = rules_by_id
                .get(rule)
                .ok_or_else(|| anyhow!("crawl launcher: no rule resolved for id `{rule}` (this is a bug)"))?;
            out.push_str(&pattern_block(r));
            out.push_str(&format!(
                "Your scope is these import sites in `/workspace/{source}`:\n{}\n\n",
                render_sites(sites)
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

/// Pull `(result, wall_ms, prompt_tokens, completion_tokens, model,
/// detections)` out of a dispatch's `--json` envelope (`res.stdout`).
/// `result` collapses to `"stop"` on a clean finish and `"error"` on
/// anything else (a hard-to-parse envelope, `max_turns`, an escalation
/// variant, or a bare non-zero exit with no envelope at all) — see this
/// function's call site for why the tri-state `stop|error|timeout` this
/// launcher's spec names isn't implemented: there is no reliable
/// `DispatchResult`-level signal today for "the host watchdog hard-killed
/// this container" versus any other non-clean exit.
fn interpret_dispatch_result(res: &DispatchResult) -> (String, u64, u64, u64, Option<String>, Option<Value>) {
    let envelope: Option<Value> =
        if res.stdout.trim().starts_with('{') { serde_json::from_str(&res.stdout).ok() } else { None };
    let result_label = match envelope.as_ref().and_then(|e| e.get("result")).and_then(Value::as_str) {
        Some("stop") => "stop".to_string(),
        Some(_) => "error".to_string(),
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

fn crawl_record(action: &str, mission_id: &str, session_id: Option<&str>, payload: Value) -> darkmux_flow::FlowRecord {
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
        model: None,
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
            }
            loaded_plan
        }
        None => plan::plan(&manifest, &rules_vec, &resolved_sources)
            .with_context(|| format!("planning corpus '{}'", manifest.name))?,
    };

    let (selected, truncated) = select_units(&the_plan, units_filter.as_deref(), limit)?;

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
    let _ = darkmux_flow::record(crawl_record(
        "crawl.mission.started",
        &mission_id,
        Some(&mission_id),
        json!({
            "corpus": manifest.name,
            "units_planned": selected.len(),
            "est_tokens": est_tokens_total,
            "sources": sources_summary,
        }),
    ));

    // ── kill file / per-run artifacts ──────────────────────────────────
    let root = manifest.resolved_root();
    let kill_file = root.join("STOP");
    let runs_dir = root.join("runs").join(&mission_id);
    std::fs::create_dir_all(&runs_dir).with_context(|| format!("creating {}", runs_dir.display()))?;
    let ledger_path = runs_dir.join("ledger.jsonl");

    #[cfg(unix)]
    darkmux_types::interrupt::install();

    let mut stopped_by: &'static str = if truncated { "limit" } else { "done" };
    let mut units_completed = 0usize;
    let mut units_errored = 0usize;
    let mut units_skipped = 0usize;
    let mut findings_total = 0usize;
    let mut prompt_tokens_total: u64 = 0;
    let mut completion_tokens_total: u64 = 0;
    let mut wall_ms_total: u64 = 0;
    let mut per_unit_rows: Vec<Value> = Vec::new();

    for (i, unit) in selected.iter().enumerate() {
        if kill_file.exists() {
            stopped_by = "kill_file";
            units_skipped = selected.len() - i;
            break;
        }
        #[cfg(unix)]
        if darkmux_types::interrupt::is_set() {
            stopped_by = "interrupted";
            units_skipped = selected.len() - i;
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
        let _ = darkmux_flow::record(crawl_record("crawl.unit.started", &mission_id, Some(&mission_id), started_payload));

        let message = build_message(&rules_by_id, unit)?;
        let session_id = format!("crawl-{mission_id}-{}", unit.id());
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

        let (result_label, wall_ms, prompt_tok, completion_tok, model, detections) = match &dispatch_outcome {
            Err(_) => ("error".to_string(), 0u64, 0u64, 0u64, None, None),
            Ok(res) => interpret_dispatch_result(res),
        };

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
                        let _ = darkmux_flow::record(crawl_record("crawl.finding", &mission_id, Some(&session_id), rec));
                    }
                    if !ledger_buf.is_empty() {
                        let _ = append_file(&ledger_path, &ledger_buf);
                    }
                    let _ = std::fs::copy(&findings_path, runs_dir.join(format!("{}.findings.jsonl", unit.id())));
                }
            }
        }

        let is_error = dispatch_outcome.is_err() || result_label == "error";
        if is_error {
            units_errored += 1;
        } else {
            units_completed += 1;
        }
        findings_total += findings_n;
        prompt_tokens_total += prompt_tok;
        completion_tokens_total += completion_tok;
        wall_ms_total += wall_ms;

        if let Ok(mut step) = crew::lifecycle::load_step(&mission_id, &phase_id, &step_id) {
            step.status = if is_error { NodeStatus::Error } else { NodeStatus::Complete };
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
        let _ =
            darkmux_flow::record(crawl_record("crawl.unit.completed", &mission_id, Some(&session_id), completed_payload));

        per_unit_rows.push(json!({
            "unit": unit.id(),
            "source": source,
            "rule": rule_ids,
            "result": result_label,
            "findings": findings_n,
            "prompt_tokens": prompt_tok,
            "completion_tokens": completion_tok,
            "wall_ms": wall_ms,
        }));

        #[cfg(unix)]
        if darkmux_types::interrupt::is_set() {
            stopped_by = "interrupted";
            units_skipped = selected.len() - (i + 1);
            break;
        }
    }

    // ── finalize ──────────────────────────────────────────────────────
    let _ = crew::lifecycle::phase_complete(&phase_id);
    let _ = crew::lifecycle::mission_close_with_reasoning(&mission_id, Some(&format!("crawl stopped_by={stopped_by}")));

    let total_tokens = prompt_tokens_total + completion_tokens_total;
    let wall_hours = (wall_ms_total as f64) / 1000.0 / 3600.0;
    let tokens_per_hour = if wall_hours > 0.0 { (total_tokens as f64 / wall_hours).round() as u64 } else { 0 };

    let summary = json!({
        "mission_id": mission_id,
        "corpus": manifest.name,
        "units_completed": units_completed,
        "units_errored": units_errored,
        "units_skipped": units_skipped,
        "findings": findings_total,
        "prompt_tokens": prompt_tokens_total,
        "completion_tokens": completion_tokens_total,
        "wall_ms": wall_ms_total,
        "tokens_per_hour": tokens_per_hour,
        "stopped_by": stopped_by,
    });
    let _ = darkmux_flow::record(crawl_record("crawl.mission.completed", &mission_id, Some(&mission_id), summary.clone()));

    let mut envelope = summary.clone();
    if let Some(obj) = envelope.as_object_mut() {
        obj.insert("units".to_string(), json!(per_unit_rows));
    }
    let envelope_path = runs_dir.join("envelope.json");
    std::fs::write(&envelope_path, serde_json::to_string_pretty(&envelope)?)
        .with_context(|| format!("writing {}", envelope_path.display()))?;

    print_summary_table(&mission_id, &manifest.name, units_completed, units_errored, units_skipped, findings_total, total_tokens, wall_ms_total, stopped_by, &ledger_path);

    Ok(match stopped_by {
        "kill_file" => 3,
        "interrupted" => 130,
        _ => 0,
    })
}

#[allow(clippy::too_many_arguments)]
fn print_summary_table(
    mission_id: &str,
    corpus: &str,
    units_completed: usize,
    units_errored: usize,
    units_skipped: usize,
    findings: usize,
    total_tokens: u64,
    wall_ms: u64,
    stopped_by: &str,
    ledger_path: &Path,
) {
    println!("{}", style::header(&format!("darkmux mission launch crawl — {corpus} ({mission_id})")));
    println!(
        "  units: {units_completed} completed, {units_errored} errored, {units_skipped} skipped"
    );
    println!("  findings: {findings}");
    println!("  tokens: {total_tokens}   wall: {wall_ms}ms");
    println!("  stopped_by: {stopped_by}");
    println!("  ledger: {}", ledger_path.display());
}

#[cfg(test)]
#[path = "crawl_launch_tests.rs"]
mod tests;
