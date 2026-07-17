//! `darkmux mission launch <config-id>` — mints a mission INSTANCE from a
//! named mission CONFIG (#1284 Packet 4a, the instance-model collapse).
//!
//! Per the epic's locked arc design (#1284): "config-launched becomes the
//! only instance-creation path... instance = resolved-config snapshot +
//! runtime state." This module is that path. It:
//!
//!   1. Resolves `<config-id>` through `mission_config::load` (user →
//!      on-disk → embedded) and validates it loud (contract 7 — semantic
//!      validation is a separate, explicit CONSUMPTION-time pass, never
//!      folded into the lenient-on-read `load`).
//!   2. Collects the config's declared `inputs` from `--input <file.json>`
//!      / `--param key=value` (params win), bailing loud with a
//!      copy-pasteable example when a required input is missing.
//!   3. Mints a mission instance: `mission.json`, one `phases/<id>.json`
//!      per declared phase, AND a `config-snapshot.json` — the
//!      fully-resolved config frozen alongside the instance so a later
//!      edit/delete of the source config never orphans a running
//!      instance's own record of what it ran (mirrors the review
//!      pipeline's crew-staffing-snapshot precedent).
//!   4. Interprets the graph (`mission_config::interpret`) and, when the
//!      graph is one this packet's launcher knows how to EXECUTE, runs it
//!      through the real scheduler and finalizes via `MissionEnvelope`.
//!
//! **Scope boundary (read before extending).** This packet wires exactly
//! ONE mission type all the way through to real dispatch: `coder-phase`,
//! reusing `mission_run.rs`'s own `MissionWorktreeStepKind` /
//! `MissionCoderStepKind` / `MissionVerifyStepKind` Tier 3 kinds verbatim
//! (elevated to `pub(crate)` for this module — see their doc comments) so
//! the `mission.worktree`/`mission.coder`/`mission.verify` flow-record
//! shape and `darkmux-serve`'s `/diff` contract stay byte-identical
//! whether a mission was minted by `mission run` or `mission launch`.
//! `darkmux mission run` is UNCHANGED and still the primary path for an
//! operator working an EXISTING hand-authored (or `mission propose`-drafted
//! pre-Packet-4a) mission one phase at a time; `mission launch` is the new,
//! ADDITIONAL config-driven path, proven out here.
//!
//! **Gate semantics (#1284 review round 1, must-fix 1).** The coder-phase
//! path deliberately does NOT finalize the mission. `mission run` stops at
//! the "gate — awaiting frontier/operator sign-off" banner with the phase
//! left `Running`, so the operator adjudicates and `mission ship` finishes
//! the loop — and `mission launch coder-phase` mirrors that outcome map
//! EXACTLY (same gate banners, same exit codes, same Running end state;
//! see [`coder_phase_gate_outcome`]). Auto-closing past that gate was an
//! operator-sovereignty violation (#44) at precisely the decision point
//! `mission run` reserves, and it broke `mission ship` (which refuses a
//! terminal-Complete phase). Generic `finalize_mission` stays reserved for
//! graphs with NO gate semantics (a Tier-1-only graph); a freeform config
//! mints + starts and finalizes nothing.
//!
//! `review` (the 3-phase PR-review config, #1284 Packet 4b, the clean verb
//! break that retired `darkmux pr-review run`) is executable through this
//! verb too, but via a DEDICATED launcher (`crate::mission_launch_review`)
//! rather than steps 2-4 above: `launch` branches to it as early as
//! possible (right after config load + validation, before this module's own
//! `--input`/`--param` collection or its generic header banner — review's
//! rendered payload is a stdout CONTRACT the CI workflow parses, so nothing
//! decorative may precede it). `review.*` Tier 3 kinds need crew-staffing
//! resolution (`crew_staffing`, `judge_concurrency` — see `templates/
//! builtin/mission-configs/review.json`'s own `inputs` doc) that
//! `crates/darkmux-lab/src/lab/review.rs::build_review_graph` already knows
//! how to do — `mission_launch_review::launch` is a NEW CALLER of that
//! SAME driver (the former `pr_review.rs::run_dispatch`), not a second
//! graph builder; see that module's doc for why review does not collapse
//! into steps 2-4's generic `mission_config::interpret` + `crew::
//! scheduler::run_step_graph` path (an audited non-collapse per
//! `CLAUDE.md`'s StepKind tiering section). A config whose graph
//! references a step kind THIS generic path can't construct at all (step
//! 4's `executable` check below, still reachable by any non-`review`
//! config) gets a GUARDED punt: the instance is still minted (so its
//! Task/Step records show the intended graph shape for inspection), but
//! nothing is dispatched and `launch` returns exit code `4`.

use crate::crew;
use crate::fleet;
use crate::flow;
use crate::mission_run;
use anyhow::{anyhow, bail, Context, Result};
use crew::mission_config::{self, FindingSeverity, LaunchParams, MissionConfig, TaskOverride};
use crew::types::{Mission, MissionStatus, NodeStatus, Phase, PhaseStatus};
use darkmux_types::style;
use std::collections::BTreeMap;
use std::path::Path;
use std::sync::{Arc, Mutex};

/// The three Tier 3 step kinds `mission_run.rs` defines for `coder-phase`'s
/// graph (#1352) — the only Tier 3 kinds this launcher knows how to
/// construct in Packet 4a. See the module doc's scope boundary.
const CODER_PHASE_TIER3_KINDS: &[&str] = &["mission.worktree", "mission.coder", "mission.verify"];

/// The six Tier 3 step kinds `crates/darkmux-lab/src/lab/review.rs` defines
/// for `review`'s graph (#1352) — wired through as of Packet 4b, but via a
/// DEDICATED launcher ([`crate::mission_launch_review::launch`]), not this
/// module's generic `mission_config::interpret` + `crew::scheduler::
/// run_step_graph` path. `build_review_graph`/`run_review_graph` already
/// carry real, working, tested cross-step behavior (a shared remote-token
/// bucket, host telemetry sampling, post-run envelope merges) a generic
/// collapse would either lose or have to re-derive — an audited non-collapse
/// per `CLAUDE.md`'s StepKind tiering section ("a collapse that changes
/// observable behavior isn't a tiering fix, it's a feature change wearing a
/// tiering fix's clothes"). Named here purely so `known_kinds`'s
/// doctor-style `validate()` pass never warns "unknown kind" on review's own
/// document.
const REVIEW_TIER3_KINDS: &[&str] =
    &["review.bundle", "review.probe", "review.dedup", "review.judge", "review.verify", "review.synthesis"];

/// `darkmux mission launch <config-id>` entry point. Returns the process
/// exit code — the coder-phase rows mirror `mission_run::run`'s own exit
/// map exactly (#1284 review round 1, must-fix 1):
///   `0` — freeform mint; or coder ran and QA came back clean/flags-only
///         (gate banner printed, phase left Running for `mission ship`);
///         or a gate-less generic graph finished Clean/Degraded.
///   `1` — coder dispatch error (phase stays Running, worktree kept for
///         inspection); or a gate-less generic graph ended Error.
///   `2` — QA found blocker(s) — resolve before shipping (phase Running).
///   `3` — QA could not run — manual review required (phase Running).
///   `4` — instance minted but NOT executed: the graph references step
///         kind(s) this launcher can't construct yet (Packet 4b).
///
/// `timeout_seconds` is the clap `--timeout` value, `None` when the
/// operator omitted it — resolved PER CONFIG (#1284 Packet 4b review gate,
/// must-fix 1): the generic/coder-phase path below resolves `None` -> 600
/// (`mission run`'s own default); the `review` branch passes the `Option`
/// through so `mission_launch_review::launch` can resolve `None` -> 3600
/// (the retired `pr-review run`'s per-call default — a 600s ceiling would
/// silently degrade any review whose judge pass runs long).
pub fn launch(
    config_id: &str,
    input_file: Option<&Path>,
    params: &[String],
    timeout_seconds: Option<u32>,
) -> Result<i32> {
    // (#1311, C7 of the #1284 Packet 4b review gate) The dependency-free
    // liveness floor's FIRST marker — before `mission_config::load` below,
    // which reads the user-tier config dir (filesystem I/O that precedes
    // any other observable output; a hang there is exactly the
    // pre-flow-init black-box class the floor exists for).
    darkmux_types::dispatch_liveness::liveness("process-start");
    fleet::validate_identifier("config_id", config_id)?;

    let loaded = mission_config::load(config_id).with_context(|| {
        format!(
            "loading mission config \"{config_id}\" — note: a user-tier copy \
             (~/.darkmux/mission-configs/{config_id}.json) or an on-disk template overrides \
             an embedded built-in; the failing file is named above if one was found"
        )
    })?;
    let config = &loaded.config;

    // (contract 7) Semantic validation is a SEPARATE, explicit pass — this
    // IS the consumption point. `known_step_kinds` is everything this
    // launcher can actually run today (Tier 1 builtins + the coder-phase
    // Tier 3 set); anything else warns rather than errors (#1284 Packet 1's
    // own rule — a step kind this call site doesn't recognize isn't
    // necessarily wrong, just not yet reachable through THIS launcher).
    let tier1_ids = crew::step_kinds::StepKindRegistry::with_builtins().ids();
    let mut known_kinds: Vec<&str> = tier1_ids.iter().map(String::as_str).collect();
    known_kinds.extend(CODER_PHASE_TIER3_KINDS.iter().copied());
    known_kinds.extend(REVIEW_TIER3_KINDS.iter().copied());
    let findings = config.validate(&known_kinds);
    let errors: Vec<_> = findings.iter().filter(|f| f.severity == FindingSeverity::Error).collect();
    if !errors.is_empty() {
        let msg = errors.iter().map(|f| f.to_string()).collect::<Vec<_>>().join("\n");
        bail!("mission launch: config \"{config_id}\" failed validation:\n{msg}");
    }
    for f in findings.iter().filter(|f| f.severity == FindingSeverity::Warning) {
        eprintln!("{}", style::warn(&f.to_string()));
    }

    // (#1284 Packet 4b) `review` gets a DEDICATED launcher rather than
    // falling through the generic interpret/scheduler path below — see
    // `REVIEW_TIER3_KINDS`'s doc for why. Review has no operator sign-off
    // gate (unlike coder-phase): its envelope finalizes generically via
    // `crew::envelope::finalize_mission` inside that module, and this
    // function's own exit-code/gate machinery never runs for it. Branches
    // BEFORE the generic header banner below (never AFTER): review's
    // rendered `{mode, review, comment}` JSON is a stdout CONTRACT the CI
    // workflow parses byte-for-byte on `--param emit=-`, so nothing
    // decorative may land on stdout ahead of it — `mission_launch_review::
    // launch` prints its own (stderr-only) diagnostics instead.
    if config.id == "review" {
        return crate::mission_launch_review::launch(config, input_file, params, timeout_seconds);
    }

    println!(
        "{}",
        style::header(&format!(
            "▶ mission launch — {} ({} tier)",
            config_id,
            loaded.source
        ))
    );

    let collected = collect_inputs(input_file, params)?;
    let missing = missing_required_inputs(config, &collected);
    if !missing.is_empty() {
        bail!("{}", missing_inputs_message(config, &missing));
    }

    // (#1284 review round 1, consider 2) A supplied input the config never
    // declared still shapes the derived instance id below — so a TYPO'D key
    // wouldn't just be ignored, it would silently derive a DIFFERENT
    // instance. Warn loudly; don't block (a config author may deliberately
    // accept undeclared pass-through values).
    for key in collected.keys() {
        if !config.inputs.iter().any(|i| i.name == *key) {
            eprintln!(
                "{}",
                style::warn(&format!(
                    "mission launch: input `{key}` is not declared by config \"{config_id}\"'s \
                     inputs — it still shapes the derived instance id, so a typo here would \
                     silently launch a different instance"
                ))
            );
        }
    }

    // (#1284 review round 1, consider 11) A config whose graph uses the
    // coder-phase step kinds needs workdir/branch/base to EXECUTE — check
    // that BEFORE minting anything, so a user-authored config that uses
    // `mission.*` kinds without declaring those inputs (the built-in
    // declares them, so the required-inputs gate above catches it first)
    // doesn't litter a half-launched instance on disk.
    if config_uses_coder_phase_kinds(config) {
        precheck_coder_phase_inputs(config, &collected)?;
    }

    // Instance id: the bare config id when the operator supplied no inputs
    // (a constant hash suffix would disambiguate nothing — #1284 review
    // round 1, must-fix 3), else `<config-id>-<disambiguator>` derived from
    // the OPERATOR-SUPPLIED inputs (never `mission_id` itself, which the
    // launcher supplies below — hashing it would be circular). Same params
    // → same instance id → the reuse/reopen path below fires (idempotent
    // relaunch); different params → a distinct instance. No `--mission-id`
    // flag needed.
    let mission_id = derive_mission_id(config_id, &collected)?;
    let mut collected = collected;
    if config.inputs.iter().any(|i| i.name == "mission_id") {
        collected.insert("mission_id".to_string(), serde_json::Value::String(mission_id.clone()));
    }

    let (real_phase_ids, reused) = ensure_mission_and_phases(&mission_id, config)?;
    if reused {
        println!(
            "  {}",
            style::dim(&format!(
                "reusing existing instance `{mission_id}` (same config + inputs as a prior launch)"
            ))
        );
    }

    // config-snapshot.json — ALWAYS written (fresh mint or relaunch
    // overwrite), regardless of whether the graph turns out executable.
    crew::lifecycle::save_config_snapshot(&mission_id, config)
        .context("persisting config-snapshot.json")?;

    let params = build_launch_params(config, &real_phase_ids, &collected);
    let (tasks, mut steps, interpret_warnings) =
        mission_config::interpret(config, &params).context("interpreting mission config graph")?;
    // (#1418) An absent `expand.over` key (typo'd collection name in a
    // user-tier config override, most likely) used to expand silently to
    // zero real copies; now named here so the operator sees it instead of
    // a mission that mints with fewer tasks than the config implies.
    for w in &interpret_warnings {
        eprintln!("{}", style::dim(&format!("mission launch: {w}")));
    }

    for task in &tasks {
        if let Err(e) = crew::lifecycle::save_task(&mission_id, task) {
            eprintln!("{}", style::dim(&format!("mission launch: task persist warning: {e:#}")));
        }
    }

    if tasks.is_empty() {
        // Freeform/manual mission (every phase has zero tasks) — mint +
        // start, leave every phase transition operator-driven.
        let verb = if reused { "reopened" } else { "minted" };
        println!(
            "{}",
            style::success(&format!(
                "✓ mission `{mission_id}` {verb} from config \"{config_id}\" — {} freeform phase(s)",
                config.phases.len()
            ))
        );
        println!("  {}", style::dim("no automated graph; drive phases by hand:"));
        for phase in &config.phases {
            println!(
                "    darkmux phase start {}   {}",
                real_phase_ids[&phase.id],
                style::dim(&format!("— {}", phase.description.as_deref().unwrap_or(&phase.id)))
            );
        }
        return Ok(0);
    }

    let all_known: Vec<&str> = tier1_ids.iter().map(String::as_str).chain(CODER_PHASE_TIER3_KINDS.iter().copied()).collect();
    let executable = steps.values().all(|s| all_known.contains(&s.kind.as_str()));
    if !executable {
        for task in &tasks {
            for step_id in &task.step_ids {
                if let Some(step) = steps.get(step_id) {
                    let _ = crew::lifecycle::save_step(&mission_id, &task.phase_id, step);
                }
            }
        }
        let unknown: Vec<&str> = steps
            .values()
            .map(|s| s.kind.as_str())
            .filter(|k| !all_known.contains(k))
            .collect();
        println!(
            "{}",
            style::warn(&format!(
                "⚠ mission `{mission_id}` minted from config \"{config_id}\", but its graph \
                 references step kind(s) this launcher can't construct yet: {}. Nothing was \
                 dispatched — Task/Step records show the intended shape for inspection. This \
                 config needs Packet 4b's remaining launcher plumbing before `mission launch` \
                 can run it end to end (exit code 4).",
                unknown.join(", ")
            ))
        );
        return Ok(4);
    }

    // Real execution — build the registry (Tier 1 always, plus the
    // coder-phase Tier 3 kinds when the graph actually uses them), start
    // every real phase that has tasks, run the scheduler.
    //
    // Generic/coder-phase timeout default: `None` -> 600, matching
    // `mission run`'s own default (see `launch`'s doc — `review` resolves
    // its own 3600 default in `mission_launch_review::launch` instead).
    let timeout_seconds = timeout_seconds.unwrap_or(600);
    let registry = crew::step_kinds::StepKindRegistry::with_builtins();
    let uses_coder_phase_kinds = steps.values().any(|s| CODER_PHASE_TIER3_KINDS.contains(&s.kind.as_str()));
    let coder_handles = if uses_coder_phase_kinds {
        Some(register_coder_phase_kinds(
            &registry,
            &mission_id,
            config,
            &real_phase_ids,
            &collected,
            timeout_seconds,
        )?)
    } else {
        None
    };

    // (#1400) Preflight, READ-ONLY pass: name any phase that's already
    // terminal-Complete from a prior finalized run — informational only,
    // no `phase_start` call here. Phases don't start eagerly at mint
    // anymore (that was the bug — every phase pulsed "running" from second
    // zero regardless of whether the scheduler had reached it); each
    // phase's OWN `phase_start` fires lazily, inside the `persist` closure
    // below, the FIRST time one of its steps actually flips `Running`.
    for phase in &config.phases {
        let real_id = &real_phase_ids[&phase.id];
        if !tasks.iter().any(|t| &t.phase_id == real_id) {
            continue;
        }
        if let Ok(p) = load_phase_for_brief(&mission_id, real_id) {
            if p.status == PhaseStatus::Complete {
                eprintln!(
                    "{}",
                    style::dim(&format!(
                        "mission launch: phase `{real_id}` is already Complete (terminal) from a \
                         prior finalized run — steps will still execute, but the phase's own \
                         status cannot move; abandon-and-recreate is not automated (operator \
                         sovereignty #44)."
                    ))
                );
            }
        }
    }

    let tasks_by_id: BTreeMap<String, crew::types::Task> =
        tasks.iter().map(|t| (t.id.clone(), t.clone())).collect();
    let facts = crew::step_kinds::Facts::default();
    let est = crew::step_kinds::FixedEstimator::default();
    // (#1400) Tracks which phases this dispatch has already lazy-started —
    // see `lazy_start_phase_for_step`'s doc.
    let mut started_phases: std::collections::HashSet<String> = std::collections::HashSet::new();
    // (#1397) `persist` durably saves each step at ITS OWN transition
    // (Running at dispatch, Complete/Error at completion), not just at the
    // end of the whole run — see `run_step_graph`'s own doc. The phase id
    // isn't on `Step` itself, so it's resolved per-call from the owning
    // Task via `tasks_by_id` (borrowed here, alongside the scheduler's own
    // immutable borrow of the same map — both read-only, no conflict). The
    // bulk save loop right after this call stays in place as a cheap,
    // idempotent final reconcile.
    let graph_result = crew::scheduler::run_step_graph(
        &mut steps,
        &tasks_by_id,
        &registry,
        &facts,
        &est,
        1,
        &crew::concurrent_dispatch::lms_host_factory,
        &mut |record| {
            let _ = flow::record(record);
        },
        &mut |step| {
            let phase_id = tasks_by_id
                .get(&step.task_id)
                .map(|t| t.phase_id.as_str())
                .unwrap_or_default();
            lazy_start_phase_for_step(&mission_id, phase_id, step.status, &mut started_phases);
            if let Err(e) = crew::lifecycle::save_step(&mission_id, phase_id, step) {
                eprintln!(
                    "{}",
                    style::dim(&format!("mission launch: step persist warning (transition): {e:#}"))
                );
            }
        },
    );

    // (#1406, F4) A scheduler-level `Err` mid-run would otherwise `?`-return
    // here with NO finalize, stranding the mission Active with `Running`
    // phases + steps forever. Reconcile the stranded steps and drive the
    // mission to a terminal Error status BEFORE propagating the failure.
    // The failure is still surfaced to the caller (loud, non-zero exit); the
    // mission board just no longer lies about a dead run being active.
    if let Err(e) = graph_result {
        reconcile_and_finalize_on_error(&mission_id, config, &real_phase_ids, &tasks, &mut steps, &e);
        return Err(e);
    }

    for task in &tasks {
        for step_id in &task.step_ids {
            if let Some(step) = steps.get(step_id) {
                if let Err(e) = crew::lifecycle::save_step(&mission_id, &task.phase_id, step) {
                    eprintln!("{}", style::dim(&format!("mission launch: step persist warning: {e:#}")));
                }
            }
        }
    }

    // (#1284 review round 1, must-fix 1) A coder-phase graph has GATE
    // semantics: stop at the operator sign-off gate exactly as `mission
    // run` does — phase stays Running, mission stays Active, NO
    // finalize_mission, and the exit code mirrors `mission_run::run`'s own
    // outcome map. `mission ship` finishes the loop from here.
    if let Some(handles) = &coder_handles {
        return coder_phase_gate_outcome(&mission_id, handles, &steps);
    }

    // Gate-less generic graph (Tier-1-only kinds) — the standard
    // MissionEnvelope finalization applies: every run reaches a terminal
    // phase/mission status (Packet 2's own doctrine for gate-free work).
    let envelope = build_envelope(&mission_id, config, &real_phase_ids, &tasks, &steps);
    let status = envelope.status;
    crew::envelope::finalize_mission(&envelope);

    print_run_summary(&mission_id, &steps);

    use crew::envelope::MissionOutcomeStatus;
    Ok(match status {
        MissionOutcomeStatus::Clean | MissionOutcomeStatus::Degraded => 0,
        MissionOutcomeStatus::Degenerate | MissionOutcomeStatus::Error => 1,
    })
}

/// Parse `--input <file.json>` (a flat object) and `--param key=value`
/// (repeatable; wins over the file) into one collected-inputs map.
pub(crate) fn collect_inputs(
    input_file: Option<&Path>,
    params: &[String],
) -> Result<BTreeMap<String, serde_json::Value>> {
    let mut collected: BTreeMap<String, serde_json::Value> = BTreeMap::new();
    if let Some(path) = input_file {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("reading --input file {}", path.display()))?;
        let value: serde_json::Value = serde_json::from_str(&text)
            .with_context(|| format!("parsing --input file {} as JSON", path.display()))?;
        let obj = value.as_object().ok_or_else(|| {
            anyhow!(
                "--input file {} must contain a JSON object mapping input name -> value",
                path.display()
            )
        })?;
        for (k, v) in obj {
            collected.insert(k.clone(), v.clone());
        }
    }
    for raw in params {
        let (k, v) = raw
            .split_once('=')
            .ok_or_else(|| anyhow!("--param `{raw}` must be in `key=value` form"))?;
        collected.insert(k.to_string(), serde_json::Value::String(v.to_string()));
    }
    Ok(collected)
}

/// Declared inputs (per [`MissionConfig::inputs`]) still missing from
/// `collected`, excluding `mission_id` (auto-supplied by the launcher — see
/// `launch`'s doc). Optional inputs (`required == Some(false)`) never
/// count as missing.
fn missing_required_inputs<'a>(
    config: &'a MissionConfig,
    collected: &BTreeMap<String, serde_json::Value>,
) -> Vec<&'a mission_config::MissionInput> {
    config
        .inputs
        .iter()
        .filter(|i| i.name != "mission_id")
        .filter(|i| i.required != Some(false))
        .filter(|i| !collected.contains_key(&i.name))
        .collect()
}

fn missing_inputs_message(config: &MissionConfig, missing: &[&mission_config::MissionInput]) -> String {
    let mut msg = format!("mission launch: config \"{}\" is missing required input(s):\n", config.id);
    for i in missing {
        msg.push_str(&format!("  - {}", i.name));
        if let Some(d) = &i.description {
            msg.push_str(&format!(": {d}"));
        }
        msg.push('\n');
    }
    msg.push_str("\nExample --input file:\n");
    let mut obj = serde_json::Map::new();
    for i in &config.inputs {
        if i.name == "mission_id" {
            continue; // launcher-supplied — never asked of the operator
        }
        obj.insert(i.name.clone(), serde_json::Value::String(format!("<{}>", i.name)));
    }
    msg.push_str(&serde_json::to_string_pretty(&serde_json::Value::Object(obj)).unwrap_or_default());
    msg.push_str("\n\nOr pass each as --param:\n  ");
    msg.push_str(
        &config
            .inputs
            .iter()
            .filter(|i| i.name != "mission_id")
            .map(|i| format!("--param {}=<{}>", i.name, i.name))
            .collect::<Vec<_>>()
            .join(" "),
    );
    msg
}

/// The instance id for one launch. Zero operator-supplied inputs → the
/// BARE config id (#1284 review round 1, must-fix 3: hashing an empty map
/// produced the same constant suffix for EVERY zero-input launch of every
/// config — a disambiguator that disambiguated nothing and made the guide's
/// bare-id follow-up commands fail). With inputs →
/// `<config-id>-<10-hex-char digest>` over the CANONICAL (BTreeMap-sorted)
/// JSON of `collected`, so the SAME inputs always derive the SAME instance
/// id (idempotent relaunch) while different inputs derive a distinct one (a
/// genuinely new instance). No CLI flag needed — see the module doc.
pub(crate) fn derive_mission_id(config_id: &str, collected: &BTreeMap<String, serde_json::Value>) -> Result<String> {
    let id = if collected.is_empty() {
        config_id.to_string()
    } else {
        let canon = serde_json::to_string(collected).context("serializing collected inputs for id derivation")?;
        let digest = blake3::hash(canon.as_bytes());
        let hex = digest.to_hex();
        let short = &hex.as_str()[..10];
        format!("{config_id}-{short}")
    };
    fleet::validate_identifier("mission_id", &id)?;
    Ok(id)
}

fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// One Phase JSON literal, shared by the fresh-mint loop and the
/// reuse-path missing-phase backfill (#1284 review round 1, consider 5).
/// `display_name` (#1398) is `PhaseConfig::display_name` verbatim — `None`
/// on a config that doesn't set one, which every renderer falls back to
/// `id` for (never `description`).
fn new_planned_phase(
    mission_id: &str,
    real_id: &str,
    description: Option<&str>,
    display_name: Option<&str>,
    now: u64,
) -> Phase {
    Phase {
        id: real_id.to_string(),
        mission_id: mission_id.to_string(),
        description: description.unwrap_or_default().to_string(),
        display_name: display_name.map(String::from),
        status: PhaseStatus::Planned,
        created_ts: now,
        started_ts: None,
        completed_ts: None,
        abandoned_ts: None,
        task_ids: Vec::new(),
    }
}

/// Mint (or, on relaunch, reopen-if-terminal reuse) the Mission + one Phase
/// per declared phase. Mirrors `src/pr_review.rs::build_mission_for_review`'s
/// exact reuse pattern (#1372): a fresh mission is created Active with every
/// phase Planned and started via the real `mission_start_with_reasoning`
/// lifecycle verb (so the standard "mission start" flow record lands); an
/// EXISTING mission (same derived id → same prior launch) is reopened if the
/// prior run left it Closed, any Abandoned phase is restarted, and any phase
/// the config declares that the old instance is MISSING (the config grew a
/// phase since the prior launch — #1284 review round 1, consider 5) is
/// minted Planned and appended to `phase_ids` — never re-created from
/// scratch, so a relaunch's Task/Step overwrites land on the SAME instance
/// rather than silently forking a duplicate.
///
/// The fresh-mint path hydrates `Mission.source_input`/`Mission.ticket`
/// from the config's `extras` (#1284 review round 1, must-fix 2) — that's
/// where `mission propose` preserves the operator's verbatim words (#815)
/// and ticket id (#816), and dropping them silently broke `coder_brief`'s
/// source-input injection plus the conventions' `{ticket}` templates.
///
/// Returns the doc phase id → real (composed) phase id map every subsequent
/// step needs, plus whether an EXISTING instance was reused.
pub(crate) fn ensure_mission_and_phases(
    mission_id: &str,
    config: &MissionConfig,
) -> Result<(BTreeMap<String, String>, bool)> {
    ensure_mission_and_phases_with_provenance(mission_id, config, None, None)
}

/// Pure, no-I/O derivation of the doc phase id → real (composed) phase id
/// map (`<mission_id>-<doc_id>`). Extracted from
/// [`ensure_mission_and_phases_with_provenance`] (#1417) so a caller can
/// compute the SAME map — and validate/consume it — before minting the
/// Mission the map would otherwise only be available after. Both this
/// function and the mint below derive it identically from `mission_id` +
/// `config.phases`, so precomputing here never drifts from what the mint
/// itself would produce.
pub(crate) fn derive_phase_ids(mission_id: &str, config: &MissionConfig) -> BTreeMap<String, String> {
    config.phases.iter().map(|p| (p.id.clone(), format!("{mission_id}-{}", p.id))).collect()
}

/// [`ensure_mission_and_phases`] with per-launcher PROVENANCE overrides
/// (#1284 Packet 4b review gate, must-fix 2). A dedicated launcher whose
/// instances are per-case (the review launcher: N CI reviews of N PRs)
/// passes a case-bearing `description` ("PR review — owner/repo@sha (crew
/// `x`)") and a case-bearing `reopen_reasoning` ("review re-run for case
/// ...") so the mission board / viewer can tell the instances apart —
/// falling back to the generic config-derived description and "relaunch of
/// config `<id>`" reasoning when `None` (the generic `launch` path).
pub(crate) fn ensure_mission_and_phases_with_provenance(
    mission_id: &str,
    config: &MissionConfig,
    description: Option<&str>,
    reopen_reasoning: Option<&str>,
) -> Result<(BTreeMap<String, String>, bool)> {
    let real_phase_ids: BTreeMap<String, String> = derive_phase_ids(mission_id, config);

    let mission_path = crew::lifecycle::mission_path(mission_id);
    if mission_path.exists() {
        if let Ok(text) = std::fs::read_to_string(&mission_path) {
            if let Ok(mission) = serde_json::from_str::<Mission>(&text) {
                if mission.status == MissionStatus::Closed {
                    let default_reasoning = format!("relaunch of config `{}`", config.id);
                    crew::lifecycle::mission_reopen_with_reasoning(
                        mission_id,
                        Some(reopen_reasoning.unwrap_or(&default_reasoning)),
                    )?;
                }
            }
        }
        // Re-read AFTER the possible reopen so the phase_ids maintenance
        // below never writes a stale (pre-reopen) status back to disk.
        let mut mission_doc: Option<Mission> = std::fs::read_to_string(&mission_path)
            .ok()
            .and_then(|t| serde_json::from_str(&t).ok());
        let mut phase_ids_dirty = false;
        let now = now_unix();
        for phase in &config.phases {
            let real_id = &real_phase_ids[&phase.id];
            let phase_path = crew::lifecycle::phase_path(mission_id, real_id);
            if phase_path.is_file() {
                // (#1400) An `Abandoned` phase on reuse is NOT restarted
                // here anymore — that used to eagerly flip it (and, on a
                // multi-phase config, every OTHER abandoned phase) straight
                // to `Running` before the scheduler dispatched anything.
                // `lazy_start_phase_for_step` already treats `Abandoned`
                // as startable (mirrors `phase_start`'s own state machine —
                // `Abandoned -> Running` clears `abandoned_ts`, same as a
                // fresh restart), so the SAME persist-hook path that starts
                // a `Planned` phase on reach also restarts an `Abandoned`
                // one — preserving #1372's "restarts only what reruns"
                // semantics while fixing the "all phases pulse at once"
                // symptom for the reopen path too.
            } else {
                // (consider 5) The config declares a phase the old instance
                // doesn't have — mint it before executing, and register it
                // in phase_ids so it isn't a dangling file.
                let p = new_planned_phase(
                    mission_id,
                    real_id,
                    phase.description.as_deref(),
                    phase.display_name.as_deref(),
                    now,
                );
                crew::lifecycle::save_phase(&p)
                    .with_context(|| format!("minting missing phase {real_id} on reuse"))?;
                if let Some(m) = &mut mission_doc {
                    if !m.phase_ids.contains(real_id) {
                        m.phase_ids.push(real_id.clone());
                        phase_ids_dirty = true;
                    }
                }
            }
        }
        if phase_ids_dirty {
            if let Some(m) = &mission_doc {
                let _ = crew::lifecycle::save_mission(m);
            }
        }
        return Ok((real_phase_ids, true));
    }

    let now = now_unix();
    let mission = Mission {
        id: mission_id.to_string(),
        description: description
            .map(String::from)
            .or_else(|| config.description.clone())
            .unwrap_or_else(|| config.name.clone()),
        status: MissionStatus::Active,
        phase_ids: config.phases.iter().map(|p| real_phase_ids[&p.id].clone()).collect(),
        created_ts: now,
        started_ts: None,
        closed_ts: None,
        paused_ts: None,
        // (must-fix 2) Hydrate from the config's extras overflow — where
        // `mission propose` preserves the operator's verbatim words (#815)
        // and ticket id (#816). Absent keys stay None, same as before.
        source_input: config
            .extras
            .get("source_input")
            .and_then(|v| v.as_str())
            .map(String::from),
        ticket: config.extras.get("ticket").and_then(|v| v.as_str()).map(String::from),
    };
    crew::lifecycle::save_mission(&mission).context("persisting mission.json")?;

    for phase in &config.phases {
        let real_id = &real_phase_ids[&phase.id];
        let p = new_planned_phase(
            mission_id,
            real_id,
            phase.description.as_deref(),
            phase.display_name.as_deref(),
            now,
        );
        crew::lifecycle::save_phase(&p).with_context(|| format!("persisting phase {real_id}"))?;
    }

    crew::lifecycle::mission_start_with_reasoning(
        mission_id,
        Some(&format!("launched from config `{}`", config.id)),
    )
    .context("starting the newly-minted mission")?;

    Ok((real_phase_ids, false))
}

/// Build the [`LaunchParams`] `mission_config::interpret` needs: every
/// phase's composed real id (generic, always safe), plus — ONLY for the
/// `coder-phase` task ids `default_phase_graph` itself already hardcodes
/// (`build-coder`/`build-verify`, see that function's own doc) — the
/// role/workdir/image/description overrides `collected` supplies. A config
/// that isn't `coder-phase`-shaped gets no task_overrides here (pure
/// pass-through of its own document defaults); Packet 4b is where a
/// genuinely generic per-config override mapping would need to be invented,
/// per the module doc's scope boundary.
fn build_launch_params(
    config: &MissionConfig,
    real_phase_ids: &BTreeMap<String, String>,
    collected: &BTreeMap<String, serde_json::Value>,
) -> LaunchParams {
    let mut task_overrides = BTreeMap::new();
    if config.id == "coder-phase" {
        let role = collected.get("role").and_then(|v| v.as_str());
        let image = collected.get("image").and_then(|v| v.as_str());
        let workdir = collected.get("workdir").and_then(|v| v.as_str()).map(std::path::PathBuf::from);
        if let Some(role) = role {
            task_overrides.insert(
                "build-coder".to_string(),
                TaskOverride {
                    role_id: Some(role.to_string()),
                    workdir: workdir.clone(),
                    image: image.map(String::from),
                    description: Some(format!("dispatch `{role}` into the worktree")),
                    ..Default::default()
                },
            );
        } else if workdir.is_some() || image.is_some() {
            task_overrides.insert(
                "build-coder".to_string(),
                TaskOverride { workdir: workdir.clone(), image: image.map(String::from), ..Default::default() },
            );
        }
        if let Some(workdir) = workdir {
            task_overrides.insert("build-verify".to_string(), TaskOverride { workdir: Some(workdir), ..Default::default() });
        }
    }

    LaunchParams {
        phase_ids: real_phase_ids.clone(),
        task_overrides,
        step_config_overrides: BTreeMap::new(),
        expansions: BTreeMap::new(),
    }
}

/// True when any step in the config's graph names one of the coder-phase
/// Tier 3 kinds — checked BEFORE minting (#1284 review round 1, consider
/// 11) so a config that can't possibly execute doesn't litter a
/// half-launched instance.
fn config_uses_coder_phase_kinds(config: &MissionConfig) -> bool {
    config.phases.iter().any(|p| {
        p.tasks
            .iter()
            .any(|t| t.steps.iter().any(|s| CODER_PHASE_TIER3_KINDS.contains(&s.kind.as_str())))
    })
}

/// Pre-mint check (#1284 review round 1, consider 11): a graph using the
/// coder-phase kinds needs `workdir`/`branch`/`base` to execute. The
/// built-in `coder-phase` config declares them required (so the generic
/// missing-inputs gate fires first); this catches a USER-authored config
/// that uses `mission.*` kinds without declaring those inputs — before
/// anything lands on disk.
fn precheck_coder_phase_inputs(
    config: &MissionConfig,
    collected: &BTreeMap<String, serde_json::Value>,
) -> Result<()> {
    let missing: Vec<&str> = ["workdir", "branch", "base"]
        .into_iter()
        .filter(|name| collected.get(*name).and_then(|v| v.as_str()).is_none())
        .collect();
    if !missing.is_empty() {
        bail!(
            "mission launch: config \"{}\" uses the coder-phase step kinds \
             (mission.worktree/mission.coder/mission.verify) but these input(s) were not \
             supplied: {}. Nothing was minted — pass each as --param <name>=<value> (or in \
             --input's JSON file), and declare them in the config's `inputs` so this is \
             caught by the standard required-inputs gate.",
            config.id,
            missing.join(", ")
        );
    }
    Ok(())
}

/// The dispatch session id for a config-launched coder-phase execution.
/// Deliberately the SAME `mission-run-` prefix `mission_run::run` stamps
/// (#1284 review round 1, must-fix 4): the viewer's mission lens keys its
/// per-run session grouping on that prefix, and this path emits the
/// identical record vocabulary — a `mission-launch-` prefix would make
/// launched runs invisible to the lens for no benefit.
fn launch_session_id(mission_id: &str, real_phase_id: &str) -> String {
    format!("mission-run-{mission_id}-{real_phase_id}")
}

/// Handles `register_coder_phase_kinds` keeps back for the post-scheduler
/// gate decision (#1284 review round 1, must-fix 1a): the two result slots
/// the step kinds populate (the generic `StepOutcome.output: String`
/// contract can't carry the rich verdict/verifier detail), plus the
/// launch-resolved identifiers the gate banners print.
pub(crate) struct CoderPhaseHandles {
    coder_slot: Arc<Mutex<Option<mission_run::CoderStepResult>>>,
    verify_slot: Arc<Mutex<Option<std::result::Result<crate::phase_cli::PhaseReviewOutput, String>>>>,
    workdir: std::path::PathBuf,
    branch: String,
    real_phase_id: String,
    session_id: String,
}

/// Register `mission_run.rs`'s three `coder-phase` Tier 3 kinds against
/// `registry`, using the operator-collected `workdir`/`branch`/`base`/
/// `role`/`image` inputs plus a launcher-resolved `repo_root`. Bails loud
/// (naming the missing input) rather than constructing a kind with an
/// empty path if the graph needs these kinds but the operator didn't
/// supply them. Returns the [`CoderPhaseHandles`] the caller reads after
/// `run_step_graph` to decide the gate outcome.
fn register_coder_phase_kinds(
    registry: &crew::step_kinds::StepKindRegistry,
    mission_id: &str,
    config: &MissionConfig,
    real_phase_ids: &BTreeMap<String, String>,
    collected: &BTreeMap<String, serde_json::Value>,
    timeout_seconds: u32,
) -> Result<CoderPhaseHandles> {
    let require = |name: &str| -> Result<String> {
        collected
            .get(name)
            .and_then(|v| v.as_str())
            .map(String::from)
            .ok_or_else(|| {
                anyhow!(
                    "mission launch: config `{}` uses the coder-phase step kinds but no \
                     `{name}` input was supplied",
                    config.id
                )
            })
    };
    let workdir = std::path::PathBuf::from(require("workdir")?);
    let branch = require("branch")?;
    let base = require("base")?;
    let role = collected
        .get("role")
        .and_then(|v| v.as_str())
        .map(String::from)
        .unwrap_or_else(|| "coder".to_string());
    let image = collected.get("image").and_then(|v| v.as_str()).map(String::from);

    // The `coder-phase` config has exactly one phase; find its real id.
    let phase_doc_id = config
        .phases
        .iter()
        .find(|p| p.tasks.iter().any(|t| t.steps.iter().any(|s| CODER_PHASE_TIER3_KINDS.contains(&s.kind.as_str()))))
        .map(|p| p.id.clone())
        .ok_or_else(|| anyhow!("mission launch: internal error — no phase in `{}` declares a coder-phase step", config.id))?;
    let real_phase_id = real_phase_ids[&phase_doc_id].clone();
    let session_id = launch_session_id(mission_id, &real_phase_id);

    let mission = load_mission_for_brief(mission_id)?;
    let phase = load_phase_for_brief(mission_id, &real_phase_id)?;

    let coder_slot: Arc<Mutex<Option<mission_run::CoderStepResult>>> = Arc::new(Mutex::new(None));
    let verify_slot: Arc<
        Mutex<Option<std::result::Result<crate::phase_cli::PhaseReviewOutput, String>>>,
    > = Arc::new(Mutex::new(None));

    let repo_root = mission_run::repo_root()?;
    registry
        .register(Arc::new(mission_run::MissionWorktreeStepKind {
            repo_root,
            wt_path: workdir.clone(),
            branch: branch.clone(),
            base: base.clone(),
            mission_id: mission_id.to_string(),
            phase_id: real_phase_id.clone(),
            session_id: session_id.clone(),
            role: role.clone(),
        }))
        .map_err(|e| anyhow!("registering mission.worktree: {e}"))?;

    let opts = crew::dispatch::DispatchOpts {
        role_id: role.clone(),
        message: mission_run::coder_brief(&phase, &mission, &[], &[], &[]),
        session_id: Some(session_id.clone()),
        timeout_seconds,
        skip_preflight: false,
        json: true,
        workdir: Some(workdir.clone()),
        phase_id: Some(real_phase_id.clone()),
        machine: None,
        wait: true,
        compaction: crew::dispatch::CompactionDispatchArgs::default(),
        profile_name: None,
        config_path: None,
        force_container: false,
        max_completion_tokens: None,
        image: image.clone(),
        model_base_url_override: None,
    };
    registry
        .register(Arc::new(mission_run::MissionCoderStepKind {
            opts: Mutex::new(Some(opts)),
            wt_path: workdir.clone(),
            mission_id: mission_id.to_string(),
            phase_id: real_phase_id.clone(),
            session_id: session_id.clone(),
            role_id: role,
            result_slot: coder_slot.clone(),
        }))
        .map_err(|e| anyhow!("registering mission.coder: {e}"))?;

    registry
        .register(Arc::new(mission_run::MissionVerifyStepKind {
            wt_path: workdir.clone(),
            base,
            phase_id: real_phase_id.clone(),
            result_slot: verify_slot.clone(),
        }))
        .map_err(|e| anyhow!("registering mission.verify: {e}"))?;

    Ok(CoderPhaseHandles {
        coder_slot,
        verify_slot,
        workdir,
        branch,
        real_phase_id,
        session_id,
    })
}

/// The post-scheduler gate decision for a coder-phase graph — a faithful
/// mirror of `mission_run::run`'s own post-graph sequence (#1284 review
/// round 1, must-fix 1), same outcome map, same banners, same records:
///
/// | condition                    | outcome                                | exit |
/// |------------------------------|----------------------------------------|------|
/// | worktree step errored        | hard `Err` (same as `mission run`)     | err  |
/// | coder step errored           | phase Running; worktree kept           | 1    |
/// | verify step errored          | "gate — QA unavailable" banner         | 3    |
/// | QA found blocker(s)          | "QA found N blocker(s)" gate banner    | 2    |
/// | clean / flags-only           | "gate — awaiting sign-off" banner      | 0    |
///
/// Never transitions the phase or the mission: the phase stays `Running`
/// and `mission ship <mission-id> --phase <phase-id>` (or `mission abort`)
/// is the operator's next move, exactly as after a `mission run`.
fn coder_phase_gate_outcome(
    mission_id: &str,
    handles: &CoderPhaseHandles,
    steps: &BTreeMap<String, crew::types::Step>,
) -> Result<i32> {
    let worktree_step_id = format!("{}-worktree-step", handles.real_phase_id);
    let coder_step_id = format!("{}-coder-step", handles.real_phase_id);
    let verify_step_id = format!("{}-verify-step", handles.real_phase_id);
    let phase_id = &handles.real_phase_id;
    let session_id = &handles.session_id;

    // Worktree creation failing is a hard stop — same as `mission run`'s
    // pre-migration `add_worktree(...)?` propagating out of `run()`.
    if steps[&worktree_step_id].status == NodeStatus::Error {
        bail!(
            "{}",
            steps[&worktree_step_id]
                .output
                .clone()
                .unwrap_or_else(|| "worktree step failed".to_string())
        );
    }

    // Coder dispatch failing maps to `mission run`'s early `return Ok(1)`;
    // the step kind itself already printed the error + emitted the
    // `mission.coder` error record. verify never ran (unreachable).
    if steps[&coder_step_id].status == NodeStatus::Error {
        return Ok(1);
    }

    let coder_result = handles
        .coder_slot
        .lock()
        .expect("mission.coder result mutex poisoned")
        .take();
    let failed_verifiers = coder_result
        .as_ref()
        .map(|r| r.failed_verifiers.clone())
        .unwrap_or_default();
    let tokens_total = coder_result.map(|r| r.tokens_total).unwrap_or(0);

    // QA dispatch itself failing is NOT a coder failure — `mission run`'s
    // distinct exit 3 path ("gate — QA unavailable, manual review
    // required").
    if steps[&verify_step_id].status == NodeStatus::Error {
        let verify_err = handles
            .verify_slot
            .lock()
            .expect("mission.verify result mutex poisoned")
            .take();
        let err_text = match verify_err {
            Some(Err(msg)) => msg,
            _ => "QA dispatch failed".to_string(),
        };
        mission_run::emit_step_result(
            flow::Level::Warn,
            "mission.verify",
            &verify_step_id,
            mission_id,
            phase_id,
            session_id,
            serde_json::json!({ "error": err_text, "total_tokens": tokens_total }),
        );
        println!("\n{}", style::header("▶ gate — QA unavailable, manual review required"));
        mission_run::print_unverified_banner(&failed_verifiers);
        println!("  {} {}", style::dim("worktree:"), handles.workdir.display());
        println!("  {} {}", style::dim("branch:  "), style::accent(&handles.branch));
        println!(
            "\n{}",
            style::warn(&format!(
                "review the diff manually, then:  darkmux mission ship {mission_id} --phase \
                 {phase_id} (or abort: darkmux mission abort {mission_id} --phase {phase_id})"
            ))
        );
        return Ok(3);
    }

    let review = match handles
        .verify_slot
        .lock()
        .expect("mission.verify result mutex poisoned")
        .take()
    {
        Some(Ok(review)) => review,
        // Unreachable in practice — see `mission_run::run`'s identical arm.
        _ => bail!("internal error: mission.verify step completed without a review result"),
    };

    // Stop at the gate. Tee up the ship step; never commit/PR/merge here.
    println!("\n{}", style::header("▶ gate — awaiting frontier/operator sign-off"));
    println!("  {} {}", style::dim("worktree:"), handles.workdir.display());
    println!("  {} {}", style::dim("branch:  "), style::accent(&handles.branch));
    mission_run::print_unverified_banner(&failed_verifiers);

    if review.by_severity.block > 0 {
        println!(
            "\n{}",
            style::warn(&format!(
                "⚠ QA found {} blocker(s). Resolve them (dispatch a fix into the worktree, or \
                 edit directly) before shipping.",
                review.by_severity.block
            ))
        );
        println!(
            "  {}",
            style::dim("re-run QA after fixing: darkmux phase review (in the worktree)")
        );
        println!(
            "  {}",
            style::dim(&format!(
                "or abandon this run: darkmux mission abort {mission_id} --phase {phase_id}"
            ))
        );
        mission_run::emit_step_result(
            flow::Level::Warn,
            "mission.verify",
            &verify_step_id,
            mission_id,
            phase_id,
            session_id,
            serde_json::json!({
                "verdict": review.verdict,
                "blockers": review.by_severity.block,
                "flags": review.by_severity.flag,
                "total_tokens": tokens_total,
            }),
        );
        return Ok(2);
    }

    println!(
        "\n{}",
        style::success(&format!(
            "✓ ready for sign-off. After review:  darkmux mission ship {mission_id} --phase {phase_id}"
        ))
    );
    println!(
        "{}",
        style::dim(&format!(
            "  record your adjudication (audit trail):  darkmux flow note \
             --session-id {session_id} \
             --text \"<verdict · what you overrode · why>\" --source adjudication",
        ))
    );
    mission_run::emit_step_result(
        flow::Level::Info,
        "mission.verify",
        &verify_step_id,
        mission_id,
        phase_id,
        session_id,
        serde_json::json!({
            "verdict": review.verdict,
            "blockers": 0,
            "flags": review.by_severity.flag,
            "nits": review.by_severity.nit,
            "total_tokens": tokens_total,
        }),
    );
    Ok(0)
}

fn load_mission_for_brief(mission_id: &str) -> Result<Mission> {
    let text = std::fs::read_to_string(crew::lifecycle::mission_path(mission_id))
        .with_context(|| format!("reading mission.json for `{mission_id}`"))?;
    serde_json::from_str(&text).context("parsing mission.json")
}

// `pub(crate)` — `mission_launch_review.rs` reuses this (and
// `lazy_start_phase_for_step` below) rather than re-deriving the same
// read.
pub(crate) fn load_phase_for_brief(mission_id: &str, phase_id: &str) -> Result<Phase> {
    let text = std::fs::read_to_string(crew::lifecycle::phase_path(mission_id, phase_id))
        .with_context(|| format!("reading phase JSON for `{phase_id}`"))?;
    serde_json::from_str(&text).context("parsing phase JSON")
}

/// (#1400) Called from a `run_step_graph`/`run_review_graph` `persist`
/// closure on EVERY step transition this dispatch performs — starts
/// `phase_id` the FIRST time one of ITS OWN steps flips to `Running`, and
/// is a no-op for every other call: a terminal (`Complete`/`Error`)
/// transition never starts anything, and a SECOND step in an
/// already-started phase is skipped via `started` (a phase whose `Running`
/// flip already fired would otherwise hit `phase_start`'s "already
/// Running" error on every subsequent step in the same phase — the state
/// machine only allows the transition once).
///
/// This is the mechanism that makes phases start LAZILY instead of every
/// phase pulsing "running" from second zero at mint: a downstream phase
/// (e.g. review's `adjudicate`/`report`) whose steps the scheduler hasn't
/// reached yet never gets a `persist` call with `Running` for one of its
/// own steps, so it stays `Planned` until the graph actually reaches it —
/// the pipeline-progressing-left-to-right story the graph lens is meant to
/// tell.
///
/// Reads a fresh phase status per FIRST-encountered phase (never trusts a
/// caller-precomputed status, which could be stale by the time the
/// scheduler reaches this phase in a long-running dispatch) — `Planned`/
/// `Abandoned` starts it; `Running` (a relaunch of a gated run mid-flight)
/// and `Complete` (a relaunch past a terminal phase — logged separately by
/// the caller's own preflight pass) are left alone. Failure to start is a
/// loud dim warning, never a hard error — the same "continue, state can be
/// reconciled with `darkmux phase` verbs" posture the pre-#1400 eager loop
/// used.
pub(crate) fn lazy_start_phase_for_step(
    mission_id: &str,
    phase_id: &str,
    step_status: crew::types::NodeStatus,
    started: &mut std::collections::HashSet<String>,
) {
    use crew::types::NodeStatus;
    if step_status != NodeStatus::Running {
        return;
    }
    if phase_id.is_empty() || !started.insert(phase_id.to_string()) {
        return;
    }
    let status = load_phase_for_brief(mission_id, phase_id)
        .map(|p| p.status)
        .unwrap_or(PhaseStatus::Planned);
    if matches!(status, PhaseStatus::Planned | PhaseStatus::Abandoned) {
        if let Err(e) = crew::lifecycle::phase_start(phase_id) {
            eprintln!(
                "{}",
                style::dim(&format!(
                    "mission launch: phase_start({phase_id}) failed: {e:#} — continuing; state \
                     can be reconciled with `darkmux phase` verbs."
                ))
            );
        }
    }
}

/// (#1406) Derive each executed phase's finalization outcome from THAT
/// phase's OWN step statuses, rather than stamping every phase with one
/// uniform run-level outcome. The old uniform mapping marked a never-started
/// (`Planned`) phase `Complete` on a `Degraded` run; `finalize_mission` then
/// called `phase_complete` on a `Planned` phase, which the state machine
/// refuses, leaving a Closed mission with a permanently `Planned` phase whose
/// `envelope.json` disagreed with disk. The honest per-phase rules:
///
/// - all of the phase's steps `Complete` (and it has at least one) → Complete
/// - any errored step → Abandoned (errored)
/// - a phase the scheduler never reached (no started steps) → Abandoned
/// - any step left non-terminal (`Running`/`Planned`) → Abandoned
///
/// A phase is `Complete` ONLY when it genuinely finished. Everything else is
/// honestly Abandoned; `PhaseOutcomeKind` has no `Error` variant, so an
/// errored phase abandons, matching the existing terminal status vocabulary.
/// Only phases that actually had tasks in this run are named (a phase with no
/// tasks is a freeform/manual phase this launcher doesn't drive).
fn derive_phase_outcomes(
    config: &MissionConfig,
    real_phase_ids: &BTreeMap<String, String>,
    tasks: &[crew::types::Task],
    steps: &BTreeMap<String, crew::types::Step>,
) -> Vec<crew::envelope::PhaseOutcome> {
    use crew::envelope::PhaseOutcome;
    config
        .phases
        .iter()
        .filter_map(|p| {
            let real_id = &real_phase_ids[&p.id];
            let phase_step_ids: Vec<&str> = tasks
                .iter()
                .filter(|t| &t.phase_id == real_id)
                .flat_map(|t| t.step_ids.iter().map(String::as_str))
                .collect();
            if phase_step_ids.is_empty() {
                // Not an executed phase (no tasks/steps), so nothing to finalize.
                return None;
            }
            let phase_steps: Vec<&crew::types::Step> =
                phase_step_ids.iter().filter_map(|sid| steps.get(*sid)).collect();
            let (outcome, reason) = phase_finalization(&phase_steps);
            Some(PhaseOutcome { phase_id: real_id.clone(), outcome, reason })
        })
        .collect()
}

/// (#1406) The per-phase outcome + provenance for one phase's step slice.
/// See [`derive_phase_outcomes`] for the rules.
fn phase_finalization(phase_steps: &[&crew::types::Step]) -> (crew::envelope::PhaseOutcomeKind, Option<String>) {
    use crew::envelope::PhaseOutcomeKind;
    let errored = phase_steps.iter().filter(|s| s.status == NodeStatus::Error).count();
    let any_started = phase_steps.iter().any(|s| s.status != NodeStatus::Planned);
    let all_complete = !phase_steps.is_empty() && phase_steps.iter().all(|s| s.status == NodeStatus::Complete);
    if all_complete {
        (PhaseOutcomeKind::Complete, None)
    } else if errored > 0 {
        (PhaseOutcomeKind::Abandoned, Some(format!("{errored} step(s) errored")))
    } else if !any_started {
        (PhaseOutcomeKind::Abandoned, Some("phase never started (scheduler did not reach it)".to_string()))
    } else {
        (PhaseOutcomeKind::Abandoned, Some("phase did not complete (steps left non-terminal)".to_string()))
    }
}

/// Fold the interpreted graph's final step statuses into a
/// [`crew::envelope::MissionEnvelope`] — the generic (mission-type-
/// agnostic) status decision: every step Complete → Clean; some Complete
/// and some Error → Degraded (real output produced, but part of the run was
/// constrained); every relevant step Error (nothing completed) → Error.
/// Per-phase finalization outcomes come from [`derive_phase_outcomes`]; the
/// run-level `status` is NOT stamped uniformly onto every phase (#1406). See
/// `envelope.rs`'s own module doc for the phase/mission-outcome mapping.
fn build_envelope(
    mission_id: &str,
    config: &MissionConfig,
    real_phase_ids: &BTreeMap<String, String>,
    tasks: &[crew::types::Task],
    steps: &BTreeMap<String, crew::types::Step>,
) -> crew::envelope::MissionEnvelope {
    use crew::envelope::{MissionEnvelope, MissionOutcomeStatus};

    let errored: Vec<&str> = steps
        .values()
        .filter(|s| s.status == NodeStatus::Error)
        .map(|s| s.id.as_str())
        .collect();
    let completed: Vec<&str> = steps
        .values()
        .filter(|s| s.status == NodeStatus::Complete)
        .map(|s| s.id.as_str())
        .collect();

    let status = if errored.is_empty() {
        MissionOutcomeStatus::Clean
    } else if completed.is_empty() {
        MissionOutcomeStatus::Error
    } else {
        MissionOutcomeStatus::Degraded
    };

    let reason = if errored.is_empty() {
        None
    } else {
        Some(
            errored
                .iter()
                .map(|id| {
                    let out = steps[*id].output.clone().unwrap_or_default();
                    format!("{id}: {out}")
                })
                .collect::<Vec<_>>()
                .join("; "),
        )
    };

    // (#1406) Per-phase outcomes derived from each phase's OWN steps, NOT
    // the run-level `status` stamped uniformly (which marked a never-started
    // phase Complete on a Degraded run). `new(.., &[])` seeds the schema
    // version + defaults; the honest phases override the (empty) default.
    let mut envelope = MissionEnvelope::new(mission_id, status, &[]);
    envelope.phases = derive_phase_outcomes(config, real_phase_ids, tasks, steps);
    envelope.reason = reason;
    if !errored.is_empty() && !completed.is_empty() {
        envelope.warnings = vec![format!("{} of {} step(s) errored during launch execution", errored.len(), steps.len())];
    }
    envelope.payload = serde_json::json!({
        "completed_steps": completed,
        "errored_steps": errored,
    });
    envelope
}

/// (#1406, F4) Error-path reconcile. A scheduler-level `Err` mid-run (a step
/// kind lookup failure, a `run_bounded` failure) propagates through
/// `run_step_graph`'s `?` BEFORE the normal finalize runs, leaving steps
/// persisted as `Running` and the mission Active with `Running` phases
/// forever: the same stranded-Active drift class an operator hit at scale
/// (10 Active missions whose phases were stranded `running` with no process
/// behind them, mobile report 2026-07-16). This brings the failed run to an
/// honest terminal state, exactly as the review launcher already does by
/// always finalizing off its captured `Result` (never `?`-propagating past
/// the finalize): flip every still-`Running` step to `Error` (persisting it),
/// then finalize the mission with an Error-status envelope whose PER-PHASE
/// outcomes come from each phase's own steps ([`derive_phase_outcomes`]), so a
/// phase that fully completed before the failure still reads `Complete`;
/// everything the failure interrupted or never reached abandons.
///
/// Best-effort throughout, matching [`crew::envelope::finalize_mission`]'s
/// own discipline: the caller still propagates the original `Err`, so the
/// failure is never swallowed; a persistence hiccup here degrades only the
/// mission-board VIEW.
fn reconcile_and_finalize_on_error(
    mission_id: &str,
    config: &MissionConfig,
    real_phase_ids: &BTreeMap<String, String>,
    tasks: &[crew::types::Task],
    steps: &mut BTreeMap<String, crew::types::Step>,
    err: &anyhow::Error,
) {
    use crew::envelope::{MissionEnvelope, MissionOutcomeStatus};

    // step id → owning phase id, so a flipped step persists under the right
    // phase directory.
    let phase_of_step: BTreeMap<&str, &str> = tasks
        .iter()
        .flat_map(|t| t.step_ids.iter().map(move |sid| (sid.as_str(), t.phase_id.as_str())))
        .collect();

    let mut reconciled = 0usize;
    for step in steps.values_mut() {
        if step.status == NodeStatus::Running {
            step.status = NodeStatus::Error;
            if step.output.is_none() {
                step.output = Some("interrupted by a mission-level error before completion".to_string());
            }
            reconciled += 1;
            if let Some(phase_id) = phase_of_step.get(step.id.as_str()) {
                if let Err(e) = crew::lifecycle::save_step(mission_id, phase_id, step) {
                    eprintln!(
                        "{}",
                        style::dim(&format!("mission launch: reconcile step persist warning: {e:#}"))
                    );
                }
            }
        }
    }

    let mut envelope = MissionEnvelope::new(mission_id, MissionOutcomeStatus::Error, &[]);
    envelope.phases = derive_phase_outcomes(config, real_phase_ids, tasks, steps);
    envelope.reason = Some(format!("mission launch errored mid-run: {err:#}"));
    if reconciled > 0 {
        envelope.warnings =
            vec![format!("{reconciled} running step(s) reconciled to error on the failure path")];
    }
    let completed: Vec<&str> =
        steps.values().filter(|s| s.status == NodeStatus::Complete).map(|s| s.id.as_str()).collect();
    let errored: Vec<&str> =
        steps.values().filter(|s| s.status == NodeStatus::Error).map(|s| s.id.as_str()).collect();
    envelope.payload = serde_json::json!({
        "completed_steps": completed,
        "errored_steps": errored,
    });
    crew::envelope::finalize_mission(&envelope);
}

fn print_run_summary(mission_id: &str, steps: &BTreeMap<String, crew::types::Step>) {
    let complete = steps.values().filter(|s| s.status == NodeStatus::Complete).count();
    let errored = steps.values().filter(|s| s.status == NodeStatus::Error).count();
    println!(
        "\n{}",
        style::header(&format!("▶ mission `{mission_id}` finished — {complete} step(s) complete, {errored} errored"))
    );
    println!("  {}", style::dim(&format!("darkmux mission status   (or) darkmux mission debrief {mission_id}")));
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::env;
    use std::io::Write as _;
    use tempfile::{NamedTempFile, TempDir};

    /// Isolates both the crew root (mission/phase/config JSON) and the flow
    /// sink to TempDirs — mirrors `envelope.rs`'s own `CrewGuard`. Every
    /// test using this MUST be `#[serial_test::serial]` since env-var
    /// mutation is a global, cross-test concern.
    struct LaunchTestGuard {
        _tmp_crew: TempDir,
        _tmp_flows: TempDir,
        prev_crew: Option<String>,
        prev_flows: Option<String>,
    }

    impl LaunchTestGuard {
        fn new() -> Self {
            let tmp_crew = TempDir::new().unwrap();
            let tmp_flows = TempDir::new().unwrap();
            let prev_crew = env::var("DARKMUX_CREW_DIR").ok();
            let prev_flows = env::var("DARKMUX_FLOWS_DIR").ok();
            // SAFETY: serialized via #[serial_test::serial] on every caller.
            unsafe {
                env::set_var("DARKMUX_CREW_DIR", tmp_crew.path());
                env::set_var("DARKMUX_FLOWS_DIR", tmp_flows.path());
            }
            Self { _tmp_crew: tmp_crew, _tmp_flows: tmp_flows, prev_crew, prev_flows }
        }

        fn write_config(&self, id: &str, json: &str) {
            let dir = crew::loader::mission_configs_dir();
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join(format!("{id}.json")), json).unwrap();
        }
    }

    impl Drop for LaunchTestGuard {
        fn drop(&mut self) {
            // SAFETY: serialized via #[serial_test::serial] on every caller.
            unsafe {
                match &self.prev_crew {
                    Some(v) => env::set_var("DARKMUX_CREW_DIR", v),
                    None => env::remove_var("DARKMUX_CREW_DIR"),
                }
                match &self.prev_flows {
                    Some(v) => env::set_var("DARKMUX_FLOWS_DIR", v),
                    None => env::remove_var("DARKMUX_FLOWS_DIR"),
                }
            }
        }
    }

    const FREEFORM_CONFIG: &str = r#"{
        "id": "freeform-test-mission",
        "name": "Freeform Test Mission",
        "description": "a hand-authored-style freeform test mission",
        "schema_version": "1.1",
        "phases": [
            {"id": "p1", "description": "first phase"},
            {"id": "p2", "description": "second phase"}
        ]
    }"#;

    // ── Generic launch path — freeform (no dispatch, no Docker) ────────

    #[test]
    #[serial_test::serial]
    fn freeform_launch_mints_instance_and_leaves_phases_manual() {
        let guard = LaunchTestGuard::new();
        guard.write_config("freeform-test-mission", FREEFORM_CONFIG);

        let exit = launch("freeform-test-mission", None, &[], None).expect("launch should succeed");
        assert_eq!(exit, 0);

        // No operator inputs at all -> the id derives from an empty map,
        // deterministically, every time.
        let mission_id = derive_mission_id("freeform-test-mission", &BTreeMap::new()).unwrap();

        let mission_path = crew::lifecycle::mission_path(&mission_id);
        assert!(mission_path.is_file(), "mission.json must exist at {}", mission_path.display());
        let mission: Mission = serde_json::from_str(&std::fs::read_to_string(&mission_path).unwrap()).unwrap();
        assert_eq!(mission.status, MissionStatus::Active);
        assert!(mission.started_ts.is_some(), "launch drives mission_start_with_reasoning, not a bare Active write");
        assert_eq!(mission.phase_ids.len(), 2);
        assert_eq!(mission.phase_ids[0], format!("{mission_id}-p1"));
        assert_eq!(mission.phase_ids[1], format!("{mission_id}-p2"));

        for real_phase_id in &mission.phase_ids {
            let phase_path = crew::lifecycle::phase_path(&mission_id, real_phase_id);
            assert!(phase_path.is_file(), "phase JSON must exist at {}", phase_path.display());
            let phase: Phase = serde_json::from_str(&std::fs::read_to_string(&phase_path).unwrap()).unwrap();
            assert_eq!(phase.status, PhaseStatus::Planned, "freeform phases are never auto-started");
            assert!(phase.started_ts.is_none());
        }

        let snapshot_path = crew::lifecycle::config_snapshot_path(&mission_id);
        assert!(snapshot_path.is_file(), "config-snapshot.json must exist at {}", snapshot_path.display());
        let snapshot = crew::lifecycle::load_config_snapshot(&mission_id).unwrap().unwrap();
        assert_eq!(snapshot.id, "freeform-test-mission");
        assert_eq!(snapshot.phases.len(), 2);

        // No MissionEnvelope for a freeform launch — nothing executed, so
        // finalize_mission never ran.
        assert!(crew::lifecycle::load_envelope(&mission_id).unwrap().is_none());

        // Idempotent relaunch: same (empty) inputs must derive the SAME id.
        let exit2 = launch("freeform-test-mission", None, &[], None).expect("relaunch should succeed");
        assert_eq!(exit2, 0);
        let mission_id2 = derive_mission_id("freeform-test-mission", &BTreeMap::new()).unwrap();
        assert_eq!(mission_id, mission_id2, "same inputs must derive the same instance id");
    }

    #[test]
    #[serial_test::serial]
    fn freeform_relaunch_after_close_reopens_the_terminal_instance() {
        let guard = LaunchTestGuard::new();
        guard.write_config("freeform-test-mission", FREEFORM_CONFIG);

        launch("freeform-test-mission", None, &[], None).unwrap();
        let mission_id = derive_mission_id("freeform-test-mission", &BTreeMap::new()).unwrap();

        crew::lifecycle::mission_close_with_reasoning(&mission_id, Some("test close")).unwrap();
        let closed: Mission =
            serde_json::from_str(&std::fs::read_to_string(crew::lifecycle::mission_path(&mission_id)).unwrap())
                .unwrap();
        assert_eq!(closed.status, MissionStatus::Closed);

        // Relaunch: same config, same (empty) inputs -> reopens the SAME
        // instance (Packet 2 reopen semantics) rather than minting a
        // duplicate or erroring on "already exists".
        let exit = launch("freeform-test-mission", None, &[], None).unwrap();
        assert_eq!(exit, 0);
        let reopened: Mission =
            serde_json::from_str(&std::fs::read_to_string(crew::lifecycle::mission_path(&mission_id)).unwrap())
                .unwrap();
        assert_eq!(reopened.status, MissionStatus::Active, "relaunch must reopen a terminal instance");
        assert!(reopened.closed_ts.is_none(), "reopen clears the prior closure");
        assert_eq!(reopened.id, mission_id, "relaunch reuses the SAME instance id, never a duplicate");
    }

    #[test]
    #[serial_test::serial]
    fn different_inputs_derive_a_distinct_instance_for_the_same_config() {
        let guard = LaunchTestGuard::new();
        guard.write_config("freeform-test-mission", FREEFORM_CONFIG);

        launch("freeform-test-mission", None, &["note=first".to_string()], None).unwrap();
        launch("freeform-test-mission", None, &["note=second".to_string()], None).unwrap();

        let mut m1 = BTreeMap::new();
        m1.insert("note".to_string(), serde_json::Value::String("first".to_string()));
        let mut m2 = BTreeMap::new();
        m2.insert("note".to_string(), serde_json::Value::String("second".to_string()));
        let id1 = derive_mission_id("freeform-test-mission", &m1).unwrap();
        let id2 = derive_mission_id("freeform-test-mission", &m2).unwrap();
        assert_ne!(id1, id2);
        assert!(crew::lifecycle::mission_path(&id1).is_file());
        assert!(crew::lifecycle::mission_path(&id2).is_file());
        let _ = guard;
    }

    #[test]
    #[serial_test::serial]
    fn missing_required_inputs_bails_with_a_copy_pasteable_example_and_mints_nothing() {
        let guard = LaunchTestGuard::new();
        // `coder-phase` is embedded — resolves with no user-tier file at all
        // (this test never writes one), and declares workdir/branch/base as
        // required inputs the operator hasn't supplied.
        let err = launch("coder-phase", None, &[], None).expect_err("missing required inputs must bail");
        let msg = err.to_string();
        assert!(msg.contains("workdir"), "{msg}");
        assert!(msg.contains("branch"), "{msg}");
        assert!(msg.contains("base"), "{msg}");
        assert!(msg.contains("--param"), "expected a copy-pasteable --param example: {msg}");
        assert!(msg.contains("Example --input file"), "expected a copy-pasteable --input example: {msg}");
        assert!(!msg.contains("`mission_id`:"), "mission_id is launcher-supplied, never asked of the operator: {msg}");

        // Nothing minted for any coder-phase-derived id.
        let missions_dir = crew::loader::missions_dir();
        assert!(
            !missions_dir.is_dir() || std::fs::read_dir(&missions_dir).unwrap().next().is_none(),
            "a missing-inputs bail must not mint anything"
        );
        let _ = guard;
    }

    // ── coder-phase wiring — registration only, never a live dispatch ──
    // (mocked dispatches only, never real LMStudio — the actual
    // `MissionCoderStepKind::run` dispatch is exercised, mocked, by
    // `crates/darkmux-crew/tests/mock_dispatch_proof.rs` against the SAME
    // underlying `crew::dispatch::dispatch` primitive this module reuses
    // unchanged; a live coder-phase dogfood dispatch is the release-gate
    // discipline's job per CLAUDE.md, not a `cargo test`-embedded one.)

    #[test]
    #[serial_test::serial]
    fn coder_phase_registration_succeeds_with_a_real_git_repo_and_valid_inputs() {
        let guard = LaunchTestGuard::new();
        let loaded = mission_config::load("coder-phase").unwrap();
        let config = &loaded.config;

        let mut collected = BTreeMap::new();
        collected.insert("workdir".to_string(), serde_json::json!("/tmp/darkmux-mission-launch-test-worktree"));
        collected.insert("branch".to_string(), serde_json::json!("darkmux-test-branch"));
        collected.insert("base".to_string(), serde_json::json!("main"));
        collected.insert("role".to_string(), serde_json::json!("coder"));

        let mission_id = derive_mission_id("coder-phase", &collected).unwrap();
        let (real_phase_ids, reused) = ensure_mission_and_phases(&mission_id, config).unwrap();
        assert!(!reused, "a fresh mint must not report reuse");

        let registry = crew::step_kinds::StepKindRegistry::with_builtins();
        let handles =
            register_coder_phase_kinds(&registry, &mission_id, config, &real_phase_ids, &collected, 600)
                .expect("registration must succeed against a real repo + valid inputs");

        for kind in CODER_PHASE_TIER3_KINDS {
            assert!(registry.get(kind).is_ok(), "kind `{kind}` must be registered");
        }
        // (#1284 review round 1, must-fix 4) The viewer's mission lens keys
        // on the `mission-run-` session-id prefix — a config-launched run
        // must stamp the SAME prefix or it's invisible to the lens.
        assert!(
            handles.session_id.starts_with("mission-run-"),
            "session id must carry the viewer's mission-run- prefix, got {}",
            handles.session_id
        );
        let _ = guard;
    }

    #[test]
    #[serial_test::serial]
    fn register_coder_phase_kinds_bails_loud_naming_the_missing_input() {
        let guard = LaunchTestGuard::new();
        let loaded = mission_config::load("coder-phase").unwrap();
        let config = &loaded.config;
        let collected: BTreeMap<String, serde_json::Value> = BTreeMap::new();
        let mission_id = derive_mission_id("coder-phase", &collected).unwrap();
        let (real_phase_ids, _) = ensure_mission_and_phases(&mission_id, config).unwrap();
        let registry = crew::step_kinds::StepKindRegistry::with_builtins();
        let err = match register_coder_phase_kinds(&registry, &mission_id, config, &real_phase_ids, &collected, 600)
        {
            Err(e) => e,
            Ok(_) => panic!("must bail without workdir/branch/base supplied"),
        };
        assert!(err.to_string().contains("workdir"), "{err}");
        let _ = guard;
    }

    // ── Gate outcome map (#1284 review round 1, must-fix 1) — mirrors ──
    // mission_run::run's own post-graph decision, pinned per condition.
    // Slots + step statuses are scripted (mocked dispatches only); the
    // load-bearing assertions are the exit code AND the phase staying
    // Running (ship-able), never auto-finalized past the sign-off gate.

    fn scripted_step(id: &str, status: NodeStatus) -> crew::types::Step {
        crew::types::Step {
            id: id.to_string(),
            task_id: format!("{id}-task"),
            kind: "mission.test".to_string(),
            status,
            config: serde_json::Value::Null,
            started_ts: None,
            completed_ts: None,
            output: None,
        }
    }

    fn scripted_gate_fixture(
        phase_id: &str,
        worktree: NodeStatus,
        coder: NodeStatus,
        verify: NodeStatus,
    ) -> (CoderPhaseHandles, BTreeMap<String, crew::types::Step>) {
        let handles = CoderPhaseHandles {
            coder_slot: Arc::new(Mutex::new(Some(mission_run::CoderStepResult {
                failed_verifiers: Vec::new(),
                tokens_total: 123,
            }))),
            verify_slot: Arc::new(Mutex::new(None)),
            workdir: std::path::PathBuf::from("/tmp/gate-test-worktree"),
            branch: "gate-test-branch".to_string(),
            real_phase_id: phase_id.to_string(),
            session_id: launch_session_id("gate-test-mission", phase_id),
        };
        let mut steps = BTreeMap::new();
        for (suffix, status) in [("worktree", worktree), ("coder", coder), ("verify", verify)] {
            let id = format!("{phase_id}-{suffix}-step");
            steps.insert(id.clone(), scripted_step(&id, status));
        }
        (handles, steps)
    }

    fn review_output(block: usize, flag: usize, verdict: &str) -> crate::phase_cli::PhaseReviewOutput {
        crate::phase_cli::PhaseReviewOutput {
            branch: "gate-test-branch".to_string(),
            base: "main".to_string(),
            reviewer_session_id: None,
            diff_files_changed: 1,
            total_findings: block + flag,
            by_severity: crate::phase_cli::SeverityCounts { block, flag, nit: 0 },
            findings: Vec::new(),
            verdict: verdict.to_string(),
        }
    }

    /// Seed a Running mission+phase so the gate tests can assert the end
    /// state is still Running (ship-able) after the outcome decision.
    fn seed_running_instance(mission_id: &str, phase_id: &str) {
        let now = 1_700_000_000u64;
        let mission = Mission {
            id: mission_id.to_string(),
            description: "gate test".to_string(),
            status: MissionStatus::Active,
            phase_ids: vec![phase_id.to_string()],
            created_ts: now,
            started_ts: Some(now),
            closed_ts: None,
            paused_ts: None,
            source_input: None,
            ticket: None,
        };
        crew::lifecycle::save_mission(&mission).unwrap();
        let mut phase = new_planned_phase(mission_id, phase_id, Some("gate phase"), None, now);
        phase.status = PhaseStatus::Running;
        phase.started_ts = Some(now);
        crew::lifecycle::save_phase(&phase).unwrap();
    }

    fn phase_status_on_disk(mission_id: &str, phase_id: &str) -> PhaseStatus {
        load_phase_for_brief(mission_id, phase_id).unwrap().status
    }

    fn mission_status_on_disk(mission_id: &str) -> MissionStatus {
        load_mission_for_brief(mission_id).unwrap().status
    }

    #[test]
    #[serial_test::serial]
    fn gate_outcome_qa_blockers_exit_2_and_phase_stays_running_shippable() {
        let _guard = LaunchTestGuard::new();
        let phase_id = "gate-test-mission-build";
        seed_running_instance("gate-test-mission", phase_id);
        let (handles, steps) =
            scripted_gate_fixture(phase_id, NodeStatus::Complete, NodeStatus::Complete, NodeStatus::Complete);
        *handles.verify_slot.lock().unwrap() = Some(Ok(review_output(2, 1, "blockers")));

        let exit = coder_phase_gate_outcome("gate-test-mission", &handles, &steps).unwrap();
        assert_eq!(exit, 2, "QA blockers must exit 2, mirroring `mission run`");
        assert_eq!(
            phase_status_on_disk("gate-test-mission", phase_id),
            PhaseStatus::Running,
            "the phase must stay Running at the gate — ship-able after the operator resolves"
        );
        assert_eq!(
            mission_status_on_disk("gate-test-mission"),
            MissionStatus::Active,
            "the mission must never auto-close past the sign-off gate"
        );
    }

    #[test]
    #[serial_test::serial]
    fn gate_outcome_coder_dispatch_failure_exit_1_and_phase_stays_running() {
        let _guard = LaunchTestGuard::new();
        let phase_id = "gate-test-mission-build";
        seed_running_instance("gate-test-mission", phase_id);
        let (handles, steps) =
            scripted_gate_fixture(phase_id, NodeStatus::Complete, NodeStatus::Error, NodeStatus::Planned);

        let exit = coder_phase_gate_outcome("gate-test-mission", &handles, &steps).unwrap();
        assert_eq!(exit, 1, "a failed coder dispatch must exit 1, never read Degraded/0");
        assert_eq!(phase_status_on_disk("gate-test-mission", phase_id), PhaseStatus::Running);
        assert_eq!(mission_status_on_disk("gate-test-mission"), MissionStatus::Active);
    }

    #[test]
    #[serial_test::serial]
    fn gate_outcome_qa_unavailable_exit_3_named() {
        let _guard = LaunchTestGuard::new();
        let phase_id = "gate-test-mission-build";
        seed_running_instance("gate-test-mission", phase_id);
        let (handles, steps) =
            scripted_gate_fixture(phase_id, NodeStatus::Complete, NodeStatus::Complete, NodeStatus::Error);
        *handles.verify_slot.lock().unwrap() = Some(Err("reviewer image pull failed".to_string()));

        let exit = coder_phase_gate_outcome("gate-test-mission", &handles, &steps).unwrap();
        assert_eq!(exit, 3, "QA-unavailable must exit 3, mirroring `mission run`");
        assert_eq!(phase_status_on_disk("gate-test-mission", phase_id), PhaseStatus::Running);
    }

    #[test]
    #[serial_test::serial]
    fn gate_outcome_clean_exit_0_and_no_finalize_past_the_gate() {
        let _guard = LaunchTestGuard::new();
        let phase_id = "gate-test-mission-build";
        seed_running_instance("gate-test-mission", phase_id);
        let (handles, steps) =
            scripted_gate_fixture(phase_id, NodeStatus::Complete, NodeStatus::Complete, NodeStatus::Complete);
        *handles.verify_slot.lock().unwrap() = Some(Ok(review_output(0, 1, "flags-only")));

        let exit = coder_phase_gate_outcome("gate-test-mission", &handles, &steps).unwrap();
        assert_eq!(exit, 0);
        assert_eq!(
            phase_status_on_disk("gate-test-mission", phase_id),
            PhaseStatus::Running,
            "even a clean run stops at the gate — `mission ship` completes the phase, not launch"
        );
        assert_eq!(mission_status_on_disk("gate-test-mission"), MissionStatus::Active);
        assert!(
            crew::lifecycle::load_envelope("gate-test-mission").unwrap().is_none(),
            "no envelope.json on the gated path — finalize_mission never ran"
        );
    }

    #[test]
    #[serial_test::serial]
    fn gate_outcome_worktree_failure_is_a_hard_error() {
        let _guard = LaunchTestGuard::new();
        let phase_id = "gate-test-mission-build";
        seed_running_instance("gate-test-mission", phase_id);
        let (handles, mut steps) =
            scripted_gate_fixture(phase_id, NodeStatus::Error, NodeStatus::Planned, NodeStatus::Planned);
        steps.get_mut(&format!("{phase_id}-worktree-step")).unwrap().output =
            Some("worktree already exists".to_string());

        let err = coder_phase_gate_outcome("gate-test-mission", &handles, &steps).unwrap_err();
        assert!(err.to_string().contains("worktree already exists"), "{err}");
    }

    // ── source_input/ticket hydration (#1284 review round 1, must-fix 2) ─

    #[test]
    #[serial_test::serial]
    fn launch_hydrates_source_input_and_ticket_from_config_extras() {
        let guard = LaunchTestGuard::new();
        guard.write_config(
            "hydration-test",
            r#"{
                "id": "hydration-test",
                "name": "Hydration Test",
                "description": "checks propose-preserved fields land on the mission",
                "source_input": "the operator's original unabridged words",
                "ticket": "SYS-4242",
                "phases": [{"id": "p1", "description": "only phase"}]
            }"#,
        );

        let exit = launch("hydration-test", None, &[], None).unwrap();
        assert_eq!(exit, 0);

        // Zero inputs -> bare config id (must-fix 3).
        let mission = load_mission_for_brief("hydration-test").unwrap();
        assert_eq!(
            mission.source_input.as_deref(),
            Some("the operator's original unabridged words"),
            "source_input must ride config extras onto the mission record (#815)"
        );
        assert_eq!(
            mission.ticket.as_deref(),
            Some("SYS-4242"),
            "ticket must ride config extras onto the mission record (#816)"
        );
    }

    // ── zero-input instance id (#1284 review round 1, must-fix 3) ───────

    #[test]
    fn zero_input_launch_derives_the_bare_config_id() {
        let id = derive_mission_id("draft-blog-post", &BTreeMap::new()).unwrap();
        assert_eq!(
            id, "draft-blog-post",
            "no operator inputs -> the bare config id (a constant hash suffix disambiguates nothing)"
        );
    }

    #[test]
    fn launch_session_id_carries_the_viewer_mission_run_prefix() {
        // (#1284 review round 1, must-fix 4) viewer.html's mission lens keys
        // per-run session grouping on the `mission-run-` prefix.
        let sid = launch_session_id("m1", "m1-build");
        assert_eq!(sid, "mission-run-m1-m1-build");
        assert!(sid.starts_with("mission-run-"));
    }

    // ── pre-mint coder-input check (#1284 review round 1, consider 11) ──

    #[test]
    #[serial_test::serial]
    fn user_config_with_coder_kinds_but_no_inputs_bails_before_minting() {
        let guard = LaunchTestGuard::new();
        // A user-authored config that uses mission.* kinds WITHOUT
        // declaring workdir/branch/base as inputs — the generic
        // required-inputs gate can't catch it; the pre-mint check must.
        guard.write_config(
            "undeclared-coder",
            r#"{
                "id": "undeclared-coder",
                "name": "Undeclared Coder",
                "phases": [{
                    "id": "build",
                    "tasks": [{
                        "id": "build-coder",
                        "steps": [{"id": "build-coder-step", "kind": "mission.coder", "config": null}]
                    }]
                }]
            }"#,
        );

        let err = launch("undeclared-coder", None, &[], None)
            .expect_err("must bail before minting when coder inputs are absent");
        let msg = err.to_string();
        assert!(msg.contains("workdir"), "{msg}");
        assert!(msg.contains("Nothing was minted"), "{msg}");
        assert!(
            !crew::lifecycle::mission_path("undeclared-coder").exists(),
            "the pre-mint check must fire before any instance state lands on disk"
        );
    }

    // ── Pure-function unit coverage (no filesystem) ─────────────────────

    #[test]
    fn collect_inputs_params_win_over_input_file() {
        let mut file = NamedTempFile::new().unwrap();
        write!(file, r#"{{"role":"file-role","base":"main"}}"#).unwrap();
        let collected = collect_inputs(Some(file.path()), &["role=param-role".to_string()]).unwrap();
        assert_eq!(collected.get("role"), Some(&serde_json::json!("param-role")));
        assert_eq!(collected.get("base"), Some(&serde_json::json!("main")));
    }

    #[test]
    fn collect_inputs_rejects_a_param_with_no_equals_sign() {
        let err = collect_inputs(None, &["not-a-kv-pair".to_string()]).unwrap_err();
        assert!(err.to_string().contains("key=value"));
    }

    #[test]
    fn derive_mission_id_is_deterministic_and_charset_safe() {
        let mut m = BTreeMap::new();
        m.insert("workdir".to_string(), serde_json::json!("/tmp/x"));
        let a = derive_mission_id("coder-phase", &m).unwrap();
        let b = derive_mission_id("coder-phase", &m).unwrap();
        assert_eq!(a, b, "same config id + same inputs must derive the same instance id");
        assert!(a.starts_with("coder-phase-"));

        let mut m2 = m.clone();
        m2.insert("workdir".to_string(), serde_json::json!("/tmp/y"));
        let c = derive_mission_id("coder-phase", &m2).unwrap();
        assert_ne!(a, c, "different inputs must derive a different instance id");
    }

    #[test]
    fn missing_required_inputs_excludes_mission_id_and_optional_fields() {
        let input = |name: &str, required: Option<bool>| mission_config::MissionInput {
            name: name.to_string(),
            description: None,
            required,
            extras: BTreeMap::new(),
        };
        let cfg = MissionConfig {
            id: "x".to_string(),
            name: "X".to_string(),
            description: None,
            schema_version: None,
            inputs: vec![
                input("mission_id", Some(true)),
                input("workdir", Some(true)),
                input("image", Some(false)),
            ],
            phases: Vec::new(),
            extras: BTreeMap::new(),
        };
        let missing = missing_required_inputs(&cfg, &BTreeMap::new());
        let names: Vec<&str> = missing.iter().map(|i| i.name.as_str()).collect();
        assert_eq!(names, vec!["workdir"], "mission_id (launcher-supplied) and image (optional) must not appear");
    }

    // ── #1400: lazy phase start ("phase 2 stays planned until reached") ──

    #[test]
    #[serial_test::serial]
    fn lazy_start_phase_for_step_only_starts_the_phase_its_step_belongs_to() {
        let _guard = LaunchTestGuard::new();
        let config: MissionConfig = serde_json::from_str(FREEFORM_CONFIG).unwrap();
        let mission_id = "lazy-start-test";
        let (real_phase_ids, _reused) = ensure_mission_and_phases(mission_id, &config).unwrap();
        let p1 = &real_phase_ids["p1"];
        let p2 = &real_phase_ids["p2"];

        // Both phases start life Planned — mint never eagerly starts
        // anything (this is #1400's headline finding: a 3-phase mission
        // used to show every phase Running from second zero).
        assert_eq!(phase_status_on_disk(mission_id, p1), PhaseStatus::Planned);
        assert_eq!(phase_status_on_disk(mission_id, p2), PhaseStatus::Planned);

        let mut started = std::collections::HashSet::new();
        // A step belonging to p1 flips Running — only p1 starts; p2 (which
        // the scheduler hasn't reached yet) stays Planned.
        lazy_start_phase_for_step(mission_id, p1, NodeStatus::Running, &mut started);
        assert_eq!(phase_status_on_disk(mission_id, p1), PhaseStatus::Running, "p1 starts on its own first step");
        assert_eq!(
            phase_status_on_disk(mission_id, p2),
            PhaseStatus::Planned,
            "p2 must stay Planned until ITS OWN step starts — not pulsed at the same time as p1"
        );

        // A terminal transition for a p1 step is a no-op for phase-start
        // purposes — only a `Running` call can ever start a phase.
        lazy_start_phase_for_step(mission_id, p1, NodeStatus::Complete, &mut started);
        assert_eq!(phase_status_on_disk(mission_id, p1), PhaseStatus::Running);
        assert_eq!(phase_status_on_disk(mission_id, p2), PhaseStatus::Planned);

        // p2 finally gets its own step start — it starts too, independently
        // and later, matching the pipeline-progressing-left-to-right story
        // the graph lens is meant to tell.
        lazy_start_phase_for_step(mission_id, p2, NodeStatus::Running, &mut started);
        assert_eq!(phase_status_on_disk(mission_id, p2), PhaseStatus::Running, "p2 starts on its own first step");
    }

    #[test]
    #[serial_test::serial]
    fn lazy_start_phase_for_step_is_idempotent_for_a_multi_step_phase() {
        let _guard = LaunchTestGuard::new();
        let config: MissionConfig = serde_json::from_str(FREEFORM_CONFIG).unwrap();
        let mission_id = "lazy-start-idempotent";
        let (real_phase_ids, _) = ensure_mission_and_phases(mission_id, &config).unwrap();
        let p1 = &real_phase_ids["p1"];

        let mut started = std::collections::HashSet::new();
        // Two DIFFERENT steps in the SAME phase both flip Running (a
        // multi-task phase) — the second call must not re-attempt
        // `phase_start` (which errors against an already-Running phase);
        // `started` is what prevents the re-attempt, never a second read
        // of the live phase status racing the first call's write.
        lazy_start_phase_for_step(mission_id, p1, NodeStatus::Running, &mut started);
        lazy_start_phase_for_step(mission_id, p1, NodeStatus::Running, &mut started);
        assert_eq!(phase_status_on_disk(mission_id, p1), PhaseStatus::Running);
    }

    /// (#1400 + #1372) The reopen-semantics regression test: a relaunch of
    /// a Closed mission must restart ONLY what reruns. Simulates a prior
    /// partial run — phase 1 finished cleanly (`Complete`), phase 2
    /// errored and was abandoned (`Abandoned`) — then reopens the SAME
    /// mission id and asserts the reopen itself touches NEITHER phase's
    /// status (no eager restart of the abandoned phase — the pre-#1400
    /// bug), while the lazy hook still restarts the abandoned one once its
    /// own step actually begins, and the terminal phase stays untouched
    /// throughout (nothing reruns for it, so nothing calls the hook for
    /// it).
    #[test]
    #[serial_test::serial]
    fn reopen_preserves_terminal_phase_status_and_restarts_only_abandoned_ones_lazily() {
        let _guard = LaunchTestGuard::new();
        let config: MissionConfig = serde_json::from_str(FREEFORM_CONFIG).unwrap();
        let mission_id = "lazy-reopen-test";
        let (real_phase_ids, _) = ensure_mission_and_phases(mission_id, &config).unwrap();
        let p1 = &real_phase_ids["p1"];
        let p2 = &real_phase_ids["p2"];

        crew::lifecycle::phase_start(p1).unwrap();
        crew::lifecycle::phase_complete(p1).unwrap();
        crew::lifecycle::phase_start(p2).unwrap();
        crew::lifecycle::phase_abandon(p2).unwrap();
        crew::lifecycle::mission_close_with_reasoning(mission_id, Some("test close")).unwrap();
        assert_eq!(mission_status_on_disk(mission_id), MissionStatus::Closed);

        // Relaunch (reopen) the SAME mission id.
        let (real_phase_ids2, reused) = ensure_mission_and_phases(mission_id, &config).unwrap();
        assert!(reused);
        assert_eq!(real_phase_ids2, real_phase_ids);
        assert_eq!(mission_status_on_disk(mission_id), MissionStatus::Active, "reopen reactivates the mission");

        // (#1400) Preserved #1372 semantics: reopen restarts only what
        // reruns. p1 (terminal Complete) is untouched; p2 (Abandoned) is
        // ALSO untouched AT REOPEN TIME — not eagerly flipped to Running —
        // only the scheduler's own lazy hook, once it actually reaches
        // p2's first step, does that.
        assert_eq!(
            phase_status_on_disk(mission_id, p1),
            PhaseStatus::Complete,
            "a terminal phase is never touched by reopen"
        );
        assert_eq!(
            phase_status_on_disk(mission_id, p2),
            PhaseStatus::Abandoned,
            "reopen must NOT eagerly restart an abandoned phase — that was the #1400 bug"
        );

        // The lazy hook restarts p2 once its own step actually begins; p1
        // is never called (its graph doesn't rerun), proving the restart
        // stays scoped to "only what reruns."
        let mut started = std::collections::HashSet::new();
        lazy_start_phase_for_step(mission_id, p2, NodeStatus::Running, &mut started);
        assert_eq!(
            phase_status_on_disk(mission_id, p2),
            PhaseStatus::Running,
            "the lazy hook restarts an Abandoned phase on reach"
        );
        assert!(
            load_phase_for_brief(mission_id, p2).unwrap().abandoned_ts.is_none(),
            "restart clears abandoned_ts, matching phase_start's own convention"
        );
        assert_eq!(
            phase_status_on_disk(mission_id, p1),
            PhaseStatus::Complete,
            "p1 stays untouched since nothing reruns for it"
        );
    }

    // ── (#1406) Honest finalize: per-phase outcomes from step statuses ──

    /// A single-step `Task` bound to `phase_real_id`, whose one step is
    /// `step_id`: the minimal shape [`derive_phase_outcomes`] /
    /// [`build_envelope`] read.
    fn task_with_step(phase_real_id: &str, step_id: &str) -> crew::types::Task {
        crew::types::Task {
            id: format!("{phase_real_id}-task"),
            phase_id: phase_real_id.to_string(),
            description: "t".to_string(),
            display_name: None,
            step_ids: vec![step_id.to_string()],
            depends_on: Vec::new(),
            role_id: None,
            profile_name: None,
            workdir: None,
            image: None,
        }
    }

    /// Seed an Active mission on disk with the named phases at the given
    /// statuses, so a finalize/reconcile test can assert the on-disk end
    /// state agrees with the envelope.
    fn seed_mission_with_phases(mission_id: &str, phases: &[(&str, PhaseStatus)]) {
        let now = 1_700_000_000u64;
        let mission = Mission {
            id: mission_id.to_string(),
            description: "1406 test".to_string(),
            status: MissionStatus::Active,
            phase_ids: phases.iter().map(|(id, _)| id.to_string()).collect(),
            created_ts: now,
            started_ts: Some(now),
            closed_ts: None,
            paused_ts: None,
            source_input: None,
            ticket: None,
        };
        crew::lifecycle::save_mission(&mission).unwrap();
        for (id, status) in phases {
            let mut p = new_planned_phase(mission_id, id, Some("phase"), None, now);
            p.status = *status;
            if matches!(status, PhaseStatus::Running | PhaseStatus::Complete) {
                p.started_ts = Some(now);
            }
            crew::lifecycle::save_phase(&p).unwrap();
        }
    }

    const GEN3_CONFIG: &str = r#"{"id":"gen3","name":"gen3","phases":[{"id":"p1"},{"id":"p2"},{"id":"p3"}]}"#;

    #[test]
    #[serial_test::serial]
    fn build_envelope_derives_honest_per_phase_outcomes_the_1406_scenario() {
        // (#1406) The issue's exact scenario: a 3-phase gate-less generic
        // mission where phase 1 completes, phase 2's step errors, and phase 3
        // is never reached. The retired uniform mapping marked EVERY phase
        // Complete on the Degraded run (the bug); the honest derivation reads
        // each phase's OWN steps.
        let config: MissionConfig = serde_json::from_str(GEN3_CONFIG).unwrap();
        let mid = "gen3";
        let real = derive_phase_ids(mid, &config);
        let (rp1, rp2, rp3) = (real["p1"].clone(), real["p2"].clone(), real["p3"].clone());

        let tasks =
            vec![task_with_step(&rp1, "p1-step"), task_with_step(&rp2, "p2-step"), task_with_step(&rp3, "p3-step")];
        let mut steps = BTreeMap::new();
        steps.insert("p1-step".to_string(), scripted_step("p1-step", NodeStatus::Complete));
        steps.insert("p2-step".to_string(), scripted_step("p2-step", NodeStatus::Error));
        steps.insert("p3-step".to_string(), scripted_step("p3-step", NodeStatus::Planned));

        let env = build_envelope(mid, &config, &real, &tasks, &steps);
        use crew::envelope::{MissionOutcomeStatus, PhaseOutcomeKind};
        assert_eq!(env.status, MissionOutcomeStatus::Degraded, "some complete + some errored → Degraded");

        let outcome = |pid: &str| env.phases.iter().find(|p| p.phase_id == pid).map(|p| p.outcome);
        assert_eq!(outcome(&rp1), Some(PhaseOutcomeKind::Complete), "p1's steps all completed → Complete");
        assert_eq!(outcome(&rp2), Some(PhaseOutcomeKind::Abandoned), "p2 has an errored step → Abandoned, never Complete");
        assert_eq!(outcome(&rp3), Some(PhaseOutcomeKind::Abandoned), "p3 never started → Abandoned, never Complete");
    }

    #[test]
    #[serial_test::serial]
    fn build_envelope_clean_run_completes_every_phase_unchanged() {
        // (#1406) A Clean run is unaffected by the per-phase derivation:
        // every phase's steps all completed, so every phase reads Complete,
        // identical to the retired uniform mapping's result for a clean run.
        let config: MissionConfig = serde_json::from_str(GEN3_CONFIG).unwrap();
        let mid = "gen3clean";
        let real = derive_phase_ids(mid, &config);
        let (rp1, rp2, rp3) = (real["p1"].clone(), real["p2"].clone(), real["p3"].clone());
        let tasks =
            vec![task_with_step(&rp1, "p1-step"), task_with_step(&rp2, "p2-step"), task_with_step(&rp3, "p3-step")];
        let mut steps = BTreeMap::new();
        for sid in ["p1-step", "p2-step", "p3-step"] {
            steps.insert(sid.to_string(), scripted_step(sid, NodeStatus::Complete));
        }
        let env = build_envelope(mid, &config, &real, &tasks, &steps);
        use crew::envelope::{MissionOutcomeStatus, PhaseOutcomeKind};
        assert_eq!(env.status, MissionOutcomeStatus::Clean);
        assert_eq!(env.phases.len(), 3);
        assert!(env.phases.iter().all(|p| p.outcome == PhaseOutcomeKind::Complete), "every phase completes on a clean run");
    }

    #[test]
    #[serial_test::serial]
    fn finalize_the_1406_scenario_agrees_with_disk() {
        // (#1406) End to end: build the honest envelope for the issue's
        // scenario and finalize it against seeded disk state, asserting the
        // mission Closes with p1 Complete, p2 terminal-not-complete, p3
        // Abandoned (no phase left Planned inside a Closed mission), and the
        // persisted envelope.json agrees with the phase files.
        let _guard = LaunchTestGuard::new();
        let config: MissionConfig = serde_json::from_str(GEN3_CONFIG).unwrap();
        let mid = "gen3";
        let real = derive_phase_ids(mid, &config);
        let (rp1, rp2, rp3) = (real["p1"].clone(), real["p2"].clone(), real["p3"].clone());

        // The lazy-start end state for the scenario: p1 finished (Running,
        // steps complete), p2's step errored (Running), p3 never started
        // (Planned).
        seed_mission_with_phases(
            mid,
            &[(&rp1, PhaseStatus::Running), (&rp2, PhaseStatus::Running), (&rp3, PhaseStatus::Planned)],
        );
        let tasks =
            vec![task_with_step(&rp1, "p1-step"), task_with_step(&rp2, "p2-step"), task_with_step(&rp3, "p3-step")];
        let mut steps = BTreeMap::new();
        steps.insert("p1-step".to_string(), scripted_step("p1-step", NodeStatus::Complete));
        steps.insert("p2-step".to_string(), scripted_step("p2-step", NodeStatus::Error));
        steps.insert("p3-step".to_string(), scripted_step("p3-step", NodeStatus::Planned));

        let env = build_envelope(mid, &config, &real, &tasks, &steps);
        crew::envelope::finalize_mission(&env);

        assert_eq!(phase_status_on_disk(mid, &rp1), PhaseStatus::Complete);
        assert_eq!(phase_status_on_disk(mid, &rp2), PhaseStatus::Abandoned, "an errored phase abandons, never completes");
        assert_ne!(
            phase_status_on_disk(mid, &rp3),
            PhaseStatus::Planned,
            "p3 must NEVER be left Planned inside a Closed mission (the #1406 bug)"
        );
        assert_eq!(phase_status_on_disk(mid, &rp3), PhaseStatus::Abandoned);
        assert_eq!(mission_status_on_disk(mid), MissionStatus::Closed);

        // Envelope-on-disk agrees with the phase files.
        use crew::envelope::PhaseOutcomeKind;
        let persisted = crew::lifecycle::load_envelope(mid).unwrap().expect("envelope.json persisted");
        let outcome = |pid: &str| persisted.phases.iter().find(|p| p.phase_id == pid).map(|p| p.outcome);
        assert_eq!(outcome(&rp1), Some(PhaseOutcomeKind::Complete));
        assert_eq!(outcome(&rp2), Some(PhaseOutcomeKind::Abandoned));
        assert_eq!(outcome(&rp3), Some(PhaseOutcomeKind::Abandoned));
    }

    #[test]
    #[serial_test::serial]
    fn reconcile_and_finalize_on_error_flips_running_steps_and_terminalizes_mission() {
        // (#1406, F4) A scheduler-level Err mid-run leaves steps persisted as
        // Running and the mission Active forever. The reconcile flips the
        // still-Running step to Error, persists it, and finalizes the mission
        // to a terminal Error status with honest per-phase outcomes.
        let _guard = LaunchTestGuard::new();
        let config: MissionConfig = serde_json::from_str(GEN3_CONFIG).unwrap();
        let mid = "gen3err";
        let real = derive_phase_ids(mid, &config);
        let (rp1, rp2, rp3) = (real["p1"].clone(), real["p2"].clone(), real["p3"].clone());

        // p1 done (Running phase, step Complete), p2's step mid-dispatch
        // (Running) when the scheduler Err'd, p3 never reached (Planned).
        seed_mission_with_phases(
            mid,
            &[(&rp1, PhaseStatus::Running), (&rp2, PhaseStatus::Running), (&rp3, PhaseStatus::Planned)],
        );
        let tasks =
            vec![task_with_step(&rp1, "p1-step"), task_with_step(&rp2, "p2-step"), task_with_step(&rp3, "p3-step")];
        let mut steps = BTreeMap::new();
        steps.insert("p1-step".to_string(), scripted_step("p1-step", NodeStatus::Complete));
        steps.insert("p2-step".to_string(), scripted_step("p2-step", NodeStatus::Running));
        steps.insert("p3-step".to_string(), scripted_step("p3-step", NodeStatus::Planned));

        let err = anyhow::anyhow!("step kind `mission.bogus` is not registered");
        reconcile_and_finalize_on_error(mid, &config, &real, &tasks, &mut steps, &err);

        // The mid-run Running step flipped to Error, in memory AND on disk;
        // no step is stranded Running.
        assert_eq!(steps["p2-step"].status, NodeStatus::Error, "the Running step flips to Error in memory");
        assert_eq!(
            crew::lifecycle::load_step(mid, &rp2, "p2-step").unwrap().status,
            NodeStatus::Error,
            "the flip is persisted; no Running step survives the failure path"
        );

        // The mission reaches a terminal status with honest per-phase
        // outcomes (p1 completed before the failure; p2 interrupted; p3 never
        // reached).
        assert_eq!(mission_status_on_disk(mid), MissionStatus::Closed, "the failed run is no longer stranded Active");
        assert_eq!(phase_status_on_disk(mid, &rp1), PhaseStatus::Complete);
        assert_eq!(phase_status_on_disk(mid, &rp2), PhaseStatus::Abandoned);
        assert_eq!(phase_status_on_disk(mid, &rp3), PhaseStatus::Abandoned);

        use crew::envelope::MissionOutcomeStatus;
        let persisted = crew::lifecycle::load_envelope(mid).unwrap().expect("envelope.json persisted");
        assert_eq!(persisted.status, MissionOutcomeStatus::Error, "a hard scheduler Err finalizes to Error status");
    }
}
