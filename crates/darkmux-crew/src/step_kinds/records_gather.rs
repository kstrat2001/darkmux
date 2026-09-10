//! `records.gather` (#2310 P4c-2b) — this mission's finding + mod records,
//! plus a diff and a scope summary, gathered into the typed shape
//! [`deliver_github_review::DeliverConfig`] reads (`DeliverConfig::
//! from_step`, when `Step.config` carries none of `findings`/`mods`/`diff`/
//! `scope` itself, falls back to a [`GatherOutput`] found on the run's
//! artifact bus — see that module's own doc for the wiring).
//!
//! **Mission-agnostic by construction**, same discipline
//! `deliver_github_review`'s own module doc states: this reads the SHARED
//! finding/mod stores (`crate::findings`, `crate::mods`), scoped by mission
//! id the way `finding list --mission`/`mod list --mission` resolve it
//! (`src/finding_cli.rs`'s `list`: `r.mission_id.as_deref() == Some(m)`;
//! `src/mod_cli.rs`'s `list`: `mods::names_mission(m, x)`) — never a crawl
//! or review type. A crawl's own `create-mods` phase could grow a
//! `records.gather` → a different delivery kind exactly the same way.
//!
//! **Tier 1 (#1352).** Reads config, resolves the mission id off the Task's
//! own phase record (the same trick `darkmux-lab`'s
//! `crawl::unit_step::mission_id_for` uses, duplicated here in miniature
//! because this crate has no dependency on that one — see
//! `deliver_github_review`'s own doc on why `darkmux-crew` never depends
//! on `darkmux-lab`), computes a scope summary from what is on disk, and
//! writes one typed envelope. No model dispatch, no per-mission control
//! flow of its own.
//!
//! **Scope summary, honestly bounded.** (#2361 item 2) `rules_run` and
//! `hunks_covered` count only what a COMPLETED `crawl.unit` step
//! reviewed — a plan is an intention, and a run whose units errored
//! covered nothing. Before that fix, four errored units out of five still
//! reported "5 rule(s), 5/5 hunks covered", which is a false claim of
//! coverage on a payload an operator acts on. `rules_run` is therefore
//! every rule with at least one completed unit among the
//! `plan/<rule>.json` files this mission wrote under `<missions_dir>/
//! <mission-id>/plan/`; `hunks_covered` is the count of DISTINCT `(file,
//! site start)` windows those plans found, capped at `hunks_total` (#2310
//! P4c-2b PR #2357 review CONSIDER E — summing `totals.units` ACROSS
//! rules used to double-count the same hunk once per rule that planned
//! it). `hunks_total` comes from parsing the diff itself with the shared
//! `crate::diff::parse_diff`. `refused` is the RUNTIME-BOUNDARY rejection
//! count (#2310 P4c-2b PR #2357 review CONSIDER F) — `crawl.unit`'s own
//! `findings_rejected`, summed off this mission's Step records
//! (`scan_unit_and_plan_steps`, read as loose JSON — never `darkmux-lab`'s
//! typed `UnitOutcome`, same crate-boundary reason) — NEVER a gate-failed
//! mod (that renders as a double-check thread, DESIGN.md's own
//! vocabulary; counting it as "refused" too would double-book one fact
//! under two names). `not_attempted`/`errored` (#2310 P4c-2b PR #2357
//! review MUST FIX C/D) come from the SAME scan: a rule whose plan step
//! never reached `Complete`, or a unit that errored/was abandoned.

use crate::findings::{self, FindingRecord};
use crate::mods;
use crate::step_kinds::deliver_github_review::{DeliverScope, GatedMod};
use crate::step_kinds::registry::StepKindRegistry;
use crate::step_kinds::types::{CwdPolicy, Port, SeatClaim, StepKind, StepOutcome, StepRunCtx};
use crate::types::{Step, Task};
use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::sync::Arc;

pub const RECORDS_GATHER_KIND: &str = "records.gather";

/// Content id both the step's own `provides()` port and the envelope's
/// `kind` use — same one-name-for-both-roles convention `crawl.summary`
/// establishes (`CRAWL_SUMMARY_KIND == CRAWL_SUMMARY_OUTPUT_KIND`).
pub const RECORDS_GATHER_OUTPUT_KIND: &str = "records.gather";

pub const GATHER_OUTPUT_SCHEMA_VERSION: &str = "1.0";

/// Local literal duplicate of `darkmux_lab::crawl::unit_step::
/// CRAWL_UNIT_KIND`. `darkmux-crew` cannot depend on `darkmux-lab` (see
/// `scan_unit_and_plan_steps`'s own doc), so this crate keeps its own
/// copy of the string; a conformance test in `src/mission_launch.rs`
/// (which depends on both crates) asserts the two literals stay equal and
/// that both resolve against `all_step_kinds`'s real registry.
pub const SCANNED_CRAWL_UNIT_KIND: &str = "crawl.unit";
/// Local literal duplicate of `darkmux_lab::crawl::plan_sites_step::
/// PLAN_SITES_KIND`. See [`SCANNED_CRAWL_UNIT_KIND`].
pub const SCANNED_PLAN_SITES_KIND: &str = "plan.sites";
/// Local literal duplicate of `darkmux_lab::crawl::plan_step::
/// CRAWL_PLAN_KIND`. See [`SCANNED_CRAWL_UNIT_KIND`].
pub const SCANNED_CRAWL_PLAN_KIND: &str = "crawl.plan";

/// What [`RecordsGatherStepKind`] produces — everything
/// `deliver_github_review::render_github_review` needs, gathered from this
/// mission's own stores.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GatherOutput {
    pub schema_version: String,
    pub findings: Vec<FindingRecord>,
    pub mods: Vec<GatedMod>,
    pub diff: String,
    pub scope: DeliverScope,
    /// (silent-miss audit, 2026-09-06) What [`scan_unit_and_plan_steps`]
    /// could not read while building `scope` — a failed `load_phases`,
    /// a failed `load_steps_for_phase` for one phase, or a `crawl.unit`
    /// step whose own output failed to parse. Each of these previously
    /// vanished into a default/skip/zero with no trace; see
    /// [`StepScan::unreadable`]. Deliberately NOT a field on
    /// [`DeliverScope`] (`deliver_github_review.rs` is owned by another
    /// concurrent change and is not touched by this fix) — sits beside
    /// `scope` on the envelope instead. **Follow-up STILL owed** (#1748
    /// review CONSIDER 9): wire this into `render_github_review`'s
    /// scope-line rendering (or an adjacent line) so an unreadable input
    /// is visible on the PR comment itself, not only in the raw envelope.
    /// #1748's own fix pass DID touch `deliver_github_review.rs`, but its
    /// scope was the absence-claim backstop's containment (`code_span`
    /// on the caveat's `token`/`file`) and the token-binding fix
    /// ([`crate::absence_backstop::detect_absence_claim`]) — wiring this
    /// field in is a genuinely separate change (a new
    /// `render_github_review` parameter, touching every call site in
    /// that module including its ~30 test callers) that deserves its own
    /// pass rather than riding along here. Recorded explicitly, again,
    /// so the NEXT touch of this file does not have to rediscover that
    /// the obligation is still open.
    #[serde(default)]
    pub unreadable: Vec<String>,
    /// (#1748) The mechanical absence-claim backstop's findings — one
    /// [`crate::absence_backstop::AbsenceBackstopNote`] per finding KEY
    /// whose "X is missing"/"X is never called" claim the backstop
    /// CONTRADICTED against the whole file. Additive (`#[serde(default)]`),
    /// so an older `GatherOutput` on disk simply has none. Never removes a
    /// finding from [`Self::findings`] or shrinks its count — see
    /// [`crate::absence_backstop::run_backstop`]'s own doc for why a
    /// finding this check cannot evaluate is absent from the map rather
    /// than flagged either way.
    #[serde(default)]
    pub absence_backstop: BTreeMap<String, crate::absence_backstop::AbsenceBackstopNote>,
}

pub struct RecordsGatherStepKind;

impl StepKind for RecordsGatherStepKind {
    /// (#2394) [`SeatClaim::NoModel`] — this kind reads records out of the flow store; it
    /// dispatches nothing. Bounded by `runtime.dispatch_free_concurrency`
    /// and, per command, by `runtime.step_command_timeout_seconds` — never
    /// by the hosted-endpoint cap.
    fn seat(
        &self,
        _step: &Step,
        _task: &Task,
        _input: &std::collections::BTreeMap<String, String>,
        _ctx: &StepRunCtx,
    ) -> SeatClaim {
        SeatClaim::NoModel
    }

    /// (#1511) `None` — the documented no-dispatch opt-out, matching this
    /// kind's [`SeatClaim::NoModel`] above: it gathers darkmux records off disk
    /// and speaks to no model, so there is no role for the
    /// licensed-adjacent consent gate to check.
    fn dispatch_role(
        &self,
        _step: &Step,
        _task: &Task,
        _input: &std::collections::BTreeMap<String, String>,
        _ctx: &StepRunCtx,
    ) -> Option<String> {
        None
    }

    fn id(&self) -> &'static str {
        RECORDS_GATHER_KIND
    }

    fn display_name(&self) -> &'static str {
        "Gather records"
    }

    fn provides(&self) -> &'static [Port] {
        const PORTS: [Port; 1] = [Port::data(RECORDS_GATHER_OUTPUT_KIND)];
        &PORTS
    }

    /// (#1979) No model work, no dispatch session — same opt-out
    /// `deliver.github_review`/`procedural.shell`/`procedural.noop` use.
    fn dispatch_session_id(&self, _step: &Step) -> Option<String> {
        None
    }

    /// (#2577 audit) `CwdPolicy::NoAmbientDependency` (the trait default,
    /// stated explicitly here) — this kind spawns no subprocess at all: it
    /// reads darkmux's own flow/findings/mods records off disk through
    /// their own resolved paths (`findings`/`mods`/flow-store readers),
    /// never a `Command`. Was previously covered only by the trait
    /// default (silently, with no row naming this a checked audit) — a
    /// #2577-review finding. Not a `StepKindRegistry::with_builtins()`
    /// member, so the registry conformance test cannot see this kind —
    /// audited by hand.
    fn cwd_policy(&self) -> CwdPolicy {
        CwdPolicy::NoAmbientDependency
    }

    fn run(&self, step: &Step, task: &Task, _input: &BTreeMap<String, String>) -> Result<StepOutcome> {
        let mission_id = mission_id_for(task)?;

        let diff = match step.config.get("diff_file").and_then(|v| v.as_str()) {
            Some(p) if !p.trim().is_empty() => std::fs::read_to_string(p)
                .with_context(|| format!("step `{}`: `{RECORDS_GATHER_KIND}` reading config.diff_file {p}", step.id))?,
            _ => String::new(),
        };
        let mut not_attempted: Vec<String> = step
            .config
            .get("not_attempted")
            .and_then(|v| v.as_array())
            .map(|a| a.iter().filter_map(|x| x.as_str().map(str::to_string)).collect())
            .unwrap_or_default();

        let findings: Vec<FindingRecord> = findings::load_all_at(&findings::findings_dir())
            .context("loading the finding store")?
            .into_iter()
            .filter(|f| f.mission_id.as_deref() == Some(mission_id.as_str()))
            .collect();

        let mods: Vec<GatedMod> = mods::load_all_at(&mods::mods_dir())
            .context("loading the mod store")?
            .into_iter()
            .filter(|m| mods::names_mission(m, &mission_id))
            .map(|record| {
                let gate_passed = record.gate.as_ref().map(|g| g.passed);
                GatedMod { record, gate_passed }
            })
            .collect();

        let hunks_total: usize = crate::diff::parse_diff(&diff).iter().map(|(_, hunks)| hunks.len()).sum();

        // (#2310 P4c-2b PR #2357 review CONSIDER F, MUST FIX C/D) `refused`
        // is the RUNTIME-BOUNDARY rejection count (`crawl.unit`'s own
        // `findings_rejected` — a `create_finding` call the runtime itself
        // rejected, before a finding ever became a stored record), never
        // a gate-failed mod (that is a DOUBLE-CHECK thread, DESIGN.md's
        // own vocabulary — `render_github_review`'s `DeliveryForm::Mod`
        // arm already renders it as one; counting it as "refused" too
        // would double-book the same fact under two names).
        // `not_attempted`/`errored` come from this mission's own
        // plan/unit step records — a rule whose plan step errored, or a
        // unit that never converged, both surface here (MUST FIX C/D).
        let scan = scan_unit_and_plan_steps(&mission_id, &task.id);
        // (#2361 item 2) Coverage counts what a COMPLETED unit reviewed —
        // never what a plan intended. See `plan_totals`.
        let (rules_run, hunks_covered) = plan_totals(&mission_id, hunks_total, &scan.completed_units);
        not_attempted.extend(scan.not_attempted);
        // (#2310 fix-loop E2, S1-6) The rules the run never MINTED a task
        // for — see `pruned_rules`.
        let declared = declared_rules(&mission_id);
        not_attempted.extend(pruned_rules(&mission_id, &declared));
        // (F2, from #2374's review) The denominator is DISTINCT RULE IDS,
        // not rule-bearing TASKS. `declared` is keyed by task id, and a
        // config routinely declares more than one task for one rule —
        // `crawl.json`'s own shape is a `crawl.plan` task per rule AND a
        // `crawl.unit` grow template per rule — so `declared.len()` made M
        // the task count and rendered a single-rule run as "0 of 2 rules
        // reviewed". The numerator (`rules_run`, off the `plan/<rule>.json`
        // file set) was already per-rule, so the two halves of the same
        // sentence were counting different things.
        let rules_total = declared.values().collect::<std::collections::BTreeSet<_>>().len();
        not_attempted.sort();
        not_attempted.dedup();

        let scope = DeliverScope {
            rules_run,
            // (#2310 fix-loop E2, S1-6) The DENOMINATOR the scope line's
            // "N of M rules reviewed" needs, taken from the run's own
            // config snapshot — the one artifact that still names a rule
            // whose task was pruned before the mint. `0` when the snapshot
            // is unreadable, which `DeliverScope::rules_declared` reads as
            // "fall back to what the lists prove".
            rules_total,
            hunks_covered,
            hunks_total,
            refused: scan.findings_rejected,
            not_attempted,
            errored: scan.errored,
        };
        // (#1748) The mechanical absence-claim backstop — checks every
        // finding's own "X is missing"/"X is never called" claim against
        // the WHOLE FILE `plan.sites` checked out, not just the hunk the
        // reviewing seat was shown. Additive: never removes a finding
        // from `findings` above or changes its count, only annotates the
        // ones it CONTRADICTS — see `absence_backstop::run_backstop`'s own
        // doc.
        let absence_backstop = crate::absence_backstop::run_backstop(&mission_id, &findings);
        let out = GatherOutput {
            schema_version: GATHER_OUTPUT_SCHEMA_VERSION.to_string(),
            findings,
            mods,
            diff,
            scope,
            unreadable: scan.unreadable,
            absence_backstop,
        };
        let wrapped = crate::step_output::Output::wrap(
            RECORDS_GATHER_OUTPUT_KIND,
            out,
            crate::step_output::Producer::of(&mission_id, &task.id, &step.id),
        );
        Ok(StepOutcome { output: wrapped.to_output_string()?, flow_records: Vec::new() })
    }
}

/// The mission id a Task's own phase record names — the same lookup
/// `darkmux-lab`'s `crawl::unit_step::mission_id_for` performs, duplicated
/// here because `darkmux-crew` cannot depend on `darkmux-lab` (this
/// module's own doc).
fn mission_id_for(task: &Task) -> Result<String> {
    let phases = crate::loader::load_phases().context("loading phase records to locate the run")?;
    phases
        .iter()
        .find(|p| p.id == task.phase_id)
        .map(|p| p.mission_id.clone())
        .ok_or_else(|| {
            anyhow!(
                "records.gather: task `{}` names phase `{}`, which has no record — the run cannot be located",
                task.id,
                task.phase_id
            )
        })
}

/// (#2310 fix-loop E2, S1-6) Every rule this run's CONFIG declared, mapped
/// from the document task that declares it: `task id -> rule id`.
///
/// Read from the config SNAPSHOT (`lifecycle::load_config_snapshot`), which
/// is the as-declared document with its `enabled` flags intact — deliberately
/// not the pruned one, because the whole point is to see the tasks that were
/// pruned away. A rule id is the `rule` key on any of the task's step configs,
/// the same definition `mission_launch::task_declares_rule` uses for
/// `--param rules=` selection (duplicated here rather than shared: that
/// function lives in the binary crate, which this library cannot depend on).
///
/// An unreadable/absent snapshot yields an empty map — descriptive, never an
/// error, the same posture `plan_totals`/`scan_unit_and_plan_steps` take.
fn declared_rules(mission_id: &str) -> std::collections::BTreeMap<String, String> {
    let mut out = std::collections::BTreeMap::new();
    let Ok(Some(config)) = crate::lifecycle::load_config_snapshot(mission_id) else { return out };
    for phase in &config.phases {
        for task in &phase.tasks {
            if let Some(rule) = task.steps.iter().find_map(|s| s.config.get("rule").and_then(|v| v.as_str())) {
                out.insert(task.id.clone(), rule.to_string());
            }
        }
    }
    out
}

/// (#2310 fix-loop E2, S1-6) Rules whose task was PRUNED before the mint —
/// `enabled: false` in the document, or deselected for this launch by
/// `--param rules=<csv>`.
///
/// These are the rules `scan_unit_and_plan_steps` structurally cannot see: a
/// pruned task mints no `Task`, no `Step` and no `plan/<rule>.json`, so from
/// every on-disk record the run looks as though those rules were never part
/// of it. That is precisely the shape DESIGN.md's "The honest limit" is about
/// — a run narrowed to 2 of 7 rules read exactly like a complete 2-rule
/// review, on a payload an operator acts on.
///
/// `graph-report.json` (`PruneReport::pruned`) is the one record that
/// survives the prune, and it names the TASK id; `declared` maps that back to
/// the rule. A pruned entry naming no rule-bearing task (a phase, a step, a
/// task with no `rule` key at all — the crawl's own `summary`) contributes
/// nothing: this list is about rules, and inventing an entry for a
/// non-rule task would put a task id in a rules sentence.
fn pruned_rules(mission_id: &str, declared: &std::collections::BTreeMap<String, String>) -> Vec<String> {
    let Ok(Some(report)) = crate::lifecycle::load_graph_report(mission_id) else { return Vec::new() };
    report.pruned.iter().filter_map(|p| declared.get(&p.id).cloned()).collect()
}

/// `(rules_run, hunks_covered)` from every `plan/<rule>.json` this mission
/// wrote — the rule id from the filename stem (a plan task that was pruned
/// or never minted writes no file, so the file set alone answers "which
/// plan tasks minted"). A directory that doesn't exist (no plan phase ran)
/// yields `(vec![], 0)`, not an error — a descriptive summary, same
/// tolerance `crawl::unit_step::plan_totals` extends to a missing/
/// unreadable plan file.
///
/// (#2310 P4c-2b PR #2357 review CONSIDER E, proven nonsensical) Summing
/// `totals.units` ACROSS rules double(-N)-counted: several rules in
/// review's own catalog (`existing-solution`/`union-vs-enum`/
/// `shared-symbol-callers`, all `applies_to: []`, no `prefilter`) plan a
/// site over EVERY hunk in the diff, not just their own targeted one — so
/// 5 rules covering the SAME 1-hunk diff summed to "5 covered" against a
/// diff with exactly 1 hunk, a number bigger than the diff itself. Fixed
/// here to count DISTINCT `(file, site start line)` windows across every
/// rule's plan (read as loose JSON — see this module's own doc on why not
/// the typed `Plan`), capped by never exceeding `hunks_total` (the diff's
/// own true hunk count, computed separately from `crate::diff::parse_diff`)
/// — a plan can find fewer windows than the diff has hunks, never more.
fn plan_totals(
    mission_id: &str,
    hunks_total: usize,
    completed_units: &std::collections::BTreeSet<(String, String)>,
) -> (Vec<String>, usize) {
    let plan_dir = crate::loader::missions_dir().join(mission_id).join("plan");
    let mut rules: Vec<String> = Vec::new();
    let mut covered: std::collections::BTreeSet<(String, u64)> = std::collections::BTreeSet::new();
    let Ok(entries) = std::fs::read_dir(&plan_dir) else {
        return (rules, 0);
    };
    let mut paths: Vec<std::path::PathBuf> = entries.filter_map(|e| e.ok()).map(|e| e.path()).collect();
    paths.sort();
    for path in paths.iter().filter(|p| p.extension().is_some_and(|e| e == "json")) {
        let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else { continue };
        let Ok(raw) = std::fs::read_to_string(path) else { continue };
        let Ok(value) = serde_json::from_str::<serde_json::Value>(&raw) else { continue };
        let Some(units) = value.pointer("/body/units").and_then(|v| v.as_array()) else { continue };
        let mut reviewed_anything = false;
        for unit in units {
            // (#2361 item 2) A unit whose step did not COMPLETE reviewed
            // nothing: its windows are not covered, and a rule with no
            // completed unit did not run. A unit id that matches nothing —
            // a step whose config named no unit, a plan whose ids the run
            // never grew — is likewise not counted, which keeps the number
            // an undercount at worst and never a false claim.
            let unit_id = unit.get("id").and_then(|v| v.as_str()).unwrap_or_default();
            if !completed_units.contains(&(stem.to_string(), unit_id.to_string())) {
                continue;
            }
            reviewed_anything = true;
            let Some(sites) = unit.get("sites").and_then(|v| v.as_array()) else { continue };
            for site in sites {
                let (Some(file), Some(start)) =
                    (site.get("file").and_then(|v| v.as_str()), site.get("start").and_then(|v| v.as_u64()))
                else {
                    continue;
                };
                covered.insert((file.to_string(), start));
            }
        }
        if reviewed_anything {
            rules.push(stem.to_string());
        }
    }
    // Capped at `hunks_total`: a plan can find fewer distinct windows than
    // the diff has hunks (a rule's own `applies_to`/`exclude` narrows it),
    // never more. `hunks_total == 0` (no `diff_file` given) legitimately
    // caps coverage at 0 too — nothing to cover without a diff.
    (rules, covered.len().min(hunks_total))
}

/// What [`scan_unit_and_plan_steps`] found, scanning this mission's own
/// `Task`/`Step` records — the same records `crawl.summary` reads for its
/// OWN totals (`darkmux-lab`'s `summarize_mission`), duplicated in
/// miniature here because `darkmux-crew` cannot depend on `darkmux-lab`
/// (this module's own doc).
#[derive(Debug, Clone, Default)]
struct StepScan {
    /// Sum of every `crawl.unit` step's own `findings_rejected` — a
    /// runtime-boundary rejection (`create_finding` refused before it
    /// ever became a stored finding), read generically off the step's
    /// `UnitOutcome`-shaped JSON output (never the typed struct itself —
    /// same crate-boundary rule `plan_totals` follows for `Plan`).
    findings_rejected: usize,
    /// Rule ids whose `plan.sites`/`crawl.plan` step did NOT reach
    /// `Complete` — never even attempted.
    not_attempted: Vec<String>,
    /// Human-readable names of `crawl.unit`/`plan.sites`/`crawl.plan`
    /// steps that ended `Error`/`Abandoned` this run.
    errored: Vec<String>,
    /// (#2361 item 2) `(rule, unit id)` for every `crawl.unit` step that
    /// reached `Complete` — the ONLY units whose planned windows count as
    /// covered. Both halves come off the step's own config, which the
    /// grown task stamps (`rule`/`unit` in `review.json`'s and
    /// `crawl.json`'s `grow.config`).
    completed_units: std::collections::BTreeSet<(String, String)>,
    /// (silent-miss audit, 2026-09-06) Every place this scan could not
    /// read something it needed and previously swallowed the `Err` into a
    /// default/skip/clean outcome: `load_phases()` failing (the whole scan
    /// returns empty — no rule, unit, or rejection is ever named), a
    /// per-phase `load_steps_for_phase` failing (that phase's steps are
    /// invisible to every count above), and a `crawl.unit` step's own
    /// output failing to parse (`findings_rejected` reads as `0` — a
    /// parse failure and a genuinely clean unit are indistinguishable
    /// without this). Each entry names what was unreadable and why, so a
    /// gather that silently undercounts everything is at least named as
    /// having done so, rather than rendering as a clean, fully-scanned
    /// run.
    unreadable: Vec<String>,
}

/// (#2454) `darkmux_lab::crawl::unit_step::THERMAL_STOP` — a local literal
/// duplicate for the same crate-boundary reason as the `SCANNED_*` kind ids
/// above (`darkmux-crew` cannot depend on `darkmux-lab`), and pinned against
/// the real constant by the same conformance test in `src/mission_launch.rs`.
/// Drift here is silent and expensive in one direction: if this string stops
/// matching, every thermally-skipped unit goes back to counting as reviewed
/// coverage.
pub const UNIT_RESULT_THERMAL_STOP: &str = "thermal_stop";

/// (#2310 P4c-2b PR #2357 review MUST FIX C/D, CONSIDER F) Scan every
/// `Task`/`Step` this mission recorded (across every phase — the same
/// per-phase `lifecycle::load_steps_for_phase` walk `crawl::unit_step::
/// summarize_mission` uses) for `crawl.unit` steps (to total
/// `findings_rejected`, and to name any that never converged) and
/// `plan.sites`/`crawl.plan` steps (to name any rule that never finished
/// planning). An unreadable phase/step list no longer vanishes into a
/// default/skip — see [`StepScan::unreadable`] (silent-miss audit,
/// 2026-09-06): the mission this step cannot fully inspect still delivers
/// what it CAN see, but now says so, rather than rendering the same as a
/// mission with nothing wrong.
///
/// **The scan keys on kind ids held as local constants**
/// ([`SCANNED_CRAWL_UNIT_KIND`]/[`SCANNED_PLAN_SITES_KIND`]/
/// [`SCANNED_CRAWL_PLAN_KIND`], #2310 swarm F). This is a closed list,
/// matched by string, and there is no generic property ("this kind
/// declares residency") behind it — so a THIRD config that reviews work
/// through a step kind named anything else used to produce an EMPTY scope
/// here, silently: zero rejected findings, zero un-planned rules, zero
/// completed units, with no way to tell "nothing to report" from "nobody
/// taught the scan this kind's name". (Silent-miss audit, 2026-09-06,
/// narrowed round-2 same day): fixed by the `other` arm below, which
/// names any OTHER step that reached `Error`/`Abandoned` in `errored` —
/// an unrecognized-kind failure is exactly the kind of thing this scan
/// exists to surface, not swallow. Deliberately NOT "anything not
/// Complete": `records.gather` runs INSIDE the mission it scans, so its
/// own task's steps are `Running`/`Planned` at scan time — ordinary
/// in-flight state, not failures — and `exclude_task_id` (the caller's
/// own `task.id`) skips them entirely rather than relying on status
/// alone, since a LATER phase's not-yet-scheduled step is equally
/// `Planned` and equally not a failure.
///
/// Left as a closed match deliberately: two configs use it (`review.json`,
/// `crawl.json`), the fields the named arms read (`config.rule`,
/// `config.unit`, `output`'s `findings_rejected`) are conventions of
/// those two kinds rather than of any registered interface, and inventing
/// a `StepKind` trait method for one consumer is the extension-point
/// drift #1352 exists to stop. The obligation this doc creates instead:
/// **a new review-shaped step kind wanting its OWN rule/unit/
/// findings_rejected accounting must be added to this match at the same
/// time it is written** — a step of any other kind that merely fails is
/// now caught by the fallthrough, but its rule/unit specifics are not.
/// [`SCANNED_CRAWL_UNIT_KIND`]/[`SCANNED_PLAN_SITES_KIND`]/
/// [`SCANNED_CRAWL_PLAN_KIND`] are local literal duplicates of
/// `darkmux_lab::crawl::{unit_step::CRAWL_UNIT_KIND, plan_sites_step::
/// PLAN_SITES_KIND, plan_step::CRAWL_PLAN_KIND}` (this crate cannot
/// depend on `darkmux-lab` — this module's own doc) kept honest by a
/// conformance test in `src/mission_launch.rs`, which has access to both
/// crates' real constants and the full `StepKindRegistry`.
fn scan_unit_and_plan_steps(mission_id: &str, exclude_task_id: &str) -> StepScan {
    let mut scan = StepScan::default();
    let phases = match crate::loader::load_phases() {
        Ok(p) => p,
        Err(e) => {
            scan.unreadable.push(format!("phase records: {e:#}"));
            return scan;
        }
    };
    for phase in phases.iter().filter(|p| p.mission_id == mission_id) {
        let steps = match crate::lifecycle::load_steps_for_phase(mission_id, &phase.id) {
            Ok(s) => s,
            Err(e) => {
                scan.unreadable.push(format!("phase `{}` steps: {e:#}", phase.id));
                continue;
            }
        };
        for step in &steps {
            // (round-2 audit, 2026-09-06) `records.gather` runs INSIDE
            // the mission it scans, sharing its own TASK with a sibling
            // deliver step (`review.json`'s `deliver` task holds both
            // `records-gather-step` and `deliver-step`). At scan time
            // this very gather step is `Running` (not yet `Complete` —
            // it hasn't returned), and its sibling deliver step is still
            // `Planned` (scheduled to run right after). Neither is a
            // failure; both are simply this task's own in-flight
            // machinery, not review/crawl work to report on. Skipping
            // the gather's own task here is what keeps a clean run from
            // permanently reading as "Errored: deliver.github_review
            // `deliver-step` (Planned), records.gather `records-gather-
            // step` (Running)" on every single comment. Skipping by TASK
            // ID rather than relying on the Error/Abandoned narrowing
            // alone also covers a stale on-disk record from an earlier
            // aborted attempt at this same task (e.g. a previous
            // `deliver-step` left `Error` on disk before a retry) — that
            // is this task's own machinery re-running, not review/crawl
            // work to report on, regardless of what status it happens to
            // carry on disk right now.
            if step.task_id == exclude_task_id {
                continue;
            }
            match step.kind.as_str() {
                SCANNED_CRAWL_UNIT_KIND => {
                    if step.status != crate::types::NodeStatus::Complete {
                        scan.errored.push(format!("unit `{}` ({:?})", step.id, step.status));
                        continue;
                    }
                    // (#2454) Read the outcome BEFORE the coverage decision
                    // below, but WITHOUT letting a missing or unparseable one
                    // change that decision: this used to be an unconditional
                    // insert ahead of the read, and moving the read in front
                    // of it silently un-covered every unit whose output was
                    // absent or malformed. `None` here keeps the old benefit
                    // of the doubt; only an outcome that positively SAYS it
                    // never dispatched is treated as having reviewed nothing.
                    let doc = match step.output.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
                        None => None,
                        Some(raw) => match crate::step_output::resolve_output_doc(raw) {
                            Ok((doc, _)) => Some(doc),
                            Err(e) => {
                                // (silent-miss audit, 2026-09-06) Previously
                                // swallowed here, which read as
                                // `findings_rejected == 0` —
                                // indistinguishable from a genuinely clean
                                // unit. Named instead.
                                scan.unreadable.push(format!("unit `{}` output: {e:#}", step.id));
                                None
                            }
                        },
                    };
                    // (#2361 item 2) This unit COMPLETED, so the windows its
                    // plan named were actually reviewed —
                    // (#2454) UNLESS it completed without ever dispatching.
                    // A `Complete` status stopped being sufficient evidence
                    // the moment `crawl.unit` gained a path that returns
                    // `Ok` having done NO work: the thermal breaker's
                    // between-units gate. Such a unit read nothing, so
                    // counting its planned windows as covered would make the
                    // posted review claim coverage of hunks a thermally
                    // shortened run never looked at — the exact false claim
                    // #2361 item 2 exists to prevent, arriving through a new
                    // door. Read off the outcome's own `result` (loose JSON,
                    // never the typed `UnitOutcome` — this crate cannot
                    // depend on `darkmux-lab`); an outcome with no `result`
                    // at all keeps the old benefit of the doubt.
                    let never_dispatched = doc
                        .as_ref()
                        .and_then(|d| d.pointer("/body/result").or_else(|| d.get("result")))
                        .and_then(|v| v.as_str())
                        .is_some_and(|r| r == UNIT_RESULT_THERMAL_STOP);
                    // NOT pushed to `errored` either: nothing failed. The
                    // run's own `crawl.summary` is where a thermally
                    // shortened run is NAMED (`stopped_by: "thermal"`, the
                    // unit's own `thermal_stop` result); giving the posted
                    // review its own "skipped" line means a new
                    // `DeliverScope` field, its render and its goldens —
                    // worth doing, deliberately not folded into this fix.
                    if !never_dispatched {
                        if let (Some(rule), Some(unit)) = (
                            step.config.get("rule").and_then(|v| v.as_str()),
                            step.config.get("unit").and_then(|v| v.as_str()),
                        ) {
                            scan.completed_units.insert((rule.to_string(), unit.to_string()));
                        }
                    }
                    let rejected = doc
                        .as_ref()
                        .and_then(|d| {
                            d.pointer("/body/findings_rejected").or_else(|| d.get("findings_rejected"))
                        })
                        .and_then(|v| v.as_u64())
                        .unwrap_or(0);
                    scan.findings_rejected += rejected as usize;
                }
                SCANNED_PLAN_SITES_KIND | SCANNED_CRAWL_PLAN_KIND => {
                    if step.status == crate::types::NodeStatus::Complete {
                        continue;
                    }
                    let rule = step
                        .config
                        .get("rule")
                        .and_then(|v| v.as_str())
                        .map(str::to_string)
                        .unwrap_or_else(|| step.id.clone());
                    scan.not_attempted.push(rule);
                    scan.errored.push(format!("plan `{}` ({:?})", step.id, step.status));
                }
                other => {
                    // (silent-miss audit, 2026-09-06; narrowed round-2
                    // 2026-09-06) A step of ANY other kind that has
                    // genuinely FAILED (`Error`/`Abandoned`) is exactly
                    // what this scan exists to name — an unrecognized
                    // kind is no reason to treat its failure as
                    // invisible. Narrowed from "anything not Complete"
                    // to "Error | Abandoned only": `Planned`/`Running`
                    // are not failures — they are ordinary in-flight or
                    // not-yet-scheduled state for steps elsewhere in the
                    // SAME mission (a later phase that simply hasn't run
                    // yet), and flagging every such step as "errored"
                    // would make a clean, still-in-progress run
                    // permanently unable to report a clean scope. This
                    // cannot know a "rule" for an arbitrary kind, so it
                    // only ever contributes to `errored`, never
                    // `not_attempted`.
                    if matches!(step.status, crate::types::NodeStatus::Error | crate::types::NodeStatus::Abandoned) {
                        scan.errored.push(format!("{other} `{}` ({:?})", step.id, step.status));
                    }
                }
            }
        }
    }
    scan
}

/// Register `records.gather` — the same opt-in shape
/// `deliver_github_review::register_deliver_kind` uses: no caller
/// registers this by default; a mission config that wires both a gather
/// and a deliver step (`src/mission_launch.rs::all_step_kinds`) does.
pub fn register_records_gather_kind(registry: &StepKindRegistry) -> Result<()> {
    registry.register(Arc::new(RecordsGatherStepKind)).context("registering records.gather")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::findings::{self, Proposer, Scope};
    use crate::mods::{ForFinding, ModContext, ModRecord};
    use crate::types::{NodeStatus, Phase, PhaseStatus};
    use serde_json::json;
    use tempfile::TempDir;

    /// Scopes `DARKMUX_HOME` for one test and restores the prior value —
    /// same pattern `crawl::unit_step_tests::HomeGuard` uses, duplicated
    /// here because this crate has no dependency on that one.
    struct HomeGuard(Option<String>);
    impl HomeGuard {
        fn set(p: &std::path::Path) -> Self {
            let prior = std::env::var("DARKMUX_HOME").ok();
            std::env::set_var("DARKMUX_HOME", p);
            Self(prior)
        }
    }
    impl Drop for HomeGuard {
        fn drop(&mut self) {
            match &self.0 {
                Some(v) => std::env::set_var("DARKMUX_HOME", v),
                None => std::env::remove_var("DARKMUX_HOME"),
            }
        }
    }

    const MISSION: &str = "review-2310";
    const PHASE: &str = "review-2310-deliver";

    fn save_phase() {
        crate::lifecycle::save_phase(&Phase {
            id: PHASE.into(),
            mission_id: MISSION.into(),
            description: String::new(),
            display_name: None,
            status: PhaseStatus::Running,
            created_ts: 1,
            started_ts: None,
            completed_ts: None,
            abandoned_ts: None,
            task_ids: vec!["deliver".into()],
        })
        .unwrap();
    }

    fn task() -> Task {
        Task {
            id: "deliver".into(),
            phase_id: PHASE.into(),
            description: String::new(),
            display_name: None,
            step_ids: vec!["records-gather-step".into()],
            depends_on: Vec::new(),
            reads: Vec::new(),
            role_id: None,
            profile_name: None,
            workdir: None,
            image: None,
            run_on: crate::types::default_run_on(),
        }
    }

    fn step(config: serde_json::Value) -> Step {
        Step {
            id: "records-gather-step".into(),
            task_id: "deliver".into(),
            kind: RECORDS_GATHER_KIND.into(),
            gate: None,
            status: NodeStatus::Planned,
            config,
            started_ts: None,
            completed_ts: None,
            output: None,
        }
    }

    fn a_finding(dispatch: &str, seq: u64, mission: Option<&str>) -> FindingRecord {
        findings::build_record(
            dispatch,
            seq,
            "2026-09-05T00:00:00Z".to_string(),
            "create_finding",
            Proposer { handle: "reviewer".into(), model: "test".into(), machine_id: None },
            Scope { mission_id: mission.map(str::to_string), phase_id: None, step_id: None },
            None,
            json!({ "file": "src/a.ts", "line": 2, "pattern": "p", "evidence": "e", "why": "w" }),
        )
    }

    fn a_mod(key: &str, for_key: &str, mission: Option<&str>) -> ModRecord {
        ModRecord {
            key: key.to_string(),
            ts: "2026-09-05T00:00:01Z".to_string(),
            by: "coder".to_string(),
            r#for: vec![for_key.to_string()],
            kit: Some("kit text".to_string()),
            kit_looks_json: false,
            kit_kind: None,
            attachments: Vec::new(),
            context: ModContext {
                findings: vec![ForFinding {
                    key: for_key.to_string(),
                    mission_id: mission.map(str::to_string),
                    context: None,
                    emitted: None,
                    missing: false,
                }],
            },
            warnings: Vec::new(),
            mission_id: mission.map(str::to_string),
            phase_id: None,
            step_id: None,
            source: None,
            gate: None,
            gate_skipped_reason: None,
            schema_version: crate::mods::MOD_SCHEMA_VERSION.to_string(),
            extras: Default::default(),
        }
    }

    /// Writes a config snapshot naming `rules` (one `plan-<rule>` task per
    /// entry, each with a `rule`-bearing step) plus a graph report that
    /// prunes the tasks in `pruned`, with the reason given.
    fn save_snapshot_and_prune_report(rules: &[&str], pruned: &[(&str, &str)]) {
        let phases = serde_json::json!([{
            "id": "plan",
            "tasks": rules.iter().map(|r| serde_json::json!({
                "id": format!("plan-{r}"),
                "steps": [{"id": format!("plan-{r}-step"), "kind": "plan.sites", "config": {"rule": r}}],
            })).collect::<Vec<_>>(),
        }]);
        let config: crate::mission_config::MissionConfig = serde_json::from_value(serde_json::json!({
            "id": "review", "name": "Review v2", "phases": phases,
        }))
        .unwrap();
        crate::lifecycle::save_config_snapshot(MISSION, &config).unwrap();

        let report = crate::mission_config::prune::PruneReport {
            pruned: pruned
                .iter()
                .map(|(id, reason)| crate::mission_config::prune::Pruned {
                    id: (*id).to_string(),
                    kind: "task".to_string(),
                    reason: (*reason).to_string(),
                })
                .collect(),
            ..Default::default()
        };
        crate::lifecycle::save_graph_report(MISSION, &report).unwrap();
    }

    /// (#2310 fix-loop E2, S1-6) A rule pruned before the mint — by the
    /// document's own `enabled: false`, or by this launch's `--param
    /// rules=` selection — is NAMED in `not_attempted`, and counts toward
    /// the "of M" denominator.
    ///
    /// This is the case no on-disk RUN record can reveal: a pruned task
    /// mints no Task, no Step and no `plan/<rule>.json`, so
    /// `scan_unit_and_plan_steps` and `plan_totals` both see a run that
    /// simply never had those rules. A review narrowed from 4 rules to 1
    /// therefore rendered as a complete 1-rule review. `graph-report.json`
    /// is the only surviving record of the prune, and the config snapshot
    /// is the only thing that can map its task ids back to rule ids.
    #[test]
    #[serial_test::serial] // scopes DARKMUX_HOME, a process-global
    fn rules_pruned_before_the_mint_are_named_as_not_attempted() {
        let tmp = TempDir::new().unwrap();
        let _home = HomeGuard::set(tmp.path());
        save_phase();
        save_snapshot_and_prune_report(
            &["intent-vs-diff", "test-gap", "union-vs-enum", "swallowed-error"],
            &[("plan-test-gap", "disabled"), ("plan-union-vs-enum", "not_selected")],
        );

        let outcome = RecordsGatherStepKind.run(&step(json!({})), &task(), &BTreeMap::new()).unwrap();
        let wrapped =
            crate::step_output::Output::<GatherOutput>::read(&outcome.output, RECORDS_GATHER_OUTPUT_KIND).unwrap();
        let scope = &wrapped.body.scope;

        assert!(
            scope.not_attempted.contains(&"test-gap".to_string()),
            "a rule disabled in the document was never attempted: {scope:?}"
        );
        assert!(
            scope.not_attempted.contains(&"union-vs-enum".to_string()),
            "a rule deselected for this launch was never attempted either: {scope:?}"
        );
        assert!(
            !scope.not_attempted.contains(&"intent-vs-diff".to_string()),
            "a rule that was NOT pruned must not be named: {scope:?}"
        );
        assert_eq!(
            scope.rules_total, 4,
            "the denominator is what the CONFIG declared, pruned rules included: {scope:?}"
        );
    }

    /// The mapping is task-id -> rule-id, and only rule-bearing tasks
    /// contribute: a pruned task with no `rule` key (the crawl's own
    /// `summary`) must not put a TASK id into a sentence about rules.
    #[test]
    #[serial_test::serial] // scopes DARKMUX_HOME, a process-global
    fn a_pruned_task_that_names_no_rule_contributes_nothing() {
        let tmp = TempDir::new().unwrap();
        let _home = HomeGuard::set(tmp.path());
        save_phase();
        save_snapshot_and_prune_report(&["intent-vs-diff"], &[("summarize", "disabled")]);

        let outcome = RecordsGatherStepKind.run(&step(json!({})), &task(), &BTreeMap::new()).unwrap();
        let wrapped =
            crate::step_output::Output::<GatherOutput>::read(&outcome.output, RECORDS_GATHER_OUTPUT_KIND).unwrap();
        assert!(
            wrapped.body.scope.not_attempted.is_empty(),
            "a non-rule task's id is not a rule name: {:?}",
            wrapped.body.scope
        );
        assert_eq!(wrapped.body.scope.rules_total, 1);
    }

    /// (F2, from #2374's review) M counts DISTINCT RULES, not
    /// rule-bearing tasks. `crawl.json` declares two tasks per rule — a
    /// `crawl.plan` and a `crawl.unit` grow template — so the
    /// `task id -> rule` map holds two entries for one rule and
    /// `declared.len()` rendered a single-rule run as "0 of 2 rules
    /// reviewed", a denominator no rule count can ever reach.
    ///
    /// Red-proved by restoring `rules_total: declared.len()`: this asserts
    /// 2 instead of 1.
    #[test]
    #[serial_test::serial] // scopes DARKMUX_HOME, a process-global
    fn two_tasks_declaring_one_rule_count_as_one_rule() {
        let tmp = TempDir::new().unwrap();
        let _home = HomeGuard::set(tmp.path());
        save_phase();
        // The real shape: a plan task and a unit template, distinct task
        // ids, the SAME rule.
        let config: crate::mission_config::MissionConfig = serde_json::from_value(serde_json::json!({
            "id": "crawl", "name": "Crawl",
            "phases": [{
                "id": "plan",
                "tasks": [
                    { "id": "plan-intent-vs-diff",
                      "steps": [{"id": "s-plan", "kind": "plan.sites",
                                 "config": {"rule": "intent-vs-diff"}}] },
                    { "id": "unit-intent-vs-diff",
                      "steps": [{"id": "s-unit", "kind": "crawl.unit",
                                 "config": {"rule": "intent-vs-diff"}}] },
                ],
            }],
        }))
        .unwrap();
        crate::lifecycle::save_config_snapshot(MISSION, &config).unwrap();

        let outcome = RecordsGatherStepKind.run(&step(json!({})), &task(), &BTreeMap::new()).unwrap();
        let wrapped =
            crate::step_output::Output::<GatherOutput>::read(&outcome.output, RECORDS_GATHER_OUTPUT_KIND).unwrap();
        assert_eq!(
            wrapped.body.scope.rules_total, 1,
            "two tasks naming one rule is ONE rule in the denominator: {:?}",
            wrapped.body.scope
        );
    }

    #[test]
    #[serial_test::serial] // scopes DARKMUX_HOME, a process-global
    fn gathers_only_this_missions_findings_and_mods() {
        let tmp = TempDir::new().unwrap();
        let _home = HomeGuard::set(tmp.path());
        save_phase();

        findings::materialize(&findings::findings_dir(), &a_finding("sess-a", 1, Some(MISSION))).unwrap();
        findings::materialize(&findings::findings_dir(), &a_finding("sess-b", 1, Some("other-mission"))).unwrap();
        crate::mods::materialize(&crate::mods::mods_dir(), &a_mod("mod-1", "sess-a/1", Some(MISSION))).unwrap();
        crate::mods::materialize(&crate::mods::mods_dir(), &a_mod("mod-2", "sess-b/1", Some("other-mission")))
            .unwrap();

        let out = RecordsGatherStepKind.run(&step(json!({})), &task(), &BTreeMap::new()).unwrap();
        let wrapped = crate::step_output::Output::<GatherOutput>::read(&out.output, RECORDS_GATHER_OUTPUT_KIND).unwrap();
        assert_eq!(wrapped.body.findings.len(), 1, "{:?}", wrapped.body.findings);
        assert_eq!(wrapped.body.findings[0].key, "sess-a/1");
        assert_eq!(wrapped.body.mods.len(), 1, "{:?}", wrapped.body.mods);
        assert_eq!(wrapped.body.mods[0].record.key, "mod-1");
    }

    #[test]
    #[serial_test::serial] // scopes DARKMUX_HOME, a process-global
    fn reads_the_diff_file_and_counts_its_hunks_as_hunks_total() {
        let tmp = TempDir::new().unwrap();
        let _home = HomeGuard::set(tmp.path());
        save_phase();
        let diff_path = tmp.path().join("d.diff");
        std::fs::write(
            &diff_path,
            "diff --git a/a.ts b/a.ts\n--- a/a.ts\n+++ b/a.ts\n@@ -1,1 +1,2 @@\n foo\n+bar\n\
             diff --git a/b.ts b/b.ts\n--- a/b.ts\n+++ b/b.ts\n@@ -1,1 +1,2 @@\n baz\n+qux\n",
        )
        .unwrap();

        let out = RecordsGatherStepKind
            .run(&step(json!({ "diff_file": diff_path.to_string_lossy() })), &task(), &BTreeMap::new())
            .unwrap();
        let wrapped = crate::step_output::Output::<GatherOutput>::read(&out.output, RECORDS_GATHER_OUTPUT_KIND).unwrap();
        assert_eq!(wrapped.body.diff, std::fs::read_to_string(&diff_path).unwrap());
        assert_eq!(wrapped.body.scope.hunks_total, 2, "{:?}", wrapped.body.scope);
    }

    #[test]
    #[serial_test::serial] // scopes DARKMUX_HOME, a process-global
    fn gate_failed_mods_are_never_counted_as_refused() {
        let tmp = TempDir::new().unwrap();
        let _home = HomeGuard::set(tmp.path());
        save_phase();

        findings::materialize(&findings::findings_dir(), &a_finding("sess-a", 1, Some(MISSION))).unwrap();
        findings::materialize(&findings::findings_dir(), &a_finding("sess-a", 2, Some(MISSION))).unwrap();
        let root = crate::mods::mods_dir();
        crate::mods::materialize(&root, &a_mod("mod-pass", "sess-a/1", Some(MISSION))).unwrap();
        crate::mods::materialize(&root, &a_mod("mod-fail", "sess-a/2", Some(MISSION))).unwrap();
        crate::mods::record_gate(
            &root,
            "mod-pass",
            Some(crate::mods::GateOutcome { passed: true, command: "true".into(), exit_code: Some(0), applied: Some(true), reason: None }),
            None,
        )
        .unwrap();
        crate::mods::record_gate(
            &root,
            "mod-fail",
            Some(crate::mods::GateOutcome { passed: false, command: "false".into(), exit_code: Some(1), applied: Some(true), reason: None }),
            None,
        )
        .unwrap();

        let out = RecordsGatherStepKind.run(&step(json!({})), &task(), &BTreeMap::new()).unwrap();
        let wrapped = crate::step_output::Output::<GatherOutput>::read(&out.output, RECORDS_GATHER_OUTPUT_KIND).unwrap();
        // (#2310 P4c-2b PR #2357 review CONSIDER F, proven wrong) A
        // gate-failed mod is a DOUBLE-CHECK thread (`render_github_review`
        // already renders it that way), never counted as "refused" —
        // that field is now the runtime-boundary rejection count only
        // (`crawl.unit`'s own `findings_rejected`, asserted below), so a
        // mission with zero unit steps at all reads `refused: 0` even
        // with a gate-failed mod present.
        assert_eq!(wrapped.body.scope.refused, 0, "a gate-failed mod is a double-check thread, not a refusal: {:?}", wrapped.body.scope);
        let passed_gate = wrapped.body.mods.iter().find(|m| m.record.key == "mod-pass").unwrap();
        assert_eq!(passed_gate.gate_passed, Some(true));
        let failed_gate = wrapped.body.mods.iter().find(|m| m.record.key == "mod-fail").unwrap();
        assert_eq!(failed_gate.gate_passed, Some(false));
    }

    /// (#2310 P4c-2b PR #2357 review CONSIDER F, proven) `refused` reads
    /// off `crawl.unit`'s own `findings_rejected` — this mission's Step
    /// records, generically as JSON (never `darkmux-lab`'s typed
    /// `UnitOutcome`).
    #[test]
    #[serial_test::serial] // scopes DARKMUX_HOME, a process-global
    fn refused_sums_findings_rejected_from_this_missions_unit_steps() {
        let tmp = TempDir::new().unwrap();
        let _home = HomeGuard::set(tmp.path());
        save_phase();
        let unit_output = crate::step_output::Output::wrap(
            "crawl.unit-outcome",
            json!({ "schema_version": "1.1", "unit": "u-1", "result": "stop", "findings": 1, "findings_rejected": 3 }),
            crate::step_output::Producer::of(MISSION, "unit-task", "unit-step-1"),
        )
        .to_output_string()
        .unwrap();
        crate::lifecycle::save_step(
            MISSION,
            PHASE,
            &Step {
                id: "unit-step-1".into(),
                task_id: "unit-task".into(),
                kind: "crawl.unit".into(),
                gate: None,
                status: NodeStatus::Complete,
                config: json!({}),
                started_ts: None,
                completed_ts: None,
                output: Some(unit_output),
            },
        )
        .unwrap();
        crate::lifecycle::save_step(
            MISSION,
            PHASE,
            &Step {
                id: "unit-step-2".into(),
                task_id: "unit-task-2".into(),
                kind: "crawl.unit".into(),
                gate: None,
                status: NodeStatus::Error,
                config: json!({}),
                started_ts: None,
                completed_ts: None,
                output: Some("dispatch error".into()),
            },
        )
        .unwrap();

        let out = RecordsGatherStepKind.run(&step(json!({})), &task(), &BTreeMap::new()).unwrap();
        let wrapped = crate::step_output::Output::<GatherOutput>::read(&out.output, RECORDS_GATHER_OUTPUT_KIND).unwrap();
        assert_eq!(wrapped.body.scope.refused, 3, "{:?}", wrapped.body.scope);
        assert!(
            wrapped.body.scope.errored.iter().any(|e| e.contains("unit-step-2")),
            "the errored unit must be named: {:?}",
            wrapped.body.scope
        );
    }

    /// Save one `crawl.unit` step for `rule`/`unit` at `status`.
    fn save_unit_step(id: &str, rule: &str, unit: &str, status: NodeStatus) {
        save_unit_step_with_result(id, rule, unit, status, "stop");
    }

    /// (#2454) Same, with the outcome's `result` under the caller's control
    /// — a `Complete` step is no longer sufficient evidence a unit reviewed
    /// anything, so the tests need to say which kind of completion it was.
    fn save_unit_step_with_result(id: &str, rule: &str, unit: &str, status: NodeStatus, result: &str) {
        let output = (status == NodeStatus::Complete).then(|| {
            crate::step_output::Output::wrap(
                "crawl.unit-outcome",
                json!({ "schema_version": "1.1", "unit": unit, "result": result, "findings": 1, "findings_rejected": 0 }),
                crate::step_output::Producer::of(MISSION, "unit-task", id),
            )
            .to_output_string()
            .unwrap()
        });
        crate::lifecycle::save_step(
            MISSION,
            PHASE,
            &Step {
                id: id.into(),
                task_id: format!("unit-{rule}"),
                kind: "crawl.unit".into(),
                gate: None,
                status,
                config: json!({ "rule": rule, "unit": unit }),
                started_ts: None,
                completed_ts: None,
                output,
            },
        )
        .unwrap();
    }

    /// (#2361 item 2, PROVEN live on the 2026-09-05 live review run)
    /// FOUR of five units errored and the scope line still said "review
    /// ran: 5 rule(s), 5/5 hunks covered" — a plain false claim of
    /// coverage, because both numbers came from what was PLANNED. A plan is
    /// an intention; only a unit that COMPLETED reviewed anything. The
    /// errored four are already named in `errored`, so nothing is hidden by
    /// counting honestly here.
    #[test]
    #[serial_test::serial] // scopes DARKMUX_HOME, a process-global
    fn coverage_counts_only_rules_and_hunks_whose_unit_completed() {
        let tmp = TempDir::new().unwrap();
        let _home = HomeGuard::set(tmp.path());
        save_phase();
        let plan_dir = crate::loader::missions_dir().join(MISSION).join("plan");
        std::fs::create_dir_all(&plan_dir).unwrap();
        let rules = [
            ("swallowed-error", "src/a.ts"),
            ("unnamed-predicate", "src/b.ts"),
            ("existing-solution", "src/c.ts"),
            ("union-vs-enum", "src/d.ts"),
            ("shared-symbol-callers", "src/e.ts"),
        ];
        for (rule, file) in rules {
            write_plan_sites(&plan_dir, rule, &[(file, 1)]);
        }
        let diff_path = tmp.path().join("pr.diff");
        diff_with_n_hunks(&diff_path, &["src/a.ts", "src/b.ts", "src/c.ts", "src/d.ts", "src/e.ts"]);

        // The live shape: one unit completed, four errored.
        save_unit_step("unit-step-1", "swallowed-error", "u-0001", NodeStatus::Complete);
        for (i, (rule, _)) in rules.iter().enumerate().skip(1) {
            save_unit_step(&format!("unit-step-{}", i + 1), rule, "u-0001", NodeStatus::Error);
        }

        let out = RecordsGatherStepKind
            .run(&step(json!({ "diff_file": diff_path.to_string_lossy() })), &task(), &BTreeMap::new())
            .unwrap();
        let wrapped = crate::step_output::Output::<GatherOutput>::read(&out.output, RECORDS_GATHER_OUTPUT_KIND).unwrap();
        let scope = &wrapped.body.scope;
        assert_eq!(scope.hunks_total, 5, "{scope:?}");
        assert_eq!(
            scope.rules_run,
            vec!["swallowed-error".to_string()],
            "only the rule whose unit completed reviewed anything: {scope:?}"
        );
        assert_eq!(scope.hunks_covered, 1, "four errored units covered nothing: {scope:?}");
        assert_eq!(scope.errored.len(), 4, "and the four are still named: {scope:?}");
    }

    /// The dedup that #2357's own review installed is unchanged by the
    /// completed-only rule: two rules whose COMPLETED units plan the same
    /// window count that window ONCE.
    #[test]
    #[serial_test::serial] // scopes DARKMUX_HOME, a process-global
    fn completed_units_across_two_rules_still_dedup_the_same_window() {
        let tmp = TempDir::new().unwrap();
        let _home = HomeGuard::set(tmp.path());
        save_phase();
        let plan_dir = crate::loader::missions_dir().join(MISSION).join("plan");
        std::fs::create_dir_all(&plan_dir).unwrap();
        write_plan_sites(&plan_dir, "existing-solution", &[("src/a.ts", 1), ("src/b.ts", 1)]);
        write_plan_sites(&plan_dir, "union-vs-enum", &[("src/a.ts", 1)]);
        let diff_path = tmp.path().join("pr.diff");
        diff_with_n_hunks(&diff_path, &["src/a.ts", "src/b.ts"]);
        save_unit_step("unit-step-1", "existing-solution", "u-0001", NodeStatus::Complete);
        save_unit_step("unit-step-2", "existing-solution", "u-0002", NodeStatus::Complete);
        save_unit_step("unit-step-3", "union-vs-enum", "u-0001", NodeStatus::Complete);

        let out = RecordsGatherStepKind
            .run(&step(json!({ "diff_file": diff_path.to_string_lossy() })), &task(), &BTreeMap::new())
            .unwrap();
        let wrapped = crate::step_output::Output::<GatherOutput>::read(&out.output, RECORDS_GATHER_OUTPUT_KIND).unwrap();
        assert_eq!(wrapped.body.scope.hunks_covered, 2, "{:?}", wrapped.body.scope);
        assert_eq!(wrapped.body.scope.rules_run.len(), 2, "{:?}", wrapped.body.scope);
    }

    /// (#2454) A unit the thermal breaker skipped returns `Ok` with
    /// `result: "thermal_stop"` and NEVER dispatches, so its step reaches
    /// `Complete` having read nothing. Before this fix, `Complete` alone put
    /// its planned windows into `completed_units` — the posted review would
    /// then claim coverage of hunks a thermally shortened run never looked
    /// at, which is the exact false claim #2361 item 2 exists to prevent,
    /// arriving through a new door.
    #[test]
    #[serial_test::serial] // scopes DARKMUX_HOME, a process-global
    fn a_thermally_skipped_unit_does_not_count_as_reviewed_coverage() {
        let tmp = TempDir::new().unwrap();
        let _home = HomeGuard::set(tmp.path());
        save_phase();
        let plan_dir = crate::loader::missions_dir().join(MISSION).join("plan");
        std::fs::create_dir_all(&plan_dir).unwrap();
        write_plan_sites(&plan_dir, "swallowed-error", &[("src/a.ts", 1)]);
        write_plan_sites(&plan_dir, "union-vs-enum", &[("src/b.ts", 1)]);
        let diff_path = tmp.path().join("pr.diff");
        diff_with_n_hunks(&diff_path, &["src/a.ts", "src/b.ts"]);

        // One unit genuinely reviewed its window; the breaker tripped, so
        // the next one completed WITHOUT dispatching.
        save_unit_step("unit-step-1", "swallowed-error", "u-0001", NodeStatus::Complete);
        save_unit_step_with_result("unit-step-2", "union-vs-enum", "u-0001", NodeStatus::Complete, "thermal_stop");

        let out = RecordsGatherStepKind
            .run(&step(json!({ "diff_file": diff_path.to_string_lossy() })), &task(), &BTreeMap::new())
            .unwrap();
        let wrapped = crate::step_output::Output::<GatherOutput>::read(&out.output, RECORDS_GATHER_OUTPUT_KIND).unwrap();
        let scope = &wrapped.body.scope;
        assert_eq!(scope.hunks_total, 2, "{scope:?}");
        assert_eq!(
            scope.hunks_covered, 1,
            "the thermally-skipped unit dispatched nothing, so its window is NOT covered: {scope:?}"
        );
        assert_eq!(
            scope.rules_run,
            vec!["swallowed-error".to_string()],
            "a rule whose only unit was skipped did not run: {scope:?}"
        );
        assert!(
            scope.errored.is_empty(),
            "and it is not an ERROR either — nothing failed, the machine got hot: {scope:?}"
        );
    }

    /// Write a `plan/<rule>.json` whose `units[].sites` list is exactly
    /// `sites` — `(file, start)` pairs.
    fn write_plan_sites(plan_dir: &std::path::Path, rule: &str, sites: &[(&str, u64)]) {
        let units: Vec<serde_json::Value> = sites
            .iter()
            .enumerate()
            .map(|(i, (file, start))| {
                json!({"id": format!("u-{:04}", i + 1), "kind": "site", "sites": [{"file": file, "start": start}]})
            })
            .collect();
        std::fs::write(
            plan_dir.join(format!("{rule}.json")),
            serde_json::to_string(&json!({ "kind": "crawl.plan", "body": { "units": units } })).unwrap(),
        )
        .unwrap();
    }

    fn diff_with_n_hunks(path: &std::path::Path, files: &[&str]) {
        let mut text = String::new();
        for f in files {
            text.push_str(&format!(
                "diff --git a/{f} b/{f}\n--- a/{f}\n+++ b/{f}\n@@ -1,1 +1,2 @@\n foo\n+bar\n"
            ));
        }
        std::fs::write(path, text).unwrap();
    }

    /// (#2310 P4c-2b PR #2357 round-2 review item 1, proven vacuous) The
    /// PRIOR version of this test used a fixture where `sum(sites)` and
    /// `distinct(sites)` both landed at the SAME capped value (3), so
    /// reverting the DISTINCT count back to a per-rule SUM (the exact
    /// CONSIDER E regression) left this test green. Rebuilt so the two
    /// answers actually diverge: 4 hunks in the diff, 3 DISTINCT windows
    /// (`src/a.ts:2` shared by both rules), 5 sites total summed across
    /// rules (3 + 2) — a sum-based count would read `min(5, 4) = 4`; the
    /// correct distinct count is `3`. Mutation-killed below.
    #[test]
    #[serial_test::serial] // scopes DARKMUX_HOME, a process-global
    fn hunks_covered_counts_distinct_windows_not_the_sum_across_rules() {
        let tmp = TempDir::new().unwrap();
        let _home = HomeGuard::set(tmp.path());
        save_phase();
        let plan_dir = crate::loader::missions_dir().join(MISSION).join("plan");
        std::fs::create_dir_all(&plan_dir).unwrap();
        write_plan_sites(&plan_dir, "existing-solution", &[("src/a.ts", 2), ("src/b.ts", 5), ("src/c.ts", 9)]);
        write_plan_sites(&plan_dir, "union-vs-enum", &[("src/a.ts", 2), ("src/b.ts", 5)]);
        // (#2361 item 2) Every unit COMPLETED, so every planned window is
        // genuinely covered — this test is about the dedup arithmetic, and
        // the completed-only rule must leave it untouched.
        for (rule, units) in [("existing-solution", 3), ("union-vs-enum", 2)] {
            for u in 1..=units {
                save_unit_step(&format!("unit-{rule}-{u}"), rule, &format!("u-{u:04}"), NodeStatus::Complete);
            }
        }
        let diff_path = tmp.path().join("d.diff");
        diff_with_n_hunks(&diff_path, &["a.ts", "b.ts", "c.ts", "d.ts"]);

        let out = RecordsGatherStepKind
            .run(&step(json!({ "diff_file": diff_path.to_string_lossy() })), &task(), &BTreeMap::new())
            .unwrap();
        let wrapped = crate::step_output::Output::<GatherOutput>::read(&out.output, RECORDS_GATHER_OUTPUT_KIND).unwrap();
        assert_eq!(wrapped.body.scope.rules_run, vec!["existing-solution".to_string(), "union-vs-enum".to_string()]);
        assert_eq!(
            wrapped.body.scope.hunks_covered, 3,
            "3 DISTINCT (file, start) windows across both rules (5 sites summed, 4 hunks total) — a SUM-based \
             count would wrongly read 4: {:?}",
            wrapped.body.scope
        );
    }

    /// (#2310 P4c-2b PR #2357 round-2 review item 1) The companion case:
    /// plans find MORE distinct windows than the diff actually has hunks
    /// (3 distinct windows, only 2 hunks in the diff) — the cap must win.
    /// Mutation-killed below (deleting `.min(hunks_total)` reads 3, not 2).
    #[test]
    #[serial_test::serial] // scopes DARKMUX_HOME, a process-global
    fn hunks_covered_is_capped_when_plans_find_more_windows_than_the_diff_has_hunks() {
        let tmp = TempDir::new().unwrap();
        let _home = HomeGuard::set(tmp.path());
        save_phase();
        let plan_dir = crate::loader::missions_dir().join(MISSION).join("plan");
        std::fs::create_dir_all(&plan_dir).unwrap();
        write_plan_sites(&plan_dir, "existing-solution", &[("src/a.ts", 2), ("src/b.ts", 5), ("src/c.ts", 9)]);
        // (#2361 item 2) All three units completed — the cap, not the
        // completed-only rule, is what this test measures.
        for u in 1..=3 {
            save_unit_step(&format!("unit-es-{u}"), "existing-solution", &format!("u-{u:04}"), NodeStatus::Complete);
        }
        let diff_path = tmp.path().join("d.diff");
        diff_with_n_hunks(&diff_path, &["a.ts", "b.ts"]);

        let out = RecordsGatherStepKind
            .run(&step(json!({ "diff_file": diff_path.to_string_lossy() })), &task(), &BTreeMap::new())
            .unwrap();
        let wrapped = crate::step_output::Output::<GatherOutput>::read(&out.output, RECORDS_GATHER_OUTPUT_KIND).unwrap();
        assert_eq!(
            wrapped.body.scope.hunks_covered, 2,
            "3 distinct windows found but the diff only has 2 hunks — the cap must win: {:?}",
            wrapped.body.scope
        );
    }

    #[test]
    #[serial_test::serial] // scopes DARKMUX_HOME, a process-global
    fn a_task_naming_an_unrecorded_phase_is_refused_by_name() {
        let tmp = TempDir::new().unwrap();
        let _home = HomeGuard::set(tmp.path());
        // No `save_phase()` call — the phase record does not exist.
        let err = RecordsGatherStepKind.run(&step(json!({})), &task(), &BTreeMap::new()).unwrap_err();
        assert!(err.to_string().contains(PHASE), "{err}");
    }

    #[test]
    #[serial_test::serial] // scopes DARKMUX_HOME, a process-global
    fn deliver_github_review_reads_a_records_gather_step_as_its_own_task_predecessor() {
        // (#2310 P4c-2b) The wiring this packet adds: `records.gather` and
        // `deliver.github_review` as two steps of ONE task, the SAME
        // same-task-predecessor `input` entry `scheduler::gather_inputs`
        // already threads to every multi-step task — no `step.config`
        // literal `findings`/`mods`/`diff`/`scope` at all.
        let tmp = TempDir::new().unwrap();
        let _home = HomeGuard::set(tmp.path());
        save_phase();
        findings::materialize(&findings::findings_dir(), &a_finding("sess-a", 1, Some(MISSION))).unwrap();

        let gather_out =
            RecordsGatherStepKind.run(&step(json!({})), &task(), &BTreeMap::new()).unwrap();
        let mut input = BTreeMap::new();
        input.insert("records-gather-step".to_string(), gather_out.output);

        let deliver_step = Step {
            id: "deliver-step".into(),
            task_id: "deliver".into(),
            kind: super::super::deliver_github_review::DELIVER_GITHUB_REVIEW_KIND.into(),
            gate: None,
            status: NodeStatus::Planned,
            config: json!({ "emit": "-" }),
            started_ts: None,
            completed_ts: None,
            output: None,
        };
        let outcome =
            super::super::deliver_github_review::DeliverGithubReviewStepKind.run(&deliver_step, &task(), &input).unwrap();
        // (#2310 fix-loop E2) The deliver step's output is a promotable
        // `{mode, summary, emit}` object; the destination is the `emit`
        // field.
        let step_output: serde_json::Value = serde_json::from_str(&outcome.output).unwrap();
        assert_eq!(step_output["emit"], json!("-"));
    }

    /// (#1748) The full mechanical-absence-backstop wiring, end to end:
    /// `records.gather` resolves the finding's rule + source tree from a
    /// real `plan/<rule>.json` on disk, reads the WHOLE file (not the
    /// diff hunk), finds the finding's claimed-absent token elsewhere in
    /// it, and hands `deliver.github_review` a note it renders as a
    /// caveat on the posted comment — with NO real dispatch, NO model,
    /// and NO `step.config` literal for the backstop map (it only ever
    /// arrives via the same-task-predecessor `GatherOutput`, exactly the
    /// path `deliver_github_review_reads_a_records_gather_step_as_its_own_task_predecessor`
    /// above exercises for `findings`/`mods`/`diff`/`scope`).
    #[test]
    #[serial_test::serial] // scopes DARKMUX_HOME, a process-global
    fn the_absence_backstop_flows_from_records_gather_through_to_the_posted_comment() {
        let tmp = TempDir::new().unwrap();
        let _home = HomeGuard::set(tmp.path());
        save_phase();

        // The checked-out source tree `plan.sites` would have written —
        // the WHOLE file the diff hunk never shows.
        let tree = tmp.path().join("checkout").join("app");
        std::fs::create_dir_all(&tree).unwrap();
        std::fs::write(tree.join("a.ts"), "export function foo() { return 1; }\n").unwrap();

        // The plan `records.gather`'s own `plan_totals` already reads —
        // this packet reads it again for `sources[].tree`.
        let plan_dir = crate::loader::missions_dir().join(MISSION).join("plan");
        std::fs::create_dir_all(&plan_dir).unwrap();
        std::fs::write(
            plan_dir.join("existing-solution.json"),
            serde_json::to_string(&json!({
                "kind": "crawl.plan",
                "schema_version": "1",
                "body": {
                    "schema_version": "1.1",
                    "workspace": "ws",
                    "planned_at": "2026-09-10T00:00:00Z",
                    "sources": [{"id": "app", "sha": "abc123", "ref": "main", "tree": tree.to_string_lossy(), "files_walked": 1}],
                    "units": [],
                    "totals": {"units": 0, "est_tokens": 0, "by_rule": {}, "skipped": [], "edges": []},
                    "rules": ["existing-solution"],
                }
            }))
            .unwrap(),
        )
        .unwrap();

        // A finding claiming `foo()` does not exist — false, per the file
        // above — anchored to a diff hunk that touches `a.ts` line 1 (so
        // it renders as a plain inline comment, not just a body count).
        let finding = findings::build_record(
            "sess-a",
            1,
            "2026-09-10T00:00:00Z".to_string(),
            "create_finding",
            Proposer { handle: "reviewer".into(), model: "test".into(), machine_id: None },
            Scope { mission_id: Some(MISSION.to_string()), phase_id: None, step_id: None },
            Some(json!({"rule": "existing-solution", "source": "app"})),
            json!({
                "file": "a.ts", "line": 1, "pattern": "existing-solution", "evidence": "ev",
                "why": "This module does not call `foo()` anywhere in this file."
            }),
        );
        findings::materialize(&findings::findings_dir(), &finding).unwrap();

        let diff_path = tmp.path().join("d.diff");
        std::fs::write(&diff_path, "diff --git a/a.ts b/a.ts\n--- a/a.ts\n+++ a.ts\n@@ -1,1 +1,1 @@\n-old\n+export function foo() { return 1; }\n").unwrap();

        let gather_out = RecordsGatherStepKind
            .run(&step(json!({ "diff_file": diff_path.to_string_lossy() })), &task(), &BTreeMap::new())
            .unwrap();
        // The backstop's own finding-level output, before it ever reaches
        // `deliver.github_review` — proves the WIRING point, not just the
        // pure check (already covered in `absence_backstop`'s own tests).
        let wrapped =
            crate::step_output::Output::<GatherOutput>::read(&gather_out.output, RECORDS_GATHER_OUTPUT_KIND).unwrap();
        assert_eq!(wrapped.body.absence_backstop.len(), 1, "{:?}", wrapped.body.absence_backstop);
        let note = wrapped.body.absence_backstop.get("sess-a/1").expect("the finding key is flagged");
        assert_eq!(note.token, "foo()");

        let mut input = BTreeMap::new();
        input.insert("records-gather-step".to_string(), gather_out.output);
        let emit_path = tmp.path().join("review.json");
        let deliver_step = Step {
            id: "deliver-step".into(),
            task_id: "deliver".into(),
            kind: super::super::deliver_github_review::DELIVER_GITHUB_REVIEW_KIND.into(),
            gate: None,
            status: NodeStatus::Planned,
            config: json!({ "emit": emit_path.to_string_lossy() }),
            started_ts: None,
            completed_ts: None,
            output: None,
        };
        super::super::deliver_github_review::DeliverGithubReviewStepKind.run(&deliver_step, &task(), &input).unwrap();

        let posted: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&emit_path).unwrap()).unwrap();
        let body = posted["review"]["comments"][0]["body"].as_str().expect("one posted comment");
        // (#1748 review MUST FIX 1) The file the caveat names goes through
        // `code_span` too, same as the token — both are model-authored.
        assert!(
            body.contains("A mechanical check found `foo()` elsewhere in this file, at `a.ts`:1"),
            "the caveat reached the posted comment: {body}"
        );
    }

    /// (silent-miss audit, 2026-09-06) Before this fix, `_ => {}` meant a
    /// step of any kind OTHER than `crawl.unit`/`plan.sites`/`crawl.plan`
    /// that ended `Error` was invisible to the scope entirely — a
    /// `crawl.json` config that grows a `crawl.summary`/`finding`/
    /// `dispatch.internal` step which then errors produced a scope
    /// identical to one where that step never ran into trouble. Red-proved
    /// by reverting the `other =>` arm to `_ => {}`: this test then fails
    /// because `scope.errored` no longer names `weird-step-1`.
    #[test]
    #[serial_test::serial] // scopes DARKMUX_HOME, a process-global
    fn an_unrecognized_kind_that_errored_is_named_not_swallowed() {
        let tmp = TempDir::new().unwrap();
        let _home = HomeGuard::set(tmp.path());
        save_phase();
        crate::lifecycle::save_step(
            MISSION,
            PHASE,
            &Step {
                id: "weird-step-1".into(),
                task_id: "weird-task".into(),
                kind: "crawl.summary".into(),
                gate: None,
                status: NodeStatus::Error,
                config: json!({}),
                started_ts: None,
                completed_ts: None,
                output: Some("dispatch error".into()),
            },
        )
        .unwrap();

        let out = RecordsGatherStepKind.run(&step(json!({})), &task(), &BTreeMap::new()).unwrap();
        let wrapped = crate::step_output::Output::<GatherOutput>::read(&out.output, RECORDS_GATHER_OUTPUT_KIND).unwrap();
        assert!(
            wrapped.body.scope.errored.iter().any(|e| e.contains("crawl.summary") && e.contains("weird-step-1")),
            "an unrecognized kind that errored must still be named: {:?}",
            wrapped.body.scope
        );
        assert!(
            wrapped.body.scope.not_attempted.is_empty(),
            "a generic errored step names no rule — it cannot know one: {:?}",
            wrapped.body.scope
        );
    }

    /// The flip side of the test above: an unrecognized kind that reached
    /// `Complete` is not a failure, and must not appear in `errored` —
    /// only genuinely troubled steps of unknown kinds get named.
    #[test]
    #[serial_test::serial] // scopes DARKMUX_HOME, a process-global
    fn an_unrecognized_kind_that_completed_is_not_named_as_errored() {
        let tmp = TempDir::new().unwrap();
        let _home = HomeGuard::set(tmp.path());
        save_phase();
        crate::lifecycle::save_step(
            MISSION,
            PHASE,
            &Step {
                id: "weird-step-2".into(),
                task_id: "weird-task-2".into(),
                kind: "crawl.summary".into(),
                gate: None,
                status: NodeStatus::Complete,
                config: json!({}),
                started_ts: None,
                completed_ts: None,
                output: Some("ok".into()),
            },
        )
        .unwrap();

        let out = RecordsGatherStepKind.run(&step(json!({})), &task(), &BTreeMap::new()).unwrap();
        let wrapped = crate::step_output::Output::<GatherOutput>::read(&out.output, RECORDS_GATHER_OUTPUT_KIND).unwrap();
        assert!(
            wrapped.body.scope.errored.is_empty(),
            "a completed step of an unrecognized kind is not a failure: {:?}",
            wrapped.body.scope
        );
    }

    /// (round-2 audit, 2026-09-06 — the reviewer's own probe) `records.
    /// gather` runs INSIDE the mission it scans, sharing its own TASK
    /// (`deliver`, per the `task()` helper) with the `deliver.
    /// github_review` step that runs right after it in the SAME task —
    /// exactly `review.json`'s real shape. At scan time the gather step
    /// itself is `Running` (it hasn't returned yet) and its sibling
    /// deliver step is `Planned` (scheduled next); NEITHER is a failure.
    /// Before this fix, the unrecognized-kind fallthrough treated "not
    /// Complete" as failure for ANY kind, so a review's own delivery
    /// machinery permanently poisoned its own scope with "Errored:
    /// deliver.github_review `deliver-step` (Planned), records.gather
    /// `records-gather-step` (Running)" on every single comment — the
    /// clean path could never be taken.
    #[test]
    #[serial_test::serial] // scopes DARKMUX_HOME, a process-global
    fn the_gathers_own_in_flight_task_siblings_are_never_named_as_errored() {
        let tmp = TempDir::new().unwrap();
        let _home = HomeGuard::set(tmp.path());
        save_phase();
        // The gather step ITSELF, persisted `Running` — it hasn't
        // returned at scan time, same task id `task()` names.
        crate::lifecycle::save_step(
            MISSION,
            PHASE,
            &Step {
                id: "records-gather-step".into(),
                task_id: "deliver".into(),
                kind: RECORDS_GATHER_KIND.into(),
                gate: None,
                status: NodeStatus::Running,
                config: json!({}),
                started_ts: None,
                completed_ts: None,
                output: None,
            },
        )
        .unwrap();
        // Its sibling deliver step, still `Planned` — scheduled to run
        // right after this gather step, same task.
        crate::lifecycle::save_step(
            MISSION,
            PHASE,
            &Step {
                id: "deliver-step".into(),
                task_id: "deliver".into(),
                kind: super::super::deliver_github_review::DELIVER_GITHUB_REVIEW_KIND.into(),
                gate: None,
                status: NodeStatus::Planned,
                config: json!({ "emit": "-" }),
                started_ts: None,
                completed_ts: None,
                output: None,
            },
        )
        .unwrap();

        let out = RecordsGatherStepKind.run(&step(json!({})), &task(), &BTreeMap::new()).unwrap();
        let wrapped = crate::step_output::Output::<GatherOutput>::read(&out.output, RECORDS_GATHER_OUTPUT_KIND).unwrap();
        assert!(
            wrapped.body.scope.errored.is_empty(),
            "the gather step's own in-flight task siblings must never read as failures: {:?}",
            wrapped.body.scope
        );
    }

    /// (round-2 audit, 2026-09-06) Discriminates the task-id skip from
    /// the Error/Abandoned narrowing above: even a stale on-disk `Error`/
    /// `Abandoned` record for the gather's OWN task (e.g. left over from
    /// an earlier aborted attempt, before a retry) must still be skipped
    /// — it is this task's own re-running machinery, not review/crawl
    /// work. Red-proved by removing the `task_id == exclude_task_id`
    /// skip alone (leaving the status narrowing in place): this test then
    /// fails because both statuses here ARE `Error`/`Abandoned`, so the
    /// narrowing alone would still name them.
    #[test]
    #[serial_test::serial] // scopes DARKMUX_HOME, a process-global
    fn a_stale_errored_record_for_the_gathers_own_task_is_still_skipped() {
        let tmp = TempDir::new().unwrap();
        let _home = HomeGuard::set(tmp.path());
        save_phase();
        crate::lifecycle::save_step(
            MISSION,
            PHASE,
            &Step {
                id: "records-gather-step".into(),
                task_id: "deliver".into(),
                kind: RECORDS_GATHER_KIND.into(),
                gate: None,
                status: NodeStatus::Error,
                config: json!({}),
                started_ts: None,
                completed_ts: None,
                output: Some("dispatch error".into()),
            },
        )
        .unwrap();
        crate::lifecycle::save_step(
            MISSION,
            PHASE,
            &Step {
                id: "deliver-step".into(),
                task_id: "deliver".into(),
                kind: super::super::deliver_github_review::DELIVER_GITHUB_REVIEW_KIND.into(),
                gate: None,
                status: NodeStatus::Abandoned,
                config: json!({ "emit": "-" }),
                started_ts: None,
                completed_ts: None,
                output: None,
            },
        )
        .unwrap();

        let out = RecordsGatherStepKind.run(&step(json!({})), &task(), &BTreeMap::new()).unwrap();
        let wrapped = crate::step_output::Output::<GatherOutput>::read(&out.output, RECORDS_GATHER_OUTPUT_KIND).unwrap();
        assert!(
            wrapped.body.scope.errored.is_empty(),
            "a stale Error/Abandoned record for the gather's OWN task must still be skipped, \
             not named as review/crawl work: {:?}",
            wrapped.body.scope
        );
    }

    /// (round-2 audit, 2026-09-06 — C3) No existing test in this module
    /// ever saved an actual `crawl.plan` STEP record (line-searching the
    /// module before this fix: `"crawl.plan"` only ever appeared as a
    /// `kind` field INSIDE a plan file's JSON body, never as a Step's own
    /// `kind`) — so the `SCANNED_CRAWL_PLAN_KIND` arm of the match was
    /// exercised by construction (`| SCANNED_CRAWL_PLAN_KIND`) but never
    /// by a real fixture proving it actually fires. Red-proved by
    /// deleting `| SCANNED_CRAWL_PLAN_KIND` from the match (leaving only
    /// `SCANNED_PLAN_SITES_KIND`): the step then falls to the generic
    /// `other` arm, which names it in `errored` but — having no
    /// rule/unit convention of its own — never in `not_attempted`, so
    /// this test's `not_attempted` assertion goes red.
    #[test]
    #[serial_test::serial] // scopes DARKMUX_HOME, a process-global
    fn a_crawl_plan_step_that_errored_names_its_rule_as_not_attempted() {
        let tmp = TempDir::new().unwrap();
        let _home = HomeGuard::set(tmp.path());
        save_phase();
        crate::lifecycle::save_step(
            MISSION,
            PHASE,
            &Step {
                id: "plan-step-1".into(),
                task_id: "plan-task".into(),
                kind: SCANNED_CRAWL_PLAN_KIND.into(),
                gate: None,
                status: NodeStatus::Error,
                config: json!({ "rule": "existing-solution" }),
                started_ts: None,
                completed_ts: None,
                output: Some("dispatch error".into()),
            },
        )
        .unwrap();

        let out = RecordsGatherStepKind.run(&step(json!({})), &task(), &BTreeMap::new()).unwrap();
        let wrapped = crate::step_output::Output::<GatherOutput>::read(&out.output, RECORDS_GATHER_OUTPUT_KIND).unwrap();
        assert!(
            wrapped.body.scope.not_attempted.contains(&"existing-solution".to_string()),
            "an errored `crawl.plan` step must name its rule as not attempted: {:?}",
            wrapped.body.scope
        );
        assert!(
            wrapped.body.scope.errored.iter().any(|e| e.contains("plan-step-1")),
            "{:?}",
            wrapped.body.scope
        );
    }

    /// (silent-miss audit, 2026-09-06) A `crawl.unit` step's own output
    /// that fails to parse used to be swallowed by `let Ok((doc, _)) = ...
    /// else { continue }` — `findings_rejected` for that unit silently
    /// read as `0`, indistinguishable from a genuinely clean unit. Now
    /// named on `GatherOutput::unreadable`. Red-proved by reverting the
    /// `resolve_output_doc` match back to the `let-else`: this test then
    /// fails because `unreadable` is empty.
    #[test]
    #[serial_test::serial] // scopes DARKMUX_HOME, a process-global
    fn a_units_unparseable_output_is_named_unreadable_not_counted_as_clean() {
        let tmp = TempDir::new().unwrap();
        let _home = HomeGuard::set(tmp.path());
        save_phase();
        crate::lifecycle::save_step(
            MISSION,
            PHASE,
            &Step {
                id: "unit-step-bad".into(),
                task_id: "unit-task-bad".into(),
                kind: "crawl.unit".into(),
                gate: None,
                status: NodeStatus::Complete,
                config: json!({}),
                started_ts: None,
                completed_ts: None,
                // Not valid `Output::read`-able JSON — the runtime wrote a
                // malformed/truncated envelope.
                output: Some("{ this is not json".into()),
            },
        )
        .unwrap();

        let out = RecordsGatherStepKind.run(&step(json!({})), &task(), &BTreeMap::new()).unwrap();
        let wrapped = crate::step_output::Output::<GatherOutput>::read(&out.output, RECORDS_GATHER_OUTPUT_KIND).unwrap();
        assert_eq!(
            wrapped.body.scope.refused, 0,
            "an unreadable output contributes no count either way: {:?}",
            wrapped.body.scope
        );
        assert!(
            wrapped.body.unreadable.iter().any(|u| u.contains("unit-step-bad")),
            "the unparseable unit output must be named as unreadable, not treated as clean: {:?}",
            wrapped.body.unreadable
        );
    }

    /// (silent-miss audit, 2026-09-06) An unreadable `steps_dir` for one
    /// phase (a `.json` file that fails to parse as a `Step`) used to be
    /// swallowed by `load_steps_for_phase`'s `Err` being skipped via
    /// `let Ok(steps) = ... else { continue }` — that phase's steps
    /// (including any `crawl.unit`/`plan.sites` records) became entirely
    /// invisible to the scan, no different from a phase with nothing to
    /// report. Now named on `GatherOutput::unreadable`.
    #[test]
    #[serial_test::serial] // scopes DARKMUX_HOME, a process-global
    fn an_unreadable_phase_step_directory_is_named_unreadable() {
        let tmp = TempDir::new().unwrap();
        let _home = HomeGuard::set(tmp.path());
        save_phase();
        let steps_dir = crate::lifecycle::steps_dir(MISSION, PHASE);
        std::fs::create_dir_all(&steps_dir).unwrap();
        std::fs::write(steps_dir.join("corrupt.json"), "{ not valid json at all").unwrap();

        let out = RecordsGatherStepKind.run(&step(json!({})), &task(), &BTreeMap::new()).unwrap();
        let wrapped = crate::step_output::Output::<GatherOutput>::read(&out.output, RECORDS_GATHER_OUTPUT_KIND).unwrap();
        assert!(
            wrapped.body.unreadable.iter().any(|u| u.contains(PHASE)),
            "an unreadable phase step directory must be named, not silently treated as \
             'nothing to report': {:?}",
            wrapped.body.unreadable
        );
    }
}
