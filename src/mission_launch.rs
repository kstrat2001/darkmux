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
//! reusing `coder_phase.rs`'s own `MissionWorktreeStepKind` /
//! `MissionCoderStepKind` / `MissionVerifyStepKind` Tier 3 kinds verbatim
//! (elevated to `pub(crate)` for this module — see their doc comments) so
//! the `mission.worktree`/`mission.coder`/`mission.verify` flow-record
//! shape and `darkmux-serve`'s `/diff` contract stay byte-identical to what
//! the retired `mission run` produced. (#1426 ship-4) `darkmux mission run`
//! retired: `mission launch coder-phase` is now the ONE path that runs the
//! coder pipeline, absorbing what `mission run` did — the launch-owned
//! `coder_phase.rs` kinds it reuses are the same ones `mission run` drove.
//!
//! **Gate semantics (#1284 review round 1, must-fix 1).** The coder-phase
//! path deliberately does NOT finalize the mission. It stops at the "gate —
//! awaiting frontier/operator sign-off" banner with the phase left
//! `Running`, so the operator adjudicates and — after shipping the git work by
//! hand — `mission finalize` finishes the loop (`mission ship` retired in
//! #1463; the outcome map the retired `mission run` also produced — same gate
//! banners, same exit codes, same Running end state;
//! see [`coder_phase_gate_outcome`]). Auto-closing past that gate was an
//! operator-sovereignty violation (#44) at precisely the decision point the
//! gate reserves. Generic `finalize_mission` stays reserved for
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
//! resolution (`staffing`, `judge_concurrency` — see `templates/
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
use crate::coder_phase;
use anyhow::{anyhow, bail, Context, Result};
use crew::mission_config::{self, FindingSeverity, LaunchParams, MissionConfig, TaskOverride};
use crew::run_obs;
use crew::types::{Mission, MissionSpec, MissionStatus, NodeStatus, Phase, PhaseStatus, Step};
use darkmux_types::style;
use std::any::Any;
use std::collections::BTreeMap;
use std::io::IsTerminal;
use std::path::Path;
use std::sync::{Arc, Mutex};

/// The three Tier 3 step kinds `coder_phase.rs` defines for `coder-phase`'s
/// graph (#1352).
///
/// **Structural-routing use ONLY (#1530 — one global step-kind registry).**
/// Before this packet, this list did double duty: it also fed the
/// known-kind VALIDATION set and the EXECUTION registry — both of those
/// now resolve against [`all_step_kinds`]'s single shared registry instead
/// (`registry.ids()`), so a config naming a kind this launcher can actually
/// construct is never rejected just because two separate hand-maintained
/// lists drifted apart. What's left is [`config_uses_coder_phase_kinds`]'s
/// structural test — "does this graph declare any of these kinds" decides
/// which task-override / precheck / gate-outcome machinery applies, a
/// question `registry.ids()` alone can't answer (knowing a kind CAN be
/// constructed doesn't say whether coder-phase-specific wiring should run).
const CODER_PHASE_TIER3_KINDS: &[&str] = &["mission.worktree", "mission.coder", "mission.verify"];

/// The five Tier 3 step kinds `crates/darkmux-lab/src/lab/review.rs` defines
/// for `review`'s graph (#1352; `review.probe`/`review.verify` retired in
/// #1442 — the probe/verify stages ride the generic Tier-1 `dispatch.map`) —
/// wired through via a DEDICATED launcher
/// ([`crate::mission_launch_review::launch`]), not this
/// module's generic `mission_config::interpret` + `crew::scheduler::
/// run_step_graph` path. `build_review_graph`/`run_review_graph` already
/// carry real, working, tested cross-step behavior (a shared remote-token
/// bucket, host telemetry sampling, post-run envelope merges) a generic
/// collapse would either lose or have to re-derive — an audited non-collapse
/// per `CLAUDE.md`'s StepKind tiering section ("a collapse that changes
/// observable behavior isn't a tiering fix, it's a feature change wearing a
/// tiering fix's clothes").
///
/// **Structural-routing use ONLY (#1530 — one global step-kind registry).**
/// Mirrors [`CODER_PHASE_TIER3_KINDS`]'s doc: this list no longer feeds
/// validation or any execution registry (both now read [`all_step_kinds`]'s
/// `registry.ids()` instead) — it exists purely so
/// [`config_uses_review_kinds`] can decide, structurally, whether a config's
/// graph is a review pipeline that must route to the dedicated launcher.
const REVIEW_TIER3_KINDS: &[&str] = &[
    "review.bundle",
    "review.dedup",
    "review.judge",
    // (#1442 ship-2b) `review.probe` / `review.verify` retired — the
    // probe/verify stages ride the generic Tier-1 `dispatch.map` (already
    // in the builtin known set); each stage's bespoke half is a
    // frozen-prompt render step.
    "review.probe-render",
    "review.verify-render",
    "review.synthesis",
];

/// The ONE registry every step kind darkmux can construct resolves through:
/// Tier 1 builtins, `darkmux-lab`'s review Tier 3 kinds, and this crate's own
/// coder-phase Tier 3 kinds (#1530 — one global step-kind registry).
///
/// **Why assembly happens here, in the binary, and not in either library
/// crate.** `darkmux-lab` depends on `darkmux-crew` — never the reverse
/// (see `CLAUDE.md`'s crate layout) — so `StepKindRegistry` (which lives in
/// `darkmux-crew`) structurally cannot know about `review.*` kinds (defined
/// in `darkmux-lab`), and the coder-phase `mission.*` kinds are launch-owned
/// (defined right here in `src/coder_phase.rs`, per `CLAUDE.md`'s StepKind
/// tiering section — Tier 3 kinds live beside the mission module that owns
/// them, never in `darkmux-crew`). This binary is the only place all three
/// families are visible at once, so it's the only place a SINGLE registry
/// spanning all of them can be built. Every step kind here is a stateless
/// unit struct (#1536/#1537/#1553), so registering each once into a shared
/// registry — rather than per-graph-per-call — is safe.
///
/// [`launch`] uses this registry for BOTH the known-kind validation pass
/// (replacing the old `CODER_PHASE_TIER3_KINDS` + `REVIEW_TIER3_KINDS`
/// hand-maintained unions) and the real execution registry. The payoff: a
/// config whose graph names BOTH a `mission.coder` step and a `review.judge`
/// step resolves every kind against this ONE registry — a capability that
/// was structurally impossible while `review`'s dedicated launcher and this
/// module's coder-phase path each built their own PARTIAL registry (see
/// `both_families_resolve_against_one_registry` in this module's tests).
/// (#1860) `pub(crate)` so `mission_config_cli::show` can build THE SAME
/// registry `mission launch` constructs its execution registry from — a
/// step's constructibility must be the identical check in both places, not
/// a re-derived approximation.
pub(crate) fn all_step_kinds() -> Result<crew::step_kinds::StepKindRegistry> {
    let registry = crew::step_kinds::StepKindRegistry::with_builtins();
    darkmux_lab::lab::review::register_review_kinds(&registry)
        .context("registering review step kinds")?;
    register_coder_phase_step_kinds(&registry).context("registering coder-phase step kinds")?;
    // (#2298) The crawl's planning as a step kind — one `crawl.plan` task per
    // rule in `crawl.json`. Registered here so `mission config show crawl`
    // validates its graph without an unknown-kind warning and so the graph
    // is constructible the day the literal launcher retires (#2301).
    darkmux_lab::crawl::plan_step::register_crawl_kinds(&registry).context("registering crawl step kinds")?;
    Ok(registry)
}

/// `darkmux mission launch <config-id>` entry point. Returns the process
/// exit code — the coder-phase rows mirror `coder_phase::run`'s own exit
/// map exactly (#1284 review round 1, must-fix 1):
///   `0` — freeform mint; or coder ran and QA came back clean/flags-only
///         (gate banner printed, phase left Running for `mission finalize`);
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
/// (#1562) [`MissionConfigSource`] → the recorded [`MissionSpecOrigin`]:
/// only the user tier is operator-owned; `OnDisk` is a repo/templates-dir
/// copy of a SHIPPED config, so it classifies with `Embedded` as builtin.
fn spec_origin_for(source: crew::mission_config::MissionConfigSource) -> crew::types::MissionSpecOrigin {
    match source {
        crew::mission_config::MissionConfigSource::User => crew::types::MissionSpecOrigin::UserConfig,
        _ => crew::types::MissionSpecOrigin::Builtin,
    }
}

/// (#1684 Packet 2) `mission launch`'s operator sign-off gate handler — the
/// #1685 spec's "interactive CLI → prompt; non-interactive → blocks
/// pending sign-off". Picked ONCE per `launch` call by checking whether
/// BOTH STDIN and STDOUT are real terminals: `darkmux mission launch
/// pr-merge` typed by a human at a shell gets [`crew::gate::
/// tty_prompt_handler`] (a y/N prompt); anything else — CI, a
/// piped/redirected stdin OR stdout, an ACP-spawned `mission launch <id>`
/// subprocess (the `RoutePlan::Launch` route in `src/acp_panel.rs` —
/// headless by construction) — gets [`crew::gate::refusal_handler`], which
/// fails CLOSED rather than hanging on input that will never arrive.
///
/// **Both streams, not just stdin (#1684 QA CONSIDER).** `darkmux mission
/// launch pr-merge > log.txt` keeps a real tty on stdin while stdout is
/// redirected — checking stdin alone would still pick the tty prompt, but
/// the prompt text itself (written to stdout by `tty_prompt_handler`)
/// would land silently in the redirected file, and the operator watching
/// the terminal would see nothing but an apparent hang. Requiring BOTH
/// streams to be terminals is the cheap fix: a redirected stdout falls
/// through to the non-interactive refusal instead, which is at least
/// LOUD about needing a real terminal.
///
/// Boxed because the two branches are different concrete closure types;
/// `'static` because neither captures anything beyond owned stdio
/// handles.
fn cli_gate_handler() -> Box<crew::gate::GateHandler<'static>> {
    if std::io::stdin().is_terminal() && std::io::stdout().is_terminal() {
        Box::new(crew::gate::tty_prompt_handler())
    } else {
        Box::new(crew::gate::refusal_handler())
    }
}

/// (#1685 QA MUST-FIX 2) Emit the SAME `gh.verb.executed` audit record
/// `acp_panel::run_ephemeral` emits, on THIS entry point too. Before this,
/// `check_cmd` (above, in `launch`) and the args-injection wiring
/// covered this path, but no audit record ever followed — a bare `darkmux
/// mission launch pr-merge` executed the merge, gated only by the tty
/// prompt, with zero trace in `/flow`. Both the docs page ("Every EXECUTED
/// gated command ... emits one flow record") and the feature's own PR
/// body promised the stronger claim; this closes the gap rather than
/// scoping the docs down to match the (narrower) implementation.
///
/// A thin wrapper around `acp_panel::emit_cmd_audit` — same record
/// shape, same `Category::Audit`/`gh.verb.executed` vocabulary, so the two
/// entry points can never drift into two different audit shapes for the
/// same fact. No-op when `config.cmd` is `None` (the ordinary case —
/// every config that isn't an operator-authored GitHub-CLI verb), mirroring
/// `run_ephemeral`'s own "no cmd declared, no audit record" rule.
///
/// `args` is this launcher's own view of the raw panel-argument text (the
/// `--param args=<value>` the operator supplied, or empty if omitted) —
/// the same `__panel_args__` value `inject_panel_args_task_if_referenced`
/// seeds into the graph, so the audited `pr` extraction matches what the
/// graph itself actually saw. `cwd` is the process's own current
/// directory: a direct CLI launch has no separate "session cwd" the way
/// the ACP ephemeral route does (see the docs page's "cwd invariant") — it
/// simply inherits the invoking terminal's cwd, so that's what's audited.
fn emit_launch_cmd_audit(
    config: &MissionConfig,
    collected: &BTreeMap<String, serde_json::Value>,
    mission_id: &str,
    gate_confirmed: Option<bool>,
    success: bool,
) {
    let Some(verb) = config.cmd.as_deref() else { return };
    let args = collected.get("args").and_then(|v| v.as_str()).unwrap_or("");
    let cwd = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
    crate::acp_panel::emit_cmd_audit(verb, args, &cwd, gate_confirmed, success, mission_id);
}

/// (#1877 — "no blind runs" is now PRESCRIBED for the generic launch path,
/// not opt-in per config) Build a whole-run `dispatch *` bookend record —
/// the same coarse liveness edge `mission_launch_review.rs`'s own
/// `review_bookend_record`/`with_dispatch_bookends` gives `review`
/// privately, generalized here so every OTHER config `launch` runs gets
/// it too, unconditionally (see the guard construction in `launch` for
/// why `review` itself never reaches this — it branches out, and mints no
/// `mission_id`, before this code is ever reached).
///
/// `source = "mission"` — distinct from the per-model-dispatch FROZEN
/// `"crew_dispatch"` value (`build_dispatch_record_with_payload`'s own
/// doc — an individual coder/verify/worktree step's OWN dispatch already
/// carries that) and from review's own `"review"`, so the viewer can tell
/// a whole-run bookend apart from either: this is not one model call, and
/// not a review run — it is "did this mission's dispatch work start and
/// finish."
///
/// `pub(crate)` (#1877 QA must-fix 1): `src/acp_panel.rs::run_ephemeral`
/// reuses this SAME builder for its own whole-run bookend pair — the
/// `session_id`/`mission_id` param happens to be a minted per-invocation
/// `correlation_id` there rather than a real mission id, but the shape
/// (config-id handle, no model, `source: "mission"`) is identical, and a
/// second hand-rolled copy is exactly the drift #1685 QA MUST-FIX 2
/// already closed for `emit_cmd_audit`.
pub(crate) fn mission_bookend_record(
    level: flow::Level,
    action: &str,
    config_id: &str,
    mission_id: &str,
    payload: serde_json::Value,
) -> flow::FlowRecord {
    let mut record = crew::dispatch::build_dispatch_record_with_payload(
        level,
        action,
        config_id,
        mission_id,
        None,
        Some(mission_id),
        None,
        Some(payload),
    );
    record.source = Some("mission".to_string());
    record
}

/// Drain every telemetry sample buffered since the last drain and
/// backfill `mission_id` onto each — mirrors `run_step_graph`'s own emit
/// closure's backfill discipline (`record.mission_id.get_or_insert_with`)
/// below, applied to telemetry the same way `mission_launch_review.rs`'s
/// `FleetFlowEmitter` backfills it for `review`'s own samples, so a
/// coder-phase run's telemetry is joinable to its mission in the viewer
/// exactly like review's already is.
///
/// `pub(crate)` (#1877 QA must-fix 1): shared with `acp_panel::
/// run_ephemeral`'s own telemetry drain — same backfill discipline, keyed
/// on that path's minted `correlation_id` instead of a real mission id.
pub(crate) fn drained_telemetry(telemetry: &run_obs::HostTelemetrySampler, mission_id: &str) -> Vec<flow::FlowRecord> {
    telemetry
        .try_drain()
        .into_iter()
        .map(|mut sample| {
            sample.mission_id.get_or_insert_with(|| mission_id.to_string());
            sample
        })
        .collect()
}

/// (#2131 review round 2, item 5) RAII stop-signal for `launch`'s own
/// child-reaping watchdog thread — `Drop` sets the shared flag, so it
/// fires on EVERY exit from `launch` (an early `?`-return, a panic, or
/// the normal fall-through at the bottom) without needing a store call at
/// each of that function's several return points. Without this, the
/// watchdog thread spawned per `launch()` call ran for the rest of the
/// PROCESS's lifetime (an interrupted run's reaping `loop` never exited;
/// a clean run's waiting `while` never got a reason to either) — harmless
/// on a real one-shot-per-invocation CLI process, but ~25 unit tests each
/// leaving a background thread spinning is real, avoidable waste.
struct WatchdogStopGuard(Arc<std::sync::atomic::AtomicBool>);

impl Drop for WatchdogStopGuard {
    fn drop(&mut self) {
        self.0.store(true, std::sync::atomic::Ordering::SeqCst);
    }
}

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

    // (#2301) `crawl` used to be routed by literal id to a bespoke
    // launcher, BEFORE the config load below, because its Task/Step graph
    // was computed at run time and there was no document to execute. There
    // is one now: `crawl.json` declares a `crawl.plan` task per rule, grows
    // a `crawl.unit` task per planned unit from each plan's output (#2300),
    // and closes with a `crawl.summary`. Nothing about a crawl needs a
    // launcher of its own any more, so it takes this path like every other
    // config and `src/crawl_launch.rs` is gone.

    let loaded = mission_config::load(config_id).with_context(|| {
        format!(
            "loading mission config \"{config_id}\" — note: a user-tier copy \
             (~/.darkmux/mission-configs/{config_id}.json) or an on-disk template overrides \
             an embedded built-in; the failing file is named above if one was found"
        )
    })?;
    let config = &loaded.config;

    // (#1685) The command allowlist gate — checked before ANY other work on
    // this config, so a direct `darkmux mission launch <id>` gets the SAME
    // refusal an ACP-panel invocation would (`acp_panel::run_ephemeral`
    // checks the identical `mission_config::check_cmd` first thing).
    // The gate holds regardless of which surface invoked the config.
    if let Some(reason) = mission_config::check_cmd(config) {
        bail!("mission launch: {reason}");
    }

    // (contract 7) Semantic validation is a SEPARATE, explicit pass — this
    // IS the consumption point. `known_kinds` is everything this launcher
    // can actually construct today; anything else warns rather than errors
    // (#1284 Packet 1's own rule — a step kind this call site doesn't
    // recognize isn't necessarily wrong, just not yet reachable through
    // THIS launcher).
    //
    // (#1530 — one global step-kind registry) `registry` is built ONCE here
    // via [`all_step_kinds`] and reused for the rest of this function — both
    // this validation pass AND, further down, the real execution registry
    // `run_step_graph` dispatches through (see the `all_known` check and the
    // `run_step_graph` call site below). Previously each of those three
    // places (validation, the "is this graph executable" check, the
    // execution registry) built or consulted its OWN partial view; now they
    // all resolve against the SAME instance.
    let registry = all_step_kinds()?;
    let known_ids = registry.ids();
    let known_kinds: Vec<&str> = known_ids.iter().map(String::as_str).collect();
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
    //
    // (#1530) Routed STRUCTURALLY (does the graph use review kinds?), not by
    // the literal id `"review"` — the same shape `coder-phase` already uses
    // via `config_uses_coder_phase_kinds`. This is what lets a differently-
    // NAMED review variant (e.g. a stored `review-lean` with fewer probe
    // draws) launch through the same dedicated driver: the driver is already
    // config-driven (`build_review_graph`/`resolve_review_roles` read the
    // graph + every declared `role_id` off the document), so the only thing
    // the old id-literal gated was the NAME, never a capability.
    if config_uses_review_kinds(config) {
        return crate::mission_launch_review::launch(
            config,
            input_file,
            params,
            timeout_seconds,
            spec_origin_for(loaded.source),
        );
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
    //
    // (#1959) `dry_run` is exempt — it's a LAUNCHER-level flag (the CLI's
    // `--dry-run`, injected as a synthetic param), not a config-declared
    // input, same category as `mission_id` being launcher-supplied rather
    // than operator-declared. Every config would otherwise warn on every
    // dry run, since none of them declare it.
    for key in collected.keys() {
        if key != "dry_run" && !config.inputs.iter().any(|i| i.name == *key) {
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

    // (#1685) If the config's graph references the reserved
    // `__panel_args__` task id (the SAME convention the ACP ephemeral
    // route uses for a panel command's raw argument text — see
    // `mission_config::inject_panel_args_task_if_referenced`'s own doc),
    // inject the synthetic task here too, seeded from `--param
    // args=<value>` — BEFORE minting or interpreting — so a direct
    // `darkmux mission launch <id> --param args=<value>` behaves
    // identically to invoking the SAME config from the editor panel.
    // Before this, a config declaring `reads: ["__panel_args__"]` (any
    // panel verb that takes an argument, e.g. a PR number) resolved fine
    // via `darkmux acp` but hard-failed `interpret` on this direct CLI
    // path with "reads unknown task id `__panel_args__`" — nothing here
    // ever injected the task the ACP route always does. A config that
    // never references the reserved id is untouched (the function's own
    // no-op early return), so this has zero effect on every other config.
    let mut config_owned = config.clone();
    mission_config::inject_panel_args_task_if_referenced(
        &mut config_owned,
        collected.get("args").and_then(|v| v.as_str()).unwrap_or(""),
    );
    // (#2299) `enabled: false` is honored HERE, before anything is minted or
    // interpreted: a disabled phase/task/step never exists in the run — no
    // graph node, no record, nothing gray. The ORIGINAL document (flags and
    // all) is what the config snapshot keeps, so provenance is the snapshot
    // plus the `graph-report.json` written beside it; the PRUNED document is
    // what every step below mints from. No CLI override by design — edit the
    // JSON and run.
    // (#2301) `--param rules=<csv>` is the operator's PER-LAUNCH selection,
    // pruned by the same mechanism with its own reason (`not_selected`), so
    // the graph a run shows is exactly the rules that will run — see
    // [`rule_selection`].
    let selection = rule_selection(&collected);
    let config_as_declared: &MissionConfig = &config_owned;
    let (config_pruned, prune_report) = match &selection {
        Some(wanted) => mission_config::prune::prune_with_selection(config_as_declared, &|task| {
            task_declares_rule(task).is_none_or(|rule| wanted.contains(rule))
        }),
        None => mission_config::prune::prune_disabled(config_as_declared),
    };
    let config: &MissionConfig = &config_pruned;

    // (#1284 review round 1, consider 11) A config whose graph uses the
    // coder-phase step kinds needs workdir/branch/base to EXECUTE — check
    // that BEFORE minting anything, so a user-authored config that uses
    // `mission.*` kinds without declaring those inputs (the built-in
    // declares them, so the required-inputs gate above catches it first)
    // doesn't litter a half-launched instance on disk.
    if config_uses_coder_phase_kinds(config) {
        precheck_coder_phase_inputs(config, &collected)?;
    }

    // (#1959) `--dry-run`: everything above this point (config load,
    // command-allowlist gate, semantic validation, panel-args injection,
    // coder-phase input precheck) has already run — a dry run surfaces the
    // SAME loud failures a real launch would. From here, mint NOTHING,
    // emit NO flow records, dispatch NOTHING: print the resolved inputs
    // and the task/step graph, then return.
    if bool_param(&collected, "dry_run") {
        print_dry_run_graph(config, &collected);
        return Ok(0);
    }

    // (#2112 review CONSIDER 3) Power-posture pre-flight (warns on
    // battery/Low Power Mode, refuses on serious/critical thermal unless
    // `--force`) + a held `PreventUserIdleSystemSleep` assertion for this
    // launch's lifetime, released automatically on every exit path via
    // `Drop`. See `src/preflight.rs` and
    // `crates/darkmux-crew/src/sleep_assertion.rs`. Deliberately placed
    // AFTER config load, the command-allowlist gate, semantic validation,
    // and the `--dry-run` short-circuit above (moved here from
    // immediately after the `config_id == "crawl"` routing check) — a
    // typo'd config id now reports "no such config", and `--dry-run`
    // never gets refused on thermal grounds, since neither one is about
    // to do any sustained work this pre-flight exists to protect.
    crate::preflight::check_power_posture(params)?;
    let _sleep_assertion = darkmux_crew::sleep_assertion::SleepAssertion::hold(&format!("darkmux mission {config_id}"));

    // (#2131 review round 4, F2) SIGINT + SIGTERM + SIGHUP — this launcher
    // (generic graphs + coder-phase) previously installed no signal
    // handling at all, the gap #2124 fixed for `mission_launch_review.rs`
    // and #1959 fixed (SIGINT only) for the retired crawl launcher. Installed HERE
    // — ahead of `mint_run_id` below, matching `mission_launch_review.rs`'s
    // `run_dispatch`, which arms before ITS mint too — not merely ahead of
    // the config-snapshot write / interpret / freeform-mint /
    // executable-check work that follows the mint. `mint_run_id` itself is
    // pure in-memory ID derivation (no disk I/O, so a signal caught inside
    // it is harmless today regardless), but arming any later would make
    // "is it safe to be here" a fact the reader has to re-derive from
    // `mint_run_id`'s own implementation rather than something structurally
    // true by placement — the SAME reasoning `mission_launch_review.rs`
    // already applies. The flag is live well before the real-execution
    // section (below) constructs this launcher's own `LaunchFinalizeGuard`.
    crate::launch_guard::arm();

    // Run id: minted fresh for THIS launch, never derived from inputs
    // (#1503). AI work is non-deterministic, so two launches of the same
    // config with the same inputs are two DIFFERENT runs, not one to
    // dedupe/reopen onto — collapsing them onto one id was the category
    // error #1503 fixes. `spec_fingerprint`, computed from the
    // OPERATOR-SUPPLIED inputs below (never `mission_id` itself — hashing it
    // would be circular), is what still lets same-config-same-inputs runs be
    // GROUPED for corpus analysis, via `Mission.spec` — a metadata field,
    // never identity. No `--mission-id` flag needed.
    let mission_id = mint_run_id(config_id)?;
    let spec = MissionSpec {
        config_id: config_id.to_string(),
        inputs_fingerprint: spec_fingerprint(&collected)?,
        // (#1562) Recorded at mint so the board never has to guess — a
        // user-tier config's launches are the operator's named work.
        origin: Some(spec_origin_for(loaded.source)),
    };
    let mut collected = collected;
    if config.inputs.iter().any(|i| i.name == "mission_id") {
        collected.insert("mission_id".to_string(), serde_json::Value::String(mission_id.clone()));
    }

    // (#1504) `ensure_mission_and_phases_with_provenance` itself is a strand
    // window `reconcile_and_finalize_on_error` (below) can't cover — that
    // helper needs a bound `real_phase_ids`, which is exactly what a failure
    // HERE means was never produced (e.g. `save_mission` wrote `mission.json`
    // but a later `save_phase` failed, or `mission_start_with_reasoning`
    // errored after every phase was minted). Left bare, this `?` would strand
    // a fresh, permanently-Active mission per failed attempt — #1503 made
    // every launch mint a UNIQUE id, so a repeated failing launch no longer
    // converges onto one reused instance the way the old derive-from-inputs
    // id used to. `reconcile_mint_failure` closes the mission (which cascades
    // any partially-minted Planned phases to Abandoned via its own #1504
    // reconcile) ONLY if minting got far enough to write `mission.json` in
    // the first place; a failure before that point never touched disk.
    let real_phase_ids = match ensure_mission_and_phases_with_provenance_and_start_payload(
        &mission_id,
        config,
        None,
        Some(spec),
        Some(serde_json::json!({ "graph": prune_report })),
    ) {
            Ok(v) => v,
            Err(e) => {
                crew::lifecycle::reconcile_mint_failure(
                    &mission_id,
                    &format!("mission launch errored during mint: {e:#}"),
                );
                return Err(e);
            }
        };

    // (#1433 follow-up) The mission is now minted (Active, Planned phases) on
    // disk. Every fallible step from here to the scheduler is a strand window:
    // a bare `?` would leave the instance permanently Active with no envelope,
    // the exact drift #1421 closed for the review launcher. This path can't
    // reorder these before the mint (they read/write the minted instance), so
    // each reconciles the just-minted mission to an honest terminal Error
    // status before propagating the failure — same `reconcile_and_finalize_on_
    // error` the scheduler-error path uses, with whatever tasks exist so far
    // (none for a pre-interpret failure → the phases abandon-on-close).
    //
    // config-snapshot.json — ALWAYS written (fresh mint or relaunch
    // overwrite), regardless of whether the graph turns out executable.
    if let Err(e) = crew::lifecycle::save_config_snapshot(&mission_id, config_as_declared)
        .context("persisting config-snapshot.json")
        .and_then(|()| {
            crew::lifecycle::save_graph_report(&mission_id, &prune_report)
                .context("persisting graph-report.json")
        })
    {
        let mut no_steps = BTreeMap::new();
        reconcile_and_finalize_on_error(&mission_id, config, &real_phase_ids, &[], &mut no_steps, &e);
        return Err(e);
    }

    if prune_report.pruned_anything() {
        println!("  {}", style::dim(&format!("graph: {}", prune_report.summary_line())));
    }

    let params = build_launch_params(config, &real_phase_ids, &collected);
    // (#2300) `tasks`/`all_steps` are the STATICALLY DECLARED graph — every
    // task the document mints up front. A task declaring `grow` is a
    // template and is deliberately absent from both: its copies are minted
    // at the phase boundary, from an earlier phase's step output, into the
    // cumulative `steps`/`run_tasks` maps the phase loop below carries.
    let (mut tasks, all_steps, interpret_warnings) =
        match mission_config::interpret(config, &params).context("interpreting mission config graph") {
            Ok(v) => v,
            Err(e) => {
                let mut no_steps = BTreeMap::new();
                reconcile_and_finalize_on_error(&mission_id, config, &real_phase_ids, &[], &mut no_steps, &e);
                return Err(e);
            }
        };
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
    // (#2300) The phase record names the tasks it owns. The generic
    // launcher never wrote it, so the field used to mean different things
    // depending on which launcher minted the run — the precondition #2301
    // needed before folding crawl onto this path (done; the crawl launcher
    // is gone). Growth appends to it at the phase boundary.
    for phase in &config.phases {
        let Some(real_phase_id) = real_phase_ids.get(&phase.id) else { continue };
        let ids: Vec<String> = tasks
            .iter()
            .filter(|t| &t.phase_id == real_phase_id)
            .map(|t| t.id.clone())
            .collect();
        write_phase_task_ids(&mission_id, real_phase_id, ids);
    }

    if tasks.is_empty() {
        // Freeform/manual mission (every phase has zero tasks) — mint + start.
        // (#1463) The per-phase `phase start/complete/abandon` bookkeeping
        // retired; the operator does the work by hand, then `mission finalize`
        // (success) or `mission abort` (kill) closes the mission out.
        println!(
            "{}",
            style::success(&format!(
                "✓ mission `{mission_id}` minted from config \"{config_id}\" — {} freeform phase(s)",
                config.phases.len()
            ))
        );
        println!("  {}", style::dim("no automated graph; the phases to work by hand:"));
        for phase in &config.phases {
            println!(
                "    {}   {}",
                style::accent(&real_phase_ids[&phase.id]),
                style::dim(&format!("— {}", phase.description.as_deref().unwrap_or(&phase.id)))
            );
        }
        println!(
            "  {}",
            style::dim(&format!(
                "when done:  darkmux mission finalize {mission_id}   (or abort: \
                 darkmux mission abort {mission_id})"
            ))
        );
        return Ok(0);
    }

    // (#1530 — one global step-kind registry) `known_kinds`, from the SAME
    // `registry` this function validated `config` against above, is reused
    // here (as `all_known`) rather than re-deriving a second hand-maintained
    // "what can this launcher construct" list.
    let all_known: &[&str] = &known_kinds;
    // (#2300) `declared` is the statically-minted step map; the phase loop
    // below MOVES each phase's steps out of it into the cumulative `steps`
    // map it actually runs, so growth can add to that map between phases.
    let mut declared = all_steps;
    let executable = declared.values().all(|s| all_known.contains(&s.kind.as_str()));
    if !executable {
        for task in &tasks {
            for step_id in &task.step_ids {
                if let Some(step) = declared.get(step_id) {
                    let _ = crew::lifecycle::save_step(&mission_id, &task.phase_id, step);
                }
            }
        }
        let unknown: Vec<&str> = declared
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

    // (#2131) Armed here, right before real execution starts — every exit
    // from this point on (an explicit `close()` call at one of this
    // function's three known terminal points below, an early `?`-return, a
    // panic that unwinds past this point, or a caught SIGTERM/SIGINT/
    // SIGHUP) leaves a matching mission terminal record behind instead of
    // a mission stuck `Active` forever. Deliberately NOT armed any earlier:
    // the freeform-mint (`Ok(0)`, just above) and unexecutable-graph
    // (`Ok(4)`, just above) returns both leave the mission Active ON
    // PURPOSE (freeform: the operator finishes it by hand; unexecutable:
    // nothing was dispatched to abandon) — arming before those would wrongly
    // abort a mission neither return path intends to touch.
    //
    // The abort writer mirrors the existing pre-mint strand-window fallback
    // this function already uses (`reconcile_and_finalize_on_error(...,
    // &[], &mut no_steps, ...)`, e.g. the config-snapshot-write failure just
    // above) — an aborted run's real `tasks`/`steps` state either isn't
    // known yet (a panic before dispatch starts) or can't be trusted (a
    // signal caught mid-dispatch, with the underlying call's children
    // killed out from under it — see the watchdog below), so this always
    // reconciles against an empty step set rather than guessing at partial
    // progress.
    let abort_mission_id = mission_id.clone();
    let abort_config = config_owned.clone();
    let abort_phase_ids = real_phase_ids.clone();
    let abort_config_id = config_id.to_string();
    let mut guard = crate::launch_guard::LaunchFinalizeGuard::new(move || {
        let mut no_steps = BTreeMap::new();
        reconcile_and_finalize_on_error(
            &abort_mission_id,
            &abort_config,
            &abort_phase_ids,
            &[],
            &mut no_steps,
            &anyhow!(
                "mission launch {abort_config_id}: mission aborted — the launcher exited before \
                 a terminal outcome was recorded (a signal, a panic, or an early return this \
                 guard did not expect)"
            ),
        );
    });
    // (#2131) `run_step_graph` (below) is ONE blocking, synchronous call on
    // THIS thread, with no polling seam of its own — the same shape
    // `mission_launch_review.rs`'s dispatch had before #2124, which fixed
    // it there with a supervised worker thread. That shape doesn't collapse
    // cleanly onto this launcher's much larger surface (coder-phase's own
    // gate machinery, the generic-graph persist/emit closures, and the
    // shared `crew::gate::GateHandler` type all assume a single caller
    // thread) without a much bigger, riskier rewrite than this fix
    // warrants — deliberately scoped out (a follow-up, not silently
    // dropped). Instead: a lightweight watchdog thread (no captures, so no
    // `Send`/`'static` concerns at all) that reaps every registered child
    // pid as soon as a signal is observed. Dispatch failures on this
    // generic/coder-phase path never auto-retry (unlike the review funnel's
    // judge stage), so killing the one blocking dispatch's child unblocks
    // `run_step_graph` promptly instead of waiting out its own per-dispatch
    // timeout; the loop keeps reaping (not just once) so a graph with more
    // than one dispatching step can't outrun a single kill. This makes the
    // response genuinely interruptible, just not to review's tighter
    // poll-tick bound.
    //
    // (#2131 review round 2, MUST-FIX 2 — correction) At the time the
    // paragraph above was written, this watchdog was a no-op for exactly
    // the dispatch this launcher actually runs: a coder-phase/crawl step
    // goes through `dispatch_internal.rs`'s docker/agentic container path,
    // which registered NOTHING with `darkmux_types::child_registry` — so
    // `kill_all` here had no pid to signal, and a caught SIGTERM was
    // silently swallowed until the dispatch's own inactivity timeout (a
    // second signal reproduced the pre-#2131 bug). Fixed at the source:
    // `dispatch_internal.rs`'s container spawn site now registers its
    // child pid, and its own trajectory tailer independently polls this
    // SAME `interrupt::is_set()` flag and kills that pid on its own
    // ~250ms cadence — so THIS watchdog is now a genuine backstop (a
    // graph with more than one dispatching step, or a future dispatch
    // path that registers a pid without its own poll seam) rather than
    // the sole mechanism the paragraph above originally described it as.
    //
    // (#2131 review round 2, item 5) Bounded: `watchdog_stop` (set by
    // `WatchdogStopGuard`'s `Drop`, above — fires on every exit from this
    // function) is re-checked in BOTH loops below, so this thread exits
    // once this `launch()` call is over instead of running for the rest
    // of the process's lifetime. And skipped entirely under `cfg(test)` —
    // `cfg!(test)` is true for the WHOLE crate whenever it's built by
    // `cargo test` (not just inside `#[cfg(test)] mod tests`), so none of
    // this module's ~25 unit tests that call `launch()` spin up a
    // watchdog thread they have no use for.
    let watchdog_stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let _watchdog_stop_guard = WatchdogStopGuard(Arc::clone(&watchdog_stop));
    if !cfg!(test) {
        let watchdog_stop = Arc::clone(&watchdog_stop);
        std::thread::spawn(move || {
            while !darkmux_types::interrupt::is_set() {
                if watchdog_stop.load(std::sync::atomic::Ordering::SeqCst) {
                    return;
                }
                std::thread::sleep(std::time::Duration::from_millis(100));
            }
            loop {
                if watchdog_stop.load(std::sync::atomic::Ordering::SeqCst) {
                    return;
                }
                darkmux_types::child_registry::kill_all(darkmux_types::child_registry::SIGKILL);
                std::thread::sleep(std::time::Duration::from_millis(100));
            }
        });
    }

    // Real execution — start every real phase that has tasks, run the
    // scheduler against `registry` (built once, at the top of this function,
    // via `all_step_kinds` — see its own doc; the coder-phase kinds it
    // registers unconditionally are only ever CONSTRUCTED when the graph
    // actually names them, gated by `uses_coder_phase_kinds` below).
    //
    // Generic/coder-phase timeout default: `None` -> 600, matching
    // `mission run`'s own default (see `launch`'s doc — `review` resolves
    // its own 3600 default in `mission_launch_review::launch` instead).
    let timeout_seconds = timeout_seconds.unwrap_or(600);
    let uses_coder_phase_kinds = declared.values().any(|s| CODER_PHASE_TIER3_KINDS.contains(&s.kind.as_str()));
    let coder_handles = if uses_coder_phase_kinds {
        // (#1433 follow-up) Still inside the strand window — a registration
        // failure (a missing input, a mission/phase read error) reconciles the
        // minted mission before propagating, so the graph's tasks abandon
        // honestly rather than leaving the instance stranded Active.
        match register_coder_phase_kinds(
            &registry,
            &mission_id,
            config,
            &real_phase_ids,
            &collected,
            timeout_seconds,
            &mut declared,
        ) {
            Ok(h) => Some(h),
            Err(e) => {
                // (#2131 review round 2, MUST-FIX 1) Routed through
                // `guard.close`, matching the scheduler-error and
                // failure-before-gate arms below — this call already
                // writes the same informative terminal record `Drop`'s
                // abort writer would, so `guard` must disarm here too.
                // Without this, the still-armed guard's `Drop` ran the
                // abort writer a SECOND time on unwind, and
                // `finalize_mission` (inside `reconcile_and_finalize_on_error`)
                // overwrote `envelope.json` unconditionally — clobbering the
                // real registration error, the reconciled-step warning, and
                // the completed/errored step detail with the generic
                // "launcher exited before a terminal outcome was recorded"
                // abort text.
                guard.close(|| {
                    reconcile_and_finalize_on_error(&mission_id, config, &real_phase_ids, &tasks, &mut declared, &e)
                });
                return Err(e);
            }
        }
    } else {
        None
    };

    // (#1877 "no blind runs" — telemetry + the whole-run dispatch bookend
    // are PRESCRIBED here, not opt-in) Constructed unconditionally, for
    // EVERY config that reaches this point — regardless of whether its
    // graph declares a model-dispatching step kind or is Tier-1-only
    // procedural/shell work (a `cmd` panel config gets telemetry too;
    // the "was anything actually sampled" question is answered by the
    // run's real wall-clock against the production cadence below, same as
    // it already is for `review`). `review` itself NEVER reaches this
    // line: `config_uses_review_kinds` branches it out, and mints no
    // `mission_id`, far above (before this function's own `--input`/
    // `--param` collection even runs) — review builds its own telemetry
    // sampler and its own `with_dispatch_bookends` privately
    // (`mission_launch_review.rs` / `darkmux_lab::lab::review::
    // run_review_graph`), already satisfying the mandate on its own. The
    // two constructions are mutually exclusive by CONTROL FLOW, not by a
    // runtime check — there is no code path that reaches both, so no
    // double-sampling is possible by construction.
    let telemetry = run_obs::HostTelemetrySampler::start(
        mission_id.clone(),
        config_id.to_string(),
        run_obs::DEFAULT_TELEMETRY_INTERVAL,
        run_obs::DEFAULT_TELEMETRY_POLL,
        crew::telemetry_sampler::sample_host,
        darkmux_profiles::lms::list_loaded,
    );
    let mut dispatch_sink = |record: flow::FlowRecord| {
        let _ = flow::record(record);
    };
    let mission_id_for_abort = mission_id.clone();
    let config_id_for_abort = config_id.to_string();
    // `BookendGuard`'s Drop fires this `on_abort` closure — building the
    // "dispatch error" abort record — for any exit between `open()` below
    // and a matching `close()`: an early `?`-return this function doesn't
    // already reconcile explicitly, or a genuine panic (contract 2 — RAII-
    // guarded on all exit paths). Every KNOWN exit point below (the
    // scheduler error, the coder-phase gate, and the gate-less finish)
    // calls `bookend.close(...)` explicitly with the real outcome; this is
    // strictly the backstop for the unexpected case.
    //
    // (#1877 QA should-fix, accepted gap — named here, not silently) `on_
    // abort` builds exactly ONE record; it has no way to also drain and
    // forward whatever `telemetry` has buffered since the last explicit
    // drain (`BookendGuard`'s `on_abort` signature is `Fn(&str, &str) ->
    // FlowRecord`, not `FnMut(&mut dyn BookendSink)`). `RunObs` (used by
    // review's sequential path) narrows this to "at most one final-tick
    // sample" because it drains before every record it emits; this bare-
    // sampler construction only drains at the 3 known exit points, so an
    // abort via this backstop loses everything buffered since the last of
    // those — up to one telemetry interval's worth on a run that panics
    // mid-dispatch. Best-effort telemetry on an already-exceptional path;
    // the bookend's own liveness record (the actual contract-2 obligation)
    // is unaffected either way.
    let mut bookend = flow::BookendGuard::new(&mut dispatch_sink, move |_id, _kind| {
        mission_bookend_record(
            flow::Level::Error,
            "dispatch error",
            &config_id_for_abort,
            &mission_id_for_abort,
            serde_json::json!({
                "runtime": "mission",
                "result_class": "error",
                "error": "mission dispatch terminated before completion (early return or panic)",
            }),
        )
    });
    bookend.open(
        "dispatch",
        "dispatch",
        mission_bookend_record(
            flow::Level::Info,
            "dispatch start",
            config_id,
            &mission_id,
            serde_json::json!({ "runtime": "mission" }),
        ),
    );

    // (#1503) The #1400 preflight that used to run here — warning that a
    // phase was already terminal-Complete from a prior finalized run — only
    // ever mattered on the reuse/reopen path: a freshly-minted mission's
    // phases are always `Planned` (`ensure_mission_and_phases_with_provenance`
    // mints unconditionally now, never reopens an existing record), so the
    // condition it checked for can no longer occur. Removed with the reuse
    // path itself.

    // (#2300) CUMULATIVE, not the whole graph up front. The phase loop
    // below adds one phase's tasks (declared, then grown) before running
    // it, so `gather_inputs` still sees every earlier phase's completed
    // steps — that is what carries a producing step's output across the
    // boundary — while the scheduler only ever has THIS phase's `Planned`
    // steps to pick up.
    let mut tasks_by_id: BTreeMap<String, crew::types::Task> = BTreeMap::new();
    let mut steps: BTreeMap<String, crew::types::Step> = BTreeMap::new();
    // Document task id -> real task id(s), for resolving a grown copy's
    // inherited `depends_on`/`reads` at the boundary.
    let grow_real_ids = mission_config::interpret::real_task_ids(config, &params);
    let mut grown_events: Vec<crew::mission_config::grow::Grown> = Vec::new();
    let facts = crew::step_kinds::Facts::default();
    let est = crew::step_kinds::FixedEstimator::default();
    // (#1400) Tracks which phases this dispatch has already lazy-started —
    // see `lazy_start_phase_for_step`'s doc.
    let mut started_phases: std::collections::HashSet<String> = std::collections::HashSet::new();
    // (#1632) The counterpart #1620 gave the review launcher and not this one.
    // Phase order comes from the CONFIG's own declaration, so it matches the
    // strictly-linear order (#1341) the close logic depends on — map iteration
    // would not.
    let phase_order: Vec<String> = config
        .phases
        .iter()
        .filter_map(|p| real_phase_ids.get(&p.id).cloned())
        .collect();
    let mut closed_phases: std::collections::HashSet<String> = std::collections::HashSet::new();
    // (#1530 Packets 2/3b-1) The coder-phase kinds' two structured-result
    // slots, plus the run's identity context, seeded onto the run-scoped
    // `ArtifactBus` via `run_step_graph`'s caller-seed path — `coder_handles`
    // already OWNS the `Arc` clones (`CoderPhaseHandles::coder_slot`/
    // `verify_slot`/`context`, minted in `register_coder_phase_kinds`), so
    // this is a pure hand-off: the SAME instance the kinds write into (via
    // `StepRunCtx::artifact`) is what `coder_phase_gate_outcome` reads back
    // below, directly off `handles`, once the graph returns. Empty (`&[]`) on
    // every non-coder-phase graph — zero behavior change there, mirroring
    // `run_review_graph`'s own `seed_artifacts` shape (#1530 Packet 1).
    let seed_artifacts: Vec<(&'static str, Arc<dyn Any + Send + Sync>)> = match &coder_handles {
        Some(h) => vec![
            (coder_phase::CODER_RESULT_ARTIFACT, h.coder_slot.clone() as Arc<dyn Any + Send + Sync>),
            (
                coder_phase::CODER_VERIFY_RESULT_ARTIFACT,
                h.verify_slot.clone() as Arc<dyn Any + Send + Sync>,
            ),
            (coder_phase::CODER_CONTEXT_ARTIFACT, h.context.clone() as Arc<dyn Any + Send + Sync>),
        ],
        None => Vec::new(),
    };
    // (#1397) `persist` durably saves each step at ITS OWN transition
    // (Running at dispatch, Complete/Error at completion), not just at the
    // end of the whole run — see `run_step_graph`'s own doc. The phase id
    // isn't on `Step` itself, so it's resolved per-call from the owning
    // Task via `tasks_by_id` (borrowed here, alongside the scheduler's own
    // immutable borrow of the same map — both read-only, no conflict). The
    // bulk save loop right after this call stays in place as a cheap,
    // idempotent final reconcile.
    // (#1684 Packet 2) Picked once per launch — see `cli_gate_handler`'s
    // own doc. Only ever actually INVOKED if this graph contains a step
    // declaring `"gate": "operator"` (`gate::resolve_gate` never calls it
    // for an ungated step), which today means an operator-authored
    // panel-verb config (e.g. the documented `pr-merge` example) launched
    // directly rather than through the ACP ephemeral route.
    //
    // (#1685 QA MUST-FIX 2) Wrapped the same way `acp_panel::run_ephemeral`
    // wraps its own gate handler: whether the operator actually CONFIRMED a
    // gated step is one of the facts `emit_launch_cmd_audit` records, so
    // it has to be observable AFTER `run_step_graph` returns (the closure
    // below is the only place that decision is made). `Rc<Cell<..>>`, not a
    // captured `&mut`, for the same reason `run_ephemeral` uses one: the
    // closure's last use is the `run_step_graph` call itself, so the borrow
    // checker would tolerate a `&mut` too, but a `Cell` keeps the intent
    // (read this back once the graph is done) obvious at the read site.
    // Stays `None` for the whole run when no gated step ever executes (a
    // read-only verb, or a config with no `gate: "operator"` step at all)
    // so the audit record honestly says "no gate" rather than fabricating a
    // yes/no for a decision that never happened.
    let gate_confirmed: std::rc::Rc<std::cell::Cell<Option<bool>>> =
        std::rc::Rc::new(std::cell::Cell::new(None));
    type BoxedGateHandler<'a> = Box<dyn FnMut(&Step, &BTreeMap<String, String>) -> crew::gate::GateDecision + 'a>;
    let mut instrumented_gate: BoxedGateHandler<'static> = {
        let mut handler = cli_gate_handler();
        let flag = gate_confirmed.clone();
        Box::new(move |step: &Step, facts: &BTreeMap<String, String>| {
            let decision = handler(step, facts);
            flag.set(Some(matches!(decision, crew::gate::GateDecision::Approved)));
            decision
        })
    };
    // (#2300) ONE `run_step_graph` call PER PHASE, in config order —
    // previously one call over the whole interpreted graph. The scheduler
    // takes its task map by shared reference, so the graph cannot grow
    // mid-run; the phase boundary is the only place a task minted from a
    // step's OUTPUT can enter. **Trade, stated plainly:** cross-phase
    // parallelism is gone. Two phases whose tasks have no edge between
    // them used to be able to overlap; now phase N+1 starts only after
    // phase N's last step settles. That is acceptable because phases are
    // sequential by design in this codebase already (`phase_order` /
    // `lazy_close_prior_phases` assume a strictly linear order, #1341),
    // and parallelism lives INSIDE a phase, where the wave scheduler runs
    // every independent task concurrently exactly as before.
    let mut graph_result: Result<crew::scheduler::SchedulerReport> =
        Ok(crew::scheduler::SchedulerReport::default());
    for phase in &config.phases {
        let Some(real_phase_id) = real_phase_ids.get(&phase.id).cloned() else {
            continue;
        };

        // Growth first: a template's copies must exist before this phase's
        // tasks are handed to the scheduler.
        match grow_phase(
            phase,
            &real_phase_id,
            &grow_real_ids,
            &tasks_by_id,
            &steps,
            all_known,
        ) {
            Ok(grown) => {
                for (event, grown_tasks, grown_steps) in grown {
                    if event.minted.is_empty() {
                        // A plan that planned nothing is a real outcome,
                        // not a failure: the phase simply has no work.
                        println!(
                            "  {}",
                            style::dim(&format!(
                                "graph: `{}` grew nothing from `{}` (0 items) — phase `{}` has \
                                 no work (grew_nothing)",
                                event.task_template, event.from, real_phase_id
                            ))
                        );
                    } else {
                        println!(
                            "  {}",
                            style::dim(&format!(
                                "graph: grew {} task(s) from `{}` into phase `{}`",
                                event.minted.len(),
                                event.from,
                                real_phase_id
                            ))
                        );
                    }
                    // (#2300) `reason` is OMITTED, never `null`, when the
                    // growth minted something — a key present with a null
                    // value reads as "there was a reason and it was
                    // unknown", which is not what happened.
                    let mut payload = serde_json::json!({
                        "phase": event.phase,
                        "task_template": event.task_template,
                        "from": event.from,
                        "source": event.source,
                        "items": event.items,
                        "minted": event.minted,
                    });
                    if event.minted.is_empty() {
                        payload["reason"] = serde_json::json!("grew_nothing");
                    }
                    bookend.emit_now(mission_bookend_record(
                        flow::Level::Info,
                        "mission.grow",
                        config_id,
                        &mission_id,
                        payload,
                    ));
                    for task in &grown_tasks {
                        if let Err(e) = crew::lifecycle::save_task(&mission_id, task) {
                            eprintln!(
                                "{}",
                                style::dim(&format!("mission launch: grown task persist warning: {e:#}"))
                            );
                        }
                    }
                    for step in grown_steps.values() {
                        if let Err(e) = crew::lifecycle::save_step(&mission_id, &real_phase_id, step) {
                            eprintln!(
                                "{}",
                                style::dim(&format!("mission launch: grown step persist warning: {e:#}"))
                            );
                        }
                    }
                    // (#2300) The phase record names the tasks it owns.
                    // The mint already wrote this phase's declared task ids
                    // (see `write_phase_task_ids` at the mint), so growth
                    // APPENDS — the field then reads the same on this
                    // launcher as it did on the retired crawl launcher,
                    // which builds `task_ids` per group and saves the phase
                    // (the one launcher that has always written it). #2301
                    // folds the two into one path; the field has to mean
                    // the same thing on both before that can happen.
                    append_phase_task_ids(&mission_id, &real_phase_id, &event.minted);
                    for task in grown_tasks {
                        tasks_by_id.insert(task.id.clone(), task.clone());
                        tasks.push(task);
                    }
                    steps.extend(grown_steps);
                    grown_events.push(event);
                }
            }
            Err(e) => {
                graph_result = Err(e);
                break;
            }
        }

        // Then this phase's statically-declared tasks, moved out of the
        // mint-time map into the cumulative one the scheduler runs.
        for task in tasks.iter().filter(|t| t.phase_id == real_phase_id) {
            if tasks_by_id.contains_key(&task.id) {
                continue;
            }
            for step_id in &task.step_ids {
                if let Some(step) = declared.remove(step_id) {
                    steps.insert(step_id.clone(), step);
                }
            }
            tasks_by_id.insert(task.id.clone(), task.clone());
        }

        let phase_has_work = steps.values().any(|s| {
            s.status == crew::types::NodeStatus::Planned
                && tasks_by_id.get(&s.task_id).is_some_and(|t| t.phase_id == real_phase_id)
        });
        if !phase_has_work {
            // (#2300) A phase that declared a `grow` template and grew
            // NOTHING has no steps at all, and phases only ever open and
            // close through `lazy_start_phase_for_step`/
            // `lazy_close_prior_phases`, which are driven BY a step. So
            // such a phase would sit `Planned` for the whole run and get
            // swept to `Abandoned` by the #1504 backstop at finalize —
            // printing a reconcile warning and recording a lie, because
            // nothing failed: the plan planned nothing. Start and complete
            // it explicitly here instead. Scoped to phases that actually
            // declared growth: a freeform phase with no tasks in the
            // document is a different thing (the operator works it by
            // hand) and keeps its existing behavior untouched.
            if phase.tasks.iter().any(|t| t.grow.is_some()) {
                complete_grown_nothing_phase(
                    &mission_id,
                    &real_phase_id,
                    &phase_order,
                    &mut started_phases,
                    &mut closed_phases,
                );
            }
            // Nothing to run in this phase — skip the scheduler setup
            // rather than spinning an empty wave.
            continue;
        }

        graph_result = crew::scheduler::run_step_graph(
        &mut steps,
        &tasks_by_id,
        &registry,
        &facts,
        &est,
        1,
        &crew::concurrent_dispatch::lms_host_factory,
        // (#1641) The launcher knows its own `mission_id` — stamp it onto
        // every record this run's `run_step_graph` call produces that
        // doesn't already carry a more specific one (`get_or_insert`, never
        // overwrite). Without this, the scheduler's generic step-lifecycle
        // records (`step_lifecycle_record`, `crates/darkmux-crew/src/
        // scheduler.rs`) and any `StepKind`'s own `mission_id: None`-
        // stamped records (e.g. `dispatch.map`'s per-item "step result")
        // carry NO instance-scoped identity, so two missions launched from
        // the SAME config collide on their shared CONFIG-scoped
        // `session_id`/`handle` (e.g. `task-review-probe-mid-task` /
        // `review-probe-mid-step`) in the viewer — one mission's step
        // absorbing another's token counts, a dead step resurrected to
        // "running" by the other mission's heartbeat.
        &mut |mut record| {
            record.mission_id.get_or_insert_with(|| mission_id.clone());
            // (#1877) Drain before emit — same interleaving discipline
            // `run_review_graph`'s own emit closure uses, so telemetry
            // streams alongside the run's other records rather than
            // batching at the end (CLAUDE.md's "no blind runs" mandate).
            // Calls `flow::record` directly here (not `bookend.emit_now`)
            // because `bookend` isn't reachable from inside this closure
            // (it's constructed on the outer scope, and this closure is
            // handed to `run_step_graph` by itself) — same underlying sink
            // either way (`dispatch_sink` IS `flow::record`), so this is a
            // borrow-driven split, not two different destinations.
            for sample in drained_telemetry(&telemetry, &mission_id) {
                let _ = flow::record(sample);
            }
            let _ = flow::record(record);
        },
        &mut |step| {
            let phase_id = tasks_by_id
                .get(&step.task_id)
                .map(|t| t.phase_id.as_str())
                .unwrap_or_default();
            // (#1632) Without this the GENERIC launcher — every non-review
            // config, including coder-phase and anything from `mission propose`
            // — started phases and never closed them, so its board row read
            // `0/N` for the whole run and then jumped to `N/N`. Exactly the
            // #1620 defect, fixed there for the review launcher only.
            if lazy_start_phase_for_step(&mission_id, phase_id, step.status, &mut started_phases) {
                lazy_close_prior_phases(&mission_id, &phase_order, phase_id, &mut closed_phases);
            }
            if let Err(e) = crew::lifecycle::save_step(&mission_id, phase_id, step) {
                eprintln!(
                    "{}",
                    style::dim(&format!("mission launch: step persist warning (transition): {e:#}"))
                );
            }
        },
        Some(instrumented_gate.as_mut()),
        None,
        &seed_artifacts,
        );
        if graph_result.is_err() {
            break;
        }
    }

    // (#2300) Growth happens long after the mint, so `graph-report.json`
    // (written at mint time, #2299) is loaded, appended to and saved —
    // never rewritten from the prune report, which would drop anything a
    // concurrent writer added.
    if !grown_events.is_empty() {
        match crew::lifecycle::load_graph_report(&mission_id) {
            Ok(Some(mut report)) => {
                report.grown.extend(grown_events.iter().cloned());
                if let Err(e) = crew::lifecycle::save_graph_report(&mission_id, &report) {
                    eprintln!("{}", style::dim(&format!("mission launch: graph-report warning: {e:#}")));
                }
            }
            Ok(None) => {}
            Err(e) => {
                eprintln!("{}", style::dim(&format!("mission launch: graph-report read warning: {e:#}")));
            }
        }
    }

    // (#1406, F4) A scheduler-level `Err` mid-run would otherwise `?`-return
    // here with NO finalize, stranding the mission Active with `Running`
    // phases + steps forever. Reconcile the stranded steps and drive the
    // mission to a terminal Error status BEFORE propagating the failure.
    // The failure is still surfaced to the caller (loud, non-zero exit); the
    // mission board just no longer lies about a dead run being active.
    if let Err(e) = graph_result {
        // (#1877) Explicit close, not the Drop backstop — a scheduler
        // error is a KNOWN outcome with real error text worth carrying,
        // same as `with_dispatch_bookends`'s own `Err` arm. (#2131) Now
        // routed through `guard.close` so the `LaunchFinalizeGuard`
        // armed above disarms here too — this call already wrote the
        // SAME terminal record `Drop`'s abort writer would have, so this
        // is the intentional, informative path, not the fallback.
        guard.close(|| {
            reconcile_and_finalize_on_error(&mission_id, config, &real_phase_ids, &tasks, &mut steps, &e)
        });
        for sample in drained_telemetry(&telemetry, &mission_id) {
            bookend.emit_now(sample);
        }
        bookend.close(
            "dispatch",
            mission_bookend_record(
                flow::Level::Error,
                "dispatch error",
                config_id,
                &mission_id,
                serde_json::json!({
                    "runtime": "mission",
                    "result_class": "error",
                    "error": e.to_string(),
                }),
            ),
        );
        // (#1685 QA MUST-FIX 2) A scheduler-level failure is still "the
        // operator's session tried to act as them" — audit-worthy on its
        // own (see `acp_panel::emit_cmd_audit`'s doc on why a failed
        // attempt is never silently dropped from the trail).
        emit_launch_cmd_audit(config, &collected, &mission_id, gate_confirmed.get(), false);
        // (#2131) The terminal record above is already durable — a no-op
        // unless a signal was actually observed, in which case this reaps
        // every child the watchdog above may not have caught yet and
        // exits with the conventional signal-terminated code.
        crate::launch_guard::reap_and_exit_on_signal();
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
    // semantics: stop at the operator sign-off gate — phase stays Running,
    // mission stays Active, NO finalize_mission. `mission finalize` finishes
    // the loop from here (#1463). See [`gate_outcome_reached_no_gate`] for the one
    // exception class (failure exits that never reached a reviewable gate).
    if let Some(handles) = &coder_handles {
        let outcome = coder_phase_gate_outcome(&mission_id, handles, &steps, &registry);
        if gate_outcome_reached_no_gate(&outcome) {
            let e = match &outcome {
                Err(e) => anyhow!("{e:#}"),
                _ => anyhow!("coder dispatch failed before a reviewable gate (exit 1)"),
            };
            // (#2131) Disarms `guard` — this is a KNOWN failure-before-gate
            // outcome with real error text, the same shape the scheduler-
            // error branch above uses.
            guard.close(|| {
                reconcile_and_finalize_on_error(&mission_id, config, &real_phase_ids, &tasks, &mut steps, &e)
            });
        } else {
            // (#2131) The gate WAS reached — the mission stays Active on
            // purpose (operator sign-off is still pending; `mission
            // finalize`/`mission abort` closes the loop later, per this
            // function's own doc above). Disarm without writing anything —
            // `Drop`'s abort writer must never abandon a mission that
            // reached its gate cleanly.
            guard.close(|| {});
        }
        // (#1685 QA MUST-FIX 2) `0` mirrors this function's own exit-code
        // doc above ("`0` — ... coder ran and QA came back clean/flags-only
        // ... gate banner printed"): the coder-phase path never actually
        // pairs with a `cmd` config in practice (the built-in verbs are
        // all `procedural.shell`-only), but the audit call is unconditional
        // here for the same reason `check_cmd` runs before `launch`
        // branches at all — the gate holds regardless of which graph shape
        // a `cmd` config happens to declare.
        let success = matches!(&outcome, Ok(code) if *code == 0);
        // (#1877, QA must-fix; extraction #1877 QA must-fix 3) Explicit
        // close on the coder branch's early return — a coder-phase run
        // doing real dispatch work needs a terminal record, not just the
        // Drop backstop. `success` still gates the command-gate AUDIT call
        // below — that one genuinely wants exit-code success, not
        // gate-reached; see `coder_branch_terminal_bookend`'s own doc for
        // why the bookend record itself keys on `reached_gate` instead.
        let (_reached_gate, record) = coder_branch_terminal_bookend(&outcome, config_id, &mission_id);
        for sample in drained_telemetry(&telemetry, &mission_id) {
            bookend.emit_now(sample);
        }
        bookend.close("dispatch", record);
        emit_launch_cmd_audit(config, &collected, &mission_id, gate_confirmed.get(), success);
        // (#2131) A no-op unless a signal was actually observed — see the
        // scheduler-error branch above for what this does when one was.
        //
        // (#2131 review round 2, item 6) When a signal WAS observed AND
        // the gate was reached (the `else` branch just above — `guard`
        // disarmed WITHOUT writing anything), this still exits 130 with
        // the mission left `Active` — on purpose, not a gap. The gate
        // outcome is real, earned work (the operator's sign-off is
        // genuinely still pending, signal or no signal); reaping +
        // hard-exiting here does not un-earn it. `mission finalize` (or
        // `mission abort`) is how the operator closes the loop from
        // here regardless of whether THIS invocation ended by a signal
        // or by simply returning after printing the gate banner — same
        // as any other gated run.
        crate::launch_guard::reap_and_exit_on_signal();
        return outcome;
    }

    // Gate-less generic graph (Tier-1-only kinds) — the standard
    // MissionEnvelope finalization applies: every run reaches a terminal
    // phase/mission status (Packet 2's own doctrine for gate-free work).
    let envelope = build_envelope(&mission_id, config, &real_phase_ids, &tasks, &steps);
    let status = envelope.status;
    // (#2301) A run's own numbers ride the `mission close` payload — the
    // home the retired crawl launcher used, kept for every generic graph
    // that ends in a summarizing step. See [`run_summary_payload`].
    let close_payload = run_summary_payload(config, &real_phase_ids, &tasks, &steps);
    // (#2131) Disarms `guard` — the third and last known terminal point.
    guard.close(|| crew::envelope::finalize_mission_with_payload(&envelope, close_payload));

    print_run_summary(&mission_id, &steps);

    use crew::envelope::MissionOutcomeStatus;
    // (#1877 item 4 — deliberately deferred, corrected) `build_envelope`
    // (above, only reached for a gate-less generic Tier-1-only graph — a
    // coder-phase config's `coder_handles` branch above `return`s before
    // this point, either into `reconcile_and_finalize_on_error` on a
    // pre-gate failure or by stopping at the operator gate with no
    // finalize at all) still constructs `status` directly, never a
    // `RunOutcome`. An earlier version of this note reasoned that per-step
    // aggregation is "a different shape" from review's per-flag docket —
    // that reasoning was wrong: `build_envelope`'s all-errored arm
    // (`completed.is_empty()` → `MissionOutcomeStatus::Error`) already IS
    // a docket-style verdict (no step produced usable output), which is
    // semantically `RunOutcome::Empty`, not `Error`.
    //
    // The REAL blocker is `MissionOutcomeStatus::from_outcome`
    // (`crates/darkmux-crew/src/envelope.rs`): it maps `Complete`/
    // `Partial`/`Empty` and has no route to `Error` at all. Adopting
    // `RunOutcome` here would silently change the all-errored arm's status
    // from `Error` to `Degenerate` (`Empty` mapped through `from_outcome`)
    // — a real status/exit-code change for that input shape, not a pure
    // refactor. So this match only ever sees `status` directly, never
    // `outcome`, and stays that way until `from_outcome` grows a fourth
    // arm (or this site gets a documented reason to keep constructing
    // `Error` by hand). What's unaffected regardless: `from_outcome`
    // already collapses a future `RunOutcome::Partial` into `Degraded`,
    // which already exits 0 here — a mission driver that adopts
    // `RunOutcome` for its Partial case inherits this exit code for free,
    // with no match arm to add.
    let exit_code = match status {
        MissionOutcomeStatus::Clean | MissionOutcomeStatus::Degraded => 0,
        // (#1881) `status` here is `build_envelope`'s OWN freshly-computed
        // value, never a deserialized one, so `Unknown` (a
        // deserialize-only forward-compat fallback) is unreachable in
        // practice. Kept exhaustive with a conservative failing exit code
        // rather than a wildcard, so a future caller that DOES pass a
        // loaded status here fails loudly instead of silently exiting 0.
        MissionOutcomeStatus::Degenerate | MissionOutcomeStatus::Error | MissionOutcomeStatus::Unknown => 1,
    };
    // (#1685 QA MUST-FIX 2) This is the branch the documented `pr-list` /
    // `pr-info` / `pr-approve` / `pr-merge` example verbs actually take —
    // every one of them is a Tier-1-only `procedural.shell`/`procedural.noop`
    // graph, so this is where a direct `darkmux mission launch pr-merge`
    // gets its audit record in practice.
    emit_launch_cmd_audit(config, &collected, &mission_id, gate_confirmed.get(), exit_code == 0);
    // (#1877) Explicit close on the gate-less generic finish — the third
    // and last KNOWN exit this guard covers.
    for sample in drained_telemetry(&telemetry, &mission_id) {
        bookend.emit_now(sample);
    }
    bookend.close(
        "dispatch",
        mission_bookend_record(
            if exit_code == 0 { flow::Level::Info } else { flow::Level::Error },
            if exit_code == 0 { "dispatch complete" } else { "dispatch error" },
            config_id,
            &mission_id,
            serde_json::json!({
                "runtime": "mission",
                "result_class": if exit_code == 0 { "ok" } else { "error" },
                "status": format!("{status:?}"),
            }),
        ),
    );
    // (#2131) A no-op unless a signal was actually observed.
    crate::launch_guard::reap_and_exit_on_signal();
    Ok(exit_code)
}

/// (#2300) Expand every `grow` template a phase declares, from the output
/// an EARLIER phase's producing task already wrote.
///
/// Returns one triple per template: the provenance event, the real Tasks,
/// and the real Steps. A template that grows ZERO tasks still returns an
/// event (with an empty `minted`) — a plan that planned nothing is a
/// recorded outcome, never a silent no-op.
///
/// Every failure mode here is an `Err` naming the task AND the path, never
/// a panic and never a quiet zero: the producer never ran, produced no
/// output, named a path that isn't there, or wrote something that isn't
/// the JSON shape the template asked for.
type GrowthBatch = (
    crew::mission_config::grow::Grown,
    Vec<crew::types::Task>,
    BTreeMap<String, crew::types::Step>,
);

fn grow_phase(
    phase: &mission_config::PhaseConfig,
    real_phase_id: &str,
    real_task_ids: &BTreeMap<String, Vec<String>>,
    tasks_by_id: &BTreeMap<String, crew::types::Task>,
    steps: &BTreeMap<String, crew::types::Step>,
    all_known: &[&str],
) -> Result<Vec<GrowthBatch>> {
    let mut out: Vec<GrowthBatch> = Vec::new();
    for task_cfg in &phase.tasks {
        let Some(spec) = &task_cfg.grow else { continue };

        let real_from = real_task_ids
            .get(&spec.from)
            .and_then(|ids| ids.first())
            .ok_or_else(|| {
                anyhow!(
 "mission launch: task `{}` grows from `{}`, which minted no task in this run (pruned by `enabled: \
  false`, or itself a `grow` template)",
                    task_cfg.id,
                    spec.from
                )
            })?;
        let producer = tasks_by_id.get(real_from).ok_or_else(|| {
            anyhow!(
 "mission launch: task `{}` grows from `{}` (real id `{real_from}`), which has not run — a `grow.from` \
  must name a task in an EARLIER phase",
                task_cfg.id,
                spec.from
            )
        })?;
        let last_step_id = producer.step_ids.last().ok_or_else(|| {
            anyhow!(
 "mission launch: task `{}` grows from `{}`, which has no steps and so produces no output",
                task_cfg.id,
                spec.from
            )
        })?;
        let last_step = steps.get(last_step_id).ok_or_else(|| {
            anyhow!(
 "mission launch: task `{}` grows from `{}`, whose last step `{last_step_id}` is not in this run's \
  graph",
                task_cfg.id,
                spec.from
            )
        })?;
        if last_step.status != crew::types::NodeStatus::Complete {
            bail!(
 "mission launch: task `{}` grows from `{}`, whose last step `{last_step_id}` ended {:?}, not Complete \
  — nothing was produced to grow from",
                task_cfg.id,
                spec.from,
                last_step.status
            );
        }
        let from_output = last_step
            .output
            .as_deref()
            .map(str::trim)
            .filter(|o| !o.is_empty())
            .ok_or_else(|| {
                anyhow!(
 "mission launch: task `{}` grows from `{}`, whose last step `{last_step_id}` completed with no output — a producing step's output must be the PATH to the \
                     JSON artifact to grow from",
                    task_cfg.id,
                    spec.from
                )
            })?
            .to_string();

        // (#2301) A producing step's output is inline JSON, a `ref`
        // naming a file, or a bare path — `resolve_output_doc` reads all
        // three, and `items_from_artifact` looks inside a typed envelope's
        // `body` when it finds one.
        let (doc, whence) = crew::step_output::resolve_output_doc(&from_output).with_context(|| {
            format!(
 "mission launch: task `{}` grows from `{}`, whose output names `{from_output}`",
                task_cfg.id, spec.from
            )
        })?;
        let items = crew::mission_config::items_from_artifact(&doc, &spec.items, &whence)
            .with_context(|| format!("mission launch: growing task `{}`", task_cfg.id))?;

        // `{{from.output}}` renders the producer's output VERBATIM — a
        // consumer reads it through `Output::read`, which takes a `ref`, a
        // bare path or inline JSON alike.
        let growth = crew::mission_config::grow_task(task_cfg, spec, items, &from_output)
            .with_context(|| format!("mission launch: growing task `{}`", task_cfg.id))?;
        let (grown_tasks, grown_steps) = mission_config::interpret::interpret_grown(
            &growth.tasks,
            &phase.id,
            real_phase_id,
            real_task_ids,
        )
        .with_context(|| format!("mission launch: interpreting grown copies of `{}`", task_cfg.id))?;

        // Same gate the statically-declared graph passes before it runs —
        // a grown step naming a kind this binary can't construct must fail
        // loudly here, not deep inside the scheduler.
        if let Some(bad) = grown_steps.values().find(|s| !all_known.contains(&s.kind.as_str())) {
            bail!(
 "mission launch: task `{}` grew step `{}` of kind `{}`, which this launcher cannot construct",
                task_cfg.id,
                bad.id,
                bad.kind
            );
        }

        out.push((
            crew::mission_config::grow::Grown {
                phase: real_phase_id.to_string(),
                task_template: task_cfg.id.clone(),
                from: spec.from.clone(),
                // (#2301) The RESOLVED name, not the raw output string —
                // a wrapped producer's output is a `{"ref": …}` pointer.
                source: whence,
                items: items.len(),
                minted: grown_tasks.iter().map(|t| t.id.clone()).collect(),
            },
            grown_tasks,
            grown_steps,
        ));
    }
    Ok(out)
}

/// (#2300) Set a phase record's `task_ids` to exactly `ids`.
///
/// Warn-and-continue, never fatal: `task_ids` is a rendering convenience
/// (`mission status`, the graph lens), and every Task record on disk
/// already carries its own `phase_id`, so a failed write costs a nicety,
/// not correctness. #2300 added this so the field meant the same thing on
/// the generic path as on the crawl launcher, which had always written it;
/// #2301 retired that launcher, so this is now the only writer.
fn write_phase_task_ids(mission_id: &str, phase_id: &str, ids: Vec<String>) {
    let result = load_phase_for_brief(mission_id, phase_id).and_then(|mut phase| {
        phase.task_ids = ids;
        crew::lifecycle::save_phase(&phase)
    });
    if let Err(e) = result {
        eprintln!(
            "{}",
            style::dim(&format!("mission launch: phase `{phase_id}` task_ids warning: {e:#}"))
        );
    }
}

/// (#2300) Append grown task ids to a phase record's existing `task_ids`.
/// Growth happens after the mint wrote the declared ids, so this is an
/// append, and it dedups so a re-entered phase can't double-list a task.
fn append_phase_task_ids(mission_id: &str, phase_id: &str, ids: &[String]) {
    if ids.is_empty() {
        return;
    }
    let existing = load_phase_for_brief(mission_id, phase_id).map(|p| p.task_ids).unwrap_or_default();
    let mut merged = existing;
    for id in ids {
        if !merged.iter().any(|e| e == id) {
            merged.push(id.clone());
        }
    }
    write_phase_task_ids(mission_id, phase_id, merged);
}

/// (#2300) Explicitly open and close a phase whose `grow` template grew
/// zero tasks, so its record reads `complete` rather than being swept to
/// `abandoned` by the #1504 finalize backstop.
///
/// A phase with no steps is invisible to `lazy_start_phase_for_step` and
/// `lazy_close_prior_phases` — both are driven by a step transition — so
/// without this the phase sits `Planned` all run and finalize reconciles it
/// to `Abandoned` with a warning. Nothing failed, though: the plan planned
/// nothing, which is a real and legitimate outcome (the `grew_nothing`
/// reason on the `mission.grow` record says exactly that). Closing the
/// prior phases first keeps the strictly-linear phase model (#1341) intact:
/// reaching this phase is still the evidence that the ones before it are
/// over, exactly as a first step transition would have been.
fn complete_grown_nothing_phase(
    mission_id: &str,
    real_phase_id: &str,
    phase_order: &[String],
    started: &mut std::collections::HashSet<String>,
    closed: &mut std::collections::HashSet<String>,
) {
    if !started.insert(real_phase_id.to_string()) {
        return;
    }
    lazy_close_prior_phases(mission_id, phase_order, real_phase_id, closed);
    let result = crew::lifecycle::phase_start(real_phase_id)
        .and_then(|_| crew::lifecycle::phase_complete(real_phase_id));
    match result {
        Ok(_) => {
            closed.insert(real_phase_id.to_string());
        }
        Err(e) => {
            eprintln!(
                "{}",
                style::dim(&format!(
                    "mission launch: completing empty phase `{real_phase_id}` failed: {e:#} — \
                     continuing; the whole-mission terminal reconciles phase state."
                ))
            );
        }
    }
}

fn bool_param(collected: &BTreeMap<String, serde_json::Value>, key: &str) -> bool {
    match collected.get(key) {
        Some(serde_json::Value::Bool(b)) => *b,
        Some(serde_json::Value::String(s)) => {
            matches!(s.trim().to_ascii_lowercase().as_str(), "true" | "1" | "yes" | "on")
        }
        _ => false,
    }
}

/// (#1959) `--dry-run`'s report for any generic step-graph config
/// (everything except `crawl`/`review`, which route to their own
/// dedicated dry-run reports before this function's caller is even
/// reached). Prints the resolved inputs, then the phase → task → step
/// structure `config.phases` declares — the SAME document `interpret`
/// would build a real graph from, just read and rendered instead of
/// minted.
fn print_dry_run_graph(config: &MissionConfig, collected: &BTreeMap<String, serde_json::Value>) {
    println!("darkmux mission launch {} --dry-run — nothing minted", config.id);
    println!("resolved inputs:");
    if collected.is_empty() {
        println!("  (none)");
    } else {
        for (k, v) in collected {
            let rendered = match v {
                serde_json::Value::String(s) => s.clone(),
                other => other.to_string(),
            };
            println!("  {k} = {rendered}");
        }
    }
    println!("graph:");
    if config.phases.is_empty() {
        println!("  (no phases)");
        return;
    }
    for phase in &config.phases {
        let label = phase.display_name.as_deref().unwrap_or(&phase.id);
        println!("  phase {label} ({} task(s))", phase.tasks.len());
        for task in &phase.tasks {
            let task_label = task.display_name.as_deref().unwrap_or(&task.id);
            let deps = if task.depends_on.is_empty() {
                String::new()
            } else {
                format!(" depends_on={:?}", task.depends_on)
            };
            let reads = if task.reads.is_empty() { String::new() } else { format!(" reads={:?}", task.reads) };
            let role = task.role_id.as_deref().map(|r| format!(" role={r}")).unwrap_or_default();
            println!("    task {task_label}{role}{deps}{reads}");
            for step in &task.steps {
                println!("      step {} [{}]", step.id, step.kind);
            }
        }
    }
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

/// (#1503) Mint a fresh, UNIQUE run id for one `mission launch` call — never
/// derived from inputs. Two launches of the same config with the same
/// inputs are two DIFFERENT runs (AI work is non-deterministic); collapsing
/// them onto one id was the category error #1503 fixes (the old
/// `derive_mission_id`'s deterministic hash-of-inputs id, which made a
/// relaunch collide with — and reopen — its own prior run).
///
/// Shape mirrors the lab harness's own `run_id` convention
/// (`crates/darkmux-lab/src/lab/run.rs`: `<workload>-<profile>-<unix-secs>-
/// <index>`) for mission↔lab consistency: `<config-id>-<unix-secs>-<6-hex
/// token>`. The lab convention's own disambiguator is a batch-loop index
/// (`--runs N`); `mission launch` has no such loop, so the token here is a
/// blake3 digest over (nanosecond time, pid, an in-process atomic counter)
/// instead — robustly unique even for two launches within the same
/// wall-clock second (the lab scheme is itself only second-granular).
pub(crate) fn mint_run_id(config_id: &str) -> Result<String> {
    static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let pid = std::process::id();
    let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let token_src = format!("{nanos}-{pid}-{n}");
    let digest = blake3::hash(token_src.as_bytes());
    let hex = digest.to_hex();
    let secs = nanos / 1_000_000_000;
    let id = format!("{config_id}-{secs}-{}", &hex.as_str()[..6]);
    fleet::validate_identifier("mission_id", &id)?;
    Ok(id)
}

/// (#1503) The spec fingerprint — what USED to be the mission id itself,
/// now demoted to `Mission.spec.inputs_fingerprint`, a grouping key rather
/// than identity. Blake3 over the canonical (BTreeMap-sorted) JSON of the
/// OPERATOR-SUPPLIED inputs (never including `mission_id`, which the
/// launcher supplies separately — hashing it in would be circular): the
/// SAME inputs against the SAME config always fingerprint identically (so
/// those runs group for corpus analysis), while different inputs
/// fingerprint differently.
pub(crate) fn spec_fingerprint(collected: &BTreeMap<String, serde_json::Value>) -> Result<String> {
    let canon =
        serde_json::to_string(collected).context("serializing collected inputs for the spec fingerprint")?;
    let digest = blake3::hash(canon.as_bytes());
    Ok(digest.to_hex().as_str()[..10].to_string())
}

fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// One Phase JSON literal, minted for every declared phase at launch.
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

/// Mint the Mission + one Phase per declared phase, with per-launcher
/// PROVENANCE overrides (#1284 Packet 4b review gate, must-fix 2). (#1503)
/// Every call is a FRESH mint — the reuse/reopen-by-derived-id path is
/// gone: since `mission_id` is now minted uniquely per launch (never
/// derived from inputs), a launch never revisits an existing mission's id,
/// so there is nothing to reopen. If the id somehow already exists on disk
/// (should be impossible given `mint_run_id`'s uniqueness guarantee), this
/// bails loud rather than silently reopening or overwriting a stranger's
/// record.
///
/// A dedicated launcher whose instances are per-case (the review launcher:
/// N CI reviews of N PRs) passes a case-bearing `description` ("PR review
/// — owner/repo@sha (crew `x`)") so the mission board / viewer can tell
/// the instances apart — falling back to the generic config-derived
/// description when `None` (the generic `launch` path). `spec` (#1503) is
/// the run's GROUPING metadata — which config, which resolved-inputs
/// fingerprint — recorded on the minted `Mission`; `None` for a caller
/// with no meaningful spec (a bare test fixture, e.g.).
///
/// Hydrates `Mission.source_input`/`Mission.ticket` from the config's
/// `extras` (#1284 review round 1, must-fix 2) — that's where `mission
/// propose` preserves the operator's verbatim words (#815) and ticket id
/// (#816), and dropping them silently broke `coder_brief`'s source-input
/// injection plus the conventions' `{ticket}` templates.
///
/// Returns the doc phase id → real (composed) phase id map every subsequent
/// step needs.
pub(crate) fn ensure_mission_and_phases_with_provenance(
    mission_id: &str,
    config: &MissionConfig,
    description: Option<&str>,
    spec: Option<MissionSpec>,
) -> Result<BTreeMap<String, String>> {
    ensure_mission_and_phases_with_provenance_and_start_payload(mission_id, config, description, spec, None)
}

/// (#2299) The same mint, with an optional payload for the `mission start`
/// record — the config launcher passes its prune report (`graph`) here so
/// the record says what the config declared and what was minted.
pub(crate) fn ensure_mission_and_phases_with_provenance_and_start_payload(
    mission_id: &str,
    config: &MissionConfig,
    description: Option<&str>,
    spec: Option<MissionSpec>,
    start_payload: Option<serde_json::Value>,
) -> Result<BTreeMap<String, String>> {
    let real_phase_ids: BTreeMap<String, String> = derive_phase_ids(mission_id, config);

    let mission_path = crew::lifecycle::mission_path(mission_id);
    if mission_path.exists() {
        bail!(
            "mission launch: run id `{mission_id}` already exists on disk — this should be \
             impossible (run ids are minted uniquely per launch, never derived from inputs, \
             see `mint_run_id`); if you're hitting this, it's either a genuine id collision or \
             a re-run against a copied/restored `.darkmux` directory. Rename or remove the \
             existing record and relaunch — implicit reuse/reopen was removed in #1503."
        );
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
        finalized_ts: None,
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
        spec,
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

    crew::lifecycle::mission_start_with_reasoning_and_payload(
        mission_id,
        Some(&format!("launched from config `{}`", config.id)),
        start_payload,
    )
    .context("starting the newly-minted mission")?;

    Ok(real_phase_ids)
}

/// Build the [`LaunchParams`] `mission_config::interpret` needs: every
/// phase's composed real id (generic, always safe), plus the
/// role/workdir/image/description overrides `collected` supplies for a
/// coder-phase-shaped graph. A graph that uses none of the coder-phase kinds
/// gets no task_overrides (pure pass-through of its own document defaults).
///
/// (#1549) Routed STRUCTURALLY — by which tasks declare the coder-phase step
/// kinds — not by `config.id == "coder-phase"` and not by the literal task
/// ids `build-coder`/`build-verify`. Both literals were live bugs, because
/// EXECUTION already routes structurally (`config_uses_coder_phase_kinds`):
/// a copied config under any other id ran correctly but skipped this block,
/// so its `Task.workdir` was never persisted — and `resolve_run_workdir`
/// (`src/coder_phase.rs`) then falls back to the DERIVED worktree path,
/// pointing `mission finalize`/`mission abort` at the wrong directory. That
/// function exists precisely "so finalize/abort target the ACTUAL worktree
/// even when the operator launched into a non-default location," which the
/// id gate silently broke for every config not literally named
/// `coder-phase`. Same treatment review's own id gate got in #1538.
/// (#2301) The LAST phase's last step output, when it is a JSON OBJECT —
/// the run's own summary, promoted to the `mission close` record's
/// payload.
///
/// Deliberately positional rather than keyed on a step KIND: the rule a
/// config author can rely on is "end your graph with a step that outputs
/// your run's summary", which composes with any kind. A last step that
/// outputs nothing, or outputs something that is not a JSON object (a
/// path, a branch name, a shell command's stdout), contributes no payload
/// — exactly the pre-#2301 behavior for every config that has one.
fn run_summary_payload(
    config: &MissionConfig,
    real_phase_ids: &BTreeMap<String, String>,
    tasks: &[crew::types::Task],
    steps: &BTreeMap<String, Step>,
) -> Option<serde_json::Value> {
    let last_phase = config.phases.last()?;
    let real_phase_id = real_phase_ids.get(&last_phase.id)?;
    let task = tasks.iter().rfind(|t| &t.phase_id == real_phase_id)?;
    let step = steps.get(task.step_ids.last()?)?;
    if step.status != crew::types::NodeStatus::Complete {
        return None;
    }
    // (#2301) A typed output rides in a `step_output::Output` envelope;
    // the payload readers want is the BODY, not the wrapper. An unwrapped
    // object passes through as-is (the pre-wrapper shape).
    match serde_json::from_str::<serde_json::Value>(step.output.as_deref()?.trim()) {
        Ok(serde_json::Value::Object(mut o)) => match (o.contains_key("kind"), o.remove("body")) {
            (true, Some(body @ serde_json::Value::Object(_))) => Some(body),
            (true, _) => None,
            (false, _) => Some(serde_json::Value::Object(o)),
        },
        _ => None,
    }
}

/// (#2301) The rule id a task is FOR — the `rule` key on any of its step
/// configs. A task with no such key (the crawl's own `summary`, and every
/// task in every other config) belongs to no rule and is never deselected.
fn task_declares_rule(task: &mission_config::TaskConfig) -> Option<&str> {
    task.steps.iter().find_map(|s| s.config.get("rule").and_then(|v| v.as_str()))
}

/// (#2301) The set `--param rules=<csv>` names, or `None` when the operator
/// named none (every enabled task runs). An explicitly EMPTY value selects
/// nothing — the operator's own honest no-op, the same reading `--param
/// limit=0` had — rather than silently meaning "all".
fn rule_selection(
    collected: &BTreeMap<String, serde_json::Value>,
) -> Option<std::collections::BTreeSet<String>> {
    let csv = collected.get("rules").and_then(|v| v.as_str())?;
    Some(csv.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect())
}

/// (#2301) The crawl's plan steps take their workspace + sizing knobs from
/// `--param`, not from the document (which carries a `{{workspace}}`
/// placeholder no interpreter substitutes). `step_config_overrides`
/// REPLACES a step's whole config, so each override is the document's own
/// config with the resolved values written over it — never a bare fragment
/// that would drop the step's `rule`.
fn crawl_plan_step_overrides(
    config: &MissionConfig,
    collected: &BTreeMap<String, serde_json::Value>,
) -> BTreeMap<String, serde_json::Value> {
    let mut out = BTreeMap::new();
    let Some(workspace) = collected.get("workspace").and_then(|v| v.as_str()) else {
        return out;
    };
    let mut sizing = serde_json::Map::new();
    for key in ["max_sites_per_unit", "max_est_tokens_per_unit"] {
        if let Some(n) = collected.get(key).and_then(param_as_u64) {
            sizing.insert(key.to_string(), serde_json::json!(n));
        }
    }
    let no_fetch = bool_param(collected, "no_fetch");
    for step in config
        .phases
        .iter()
        .flat_map(|p| p.tasks.iter())
        .flat_map(|t| t.steps.iter())
        .filter(|s| s.kind == darkmux_lab::crawl::plan_step::CRAWL_PLAN_KIND)
    {
        let mut cfg = step.config.clone();
        if !cfg.is_object() {
            cfg = serde_json::json!({});
        }
        let obj = cfg.as_object_mut().expect("just forced to an object");
        obj.insert("workspace".to_string(), serde_json::json!(workspace));
        if !sizing.is_empty() {
            obj.insert("sizing".to_string(), serde_json::Value::Object(sizing.clone()));
        }
        if no_fetch {
            obj.insert("no_fetch".to_string(), serde_json::json!(true));
        }
        out.insert(step.id.clone(), cfg);
    }
    out
}

/// A `--param` integer: the CLI collects params as strings, so a number
/// may arrive either typed or quoted.
fn param_as_u64(v: &serde_json::Value) -> Option<u64> {
    v.as_u64().or_else(|| v.as_str().and_then(|s| s.trim().parse::<u64>().ok()))
}

fn build_launch_params(
    config: &MissionConfig,
    real_phase_ids: &BTreeMap<String, String>,
    collected: &BTreeMap<String, serde_json::Value>,
) -> LaunchParams {
    let mut task_overrides = BTreeMap::new();
    if config_uses_coder_phase_kinds(config) {
        let role = collected.get("role").and_then(|v| v.as_str());
        let image = collected.get("image").and_then(|v| v.as_str());
        let workdir = collected.get("workdir").and_then(|v| v.as_str()).map(std::path::PathBuf::from);
        // The tasks are identified by what they DO (their step kinds), so a
        // renamed task in a copied config still receives its overrides.
        let coder_task = task_id_declaring_step_kind(config, "mission.coder");
        let verify_task = task_id_declaring_step_kind(config, "mission.verify");
        if let Some(coder_task) = coder_task {
            if let Some(role) = role {
                task_overrides.insert(
                    coder_task,
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
                    coder_task,
                    TaskOverride { workdir: workdir.clone(), image: image.map(String::from), ..Default::default() },
                );
            }
        }
        if let (Some(workdir), Some(verify_task)) = (workdir, verify_task) {
            task_overrides.insert(verify_task, TaskOverride { workdir: Some(workdir), ..Default::default() });
        }
    }

    LaunchParams {
        phase_ids: real_phase_ids.clone(),
        task_overrides,
        step_config_overrides: crawl_plan_step_overrides(config, collected),
    }
}

/// (#1549) The id of the first task whose graph declares a step of `kind`.
/// Lets [`build_launch_params`] attach its overrides to what a task DOES
/// rather than to what it — or its config — is NAMED, so a copied config
/// with renamed tasks still gets its `workdir`/`role`/`image` persisted onto
/// the right `Task` records.
fn task_id_declaring_step_kind(config: &MissionConfig, kind: &str) -> Option<String> {
    config
        .phases
        .iter()
        .flat_map(|p| p.tasks.iter())
        .find(|t| t.steps.iter().any(|s| s.kind == kind))
        .map(|t| t.id.clone())
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

/// (#1530) True when the config's graph uses any review-pipeline step kind —
/// the structural test that routes a review config to its dedicated launcher
/// (`crate::mission_launch_review::launch`) regardless of the config's `id`.
/// Mirrors [`config_uses_coder_phase_kinds`]: a config carrying any
/// `REVIEW_TIER3_KINDS` step is a review-pipeline config and must go through
/// the driver that owns review bundling/staffing/side-paths, whether it is
/// named `review`, `review-lean`, or anything else the operator stored.
///
/// `pub(crate)` (#1684 QA finding) — `src/acp_panel.rs`'s panel-command
/// router uses the SAME structural test to decide whether an invoked
/// command routes to `acp.rs`'s bespoke `run_review` path, rather than an
/// `id == "review"` string-literal check that would miss a renamed
/// variant exactly the way this function's own doc warns against.
pub(crate) fn config_uses_review_kinds(config: &MissionConfig) -> bool {
    config.phases.iter().any(|p| {
        p.tasks
            .iter()
            .any(|t| t.steps.iter().any(|s| REVIEW_TIER3_KINDS.contains(&s.kind.as_str())))
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
/// The canonical coder-phase id (#1436) — the SAME `mission-run-` prefix the
/// retired `mission run` stamped and that `mission finalize`/`mission abort`
/// reconstruct: the viewer's mission lens keys its per-run session grouping
/// on that prefix, and this path emits the identical record vocabulary, so
/// launched runs stay visible to the lens and legacy archives keep joining.
fn launch_session_id(mission_id: &str, real_phase_id: &str) -> String {
    darkmux_types::session_id::mission_run(mission_id, real_phase_id)
}

/// Handles `register_coder_phase_kinds` keeps back for the post-scheduler
/// gate decision (#1284 review round 1, must-fix 1a): the two result slots
/// the step kinds populate (the generic `StepOutcome.output: String`
/// contract can't carry the rich verdict/verifier detail), plus the
/// launch-resolved identifiers the gate banners print.
///
/// (#1530 Packets 2/3b-1) `coder_slot`/`verify_slot`/`context` are the SAME
/// `Arc`s `launch` seeds onto the run-scoped `ArtifactBus` (`coder_phase::
/// CODER_RESULT_ARTIFACT`/`CODER_VERIFY_RESULT_ARTIFACT`/
/// `CODER_CONTEXT_ARTIFACT`) before calling `run_step_graph` —
/// `MissionWorktreeStepKind`/`MissionCoderStepKind`/`MissionVerifyStepKind`
/// read/write them via `StepRunCtx::artifact` inside `run_streaming` (and,
/// for `context`, `residency()` too), and this struct's fields are simply
/// the caller's own clone of that hand-off, read directly (no bus access
/// needed post-run — `run_step_graph` exposes none; see `coder_phase.rs`'s
/// artifact-name doc for the full reasoning). `coder_slot`/`verify_slot`'s
/// TYPES/NAMES are unchanged from the pre-#1530-Packet-2 bespoke-slot shape;
/// `context` is new in Packet 3b-1 — the three kinds no longer hold
/// `workdir`/`branch`/`real_phase_id`/`role` etc. as constructor fields, so
/// nothing else keeps that data alive for `launch`'s `seed_artifacts` call
/// except this handle.
pub(crate) struct CoderPhaseHandles {
    coder_slot: Arc<Mutex<Option<coder_phase::CoderStepResult>>>,
    verify_slot: Arc<Mutex<Option<std::result::Result<crate::phase_cli::PhaseReviewOutput, String>>>>,
    context: Arc<coder_phase::CoderPhaseContext>,
    workdir: std::path::PathBuf,
    branch: String,
    real_phase_id: String,
    session_id: String,
}

/// Register `coder_phase.rs`'s three `coder-phase` Tier 3 kinds
/// (`mission.worktree`/`mission.coder`/`mission.verify`) onto `registry` —
/// the KIND-REGISTRATION half of what was, before #1530 ("one global
/// step-kind registry"), inlined directly into [`register_coder_phase_kinds`]
/// below. Extracted so [`all_step_kinds`] can register these three kinds
/// once, unconditionally, alongside Tier 1 builtins and `darkmux-lab`'s
/// review kinds — the MINTING half (resolving `workdir`/`branch`/`base`/
/// `role`/`image`, stamping `Step.config`, building [`CoderPhaseHandles`])
/// stays in `register_coder_phase_kinds`, which now ASSUMES these three ids
/// are already present on `registry` rather than registering them itself
/// (see that function's own doc).
///
/// Every kind here is a stateless unit struct (#1536/#1537/#1553), so this
/// is a one-time registration of shared `Arc` instances, not per-call
/// construction.
fn register_coder_phase_step_kinds(registry: &crew::step_kinds::StepKindRegistry) -> Result<()> {
    registry
        .register(Arc::new(coder_phase::MissionWorktreeStepKind))
        .map_err(|e| anyhow!("registering mission.worktree: {e}"))?;
    registry
        .register(Arc::new(coder_phase::MissionCoderStepKind))
        .map_err(|e| anyhow!("registering mission.coder: {e}"))?;
    registry
        .register(Arc::new(coder_phase::MissionVerifyStepKind))
        .map_err(|e| anyhow!("registering mission.verify: {e}"))?;
    Ok(())
}

/// Mint a [`CoderPhaseHandles`] for a `coder-phase`-shaped graph, using the
/// operator-collected `workdir`/`branch`/`base`/`role`/`image` inputs plus a
/// launcher-resolved `repo_root`. Bails loud (naming the missing input)
/// rather than proceeding with an empty path if the graph needs these kinds
/// but the operator didn't supply them. Returns the [`CoderPhaseHandles`]
/// the caller reads after `run_step_graph` to decide the gate outcome — and
/// also, since #1530 Packet 2, seeds onto the `ArtifactBus` via
/// `run_step_graph`'s `seed_artifacts` parameter (see `launch`'s own call
/// site).
///
/// **Precondition (#1530 — one global step-kind registry):** `registry`
/// must already carry `mission.worktree`/`mission.coder`/`mission.verify` —
/// this function no longer registers them itself (see
/// [`register_coder_phase_step_kinds`], which [`all_step_kinds`] calls
/// unconditionally when building the registry `launch` passes in here).
/// Before this packet, this function registered the three kinds directly
/// against a registry `launch` built fresh (`StepKindRegistry::
/// with_builtins()`) just for this call; now `launch` passes in the SAME
/// registry it already validated `config`'s step kinds against, which
/// `all_step_kinds` populated with these ids unconditionally — registering
/// them again here would hit `StepKindRegistry::register`'s duplicate-id
/// guard.
///
/// (#1530 Packet 3b-1) The run's identity (repo_root/wt_path/branch/base/
/// mission_id/phase_id/session_id/role) is stamped onto `CoderPhaseContext`
/// (never onto per-kind constructor fields — the kinds stay stateless
/// singletons), and `steps` (the SAME interpreted graph `launch` is about to
/// run) is mutated in place to stamp the coder step's own
/// `timeout_seconds`/`image`/`injected_budget_chars` onto its `Step.config`
/// — the same "compute once, stamp, read back in `run_streaming`" pattern
/// `darkmux-lab`'s `build_review_graph_from_config` uses for the review
/// judge seat's staffing (#1530 Packet 3a).
///
/// (#1546) `message` is no longer among the stamped fields — composing the
/// brief text itself (the mission/phase disk load, the corrections/
/// cautions/lessons walk, the rank + budget, the provenance printing) is no
/// longer this function's job at all. It moved into `MissionCoderStepKind::
/// run_streaming`, which now calls `coder_phase::
/// coder_brief_with_injected_context` itself, reading `injected_budget_chars`
/// back off `Step.config` and `mission_id`/`phase_id`/`wt_path` off the SAME
/// `CoderPhaseContext` this function already stamps — mirroring #1545's
/// bundling-becomes-runtime migration for the review pipeline: no
/// domain data (a composed, model-facing brief) is produced before graph
/// execution, only the pointer + a build-time-knowable budget number are.
fn register_coder_phase_kinds(
    registry: &crew::step_kinds::StepKindRegistry,
    mission_id: &str,
    config: &MissionConfig,
    real_phase_ids: &BTreeMap<String, String>,
    collected: &BTreeMap<String, serde_json::Value>,
    timeout_seconds: u32,
    steps: &mut BTreeMap<String, crew::types::Step>,
) -> Result<CoderPhaseHandles> {
    // (#1530 — one global step-kind registry) Loud precondition check —
    // see this function's own doc. `all_step_kinds` always registers these
    // three ids, so this only ever fires if a future caller passes in a
    // registry built some OTHER way; failing here, by name, beats a later
    // "unknown step kind" surfacing from deep inside `run_step_graph`.
    for kind in CODER_PHASE_TIER3_KINDS {
        registry.get(kind).with_context(|| {
            format!(
                "internal error: register_coder_phase_kinds called against a registry missing \
                 `{kind}` — the caller must build it via `all_step_kinds` (or otherwise call \
                 `register_coder_phase_step_kinds` first)"
            )
        })?;
    }

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

    // (#1546) Mission/phase records are no longer loaded here — that disk
    // read is composition's own run-time work now (see the
    // `injected_budget_chars` note below), done by `MissionCoderStepKind::
    // run_streaming` itself off the `mission_id`/`phase_id` `CoderPhaseContext`
    // already carries.
    let coder_slot: Arc<Mutex<Option<coder_phase::CoderStepResult>>> = Arc::new(Mutex::new(None));
    let verify_slot: Arc<
        Mutex<Option<std::result::Result<crate::phase_cli::PhaseReviewOutput, String>>>,
    > = Arc::new(Mutex::new(None));

    let repo_root = coder_phase::repo_root()?;

    // (#1546, mirroring #1545's bundling-becomes-runtime migration for the
    // review pipeline) The coder brief's INPUTS are the build-time-known
    // SPEC: mission id / phase id / worktree path already ride
    // `CoderPhaseContext` below, and the one coder-step-specific value is
    // this — the injected-context char budget (#1011). It's genuinely
    // knowable here: a pure function of the default profile's context
    // window (config, not graph output) and the operator's configured
    // fraction, with no dependency on the mission/phase records or the
    // worktree the composition itself needs. Stamped onto the coder step's
    // OWN `Step.config`, mirroring how `timeout_seconds`/`image` already
    // are. Composition ITSELF — loading the mission/phase records, walking
    // flow records for prior adjudication corrections (#849) and detector
    // cautions (#994), reading the engagement lessons store, ranking +
    // budgeting those sources (#1011), printing the operator-facing
    // provenance lines (#1426 ship-4), and assembling the final brief text
    // — moves into `MissionCoderStepKind::run_streaming`, the pipeline's
    // ONE call site for it now (never called from here), so there is no
    // double-print/double-disk-walk hazard to guard against by computing it
    // early: it happens exactly once, when the coder step actually runs.
    let injected_budget_chars = coder_phase::injected_budget_chars(
        // (#1282) `Err` = the default profile is quarantined; the coder
        // dispatch itself would hard-fail with the same error, so fail
        // loud here, at registration — the same point in the sequence the
        // retired eager call used to fail at.
        crew::dispatch_internal::resolve_context_window_internal(None, None)?,
    );
    let coder_step_id = format!("{real_phase_id}-coder-step");
    let coder_step = steps.get_mut(&coder_step_id).ok_or_else(|| {
        anyhow!(
            "mission launch: internal error — no `{coder_step_id}` step in the interpreted \
             `{}` graph",
            config.id
        )
    })?;
    coder_step.config = serde_json::json!({
        "timeout_seconds": timeout_seconds,
        "image": image,
        "injected_budget_chars": injected_budget_chars,
    });

    let context = Arc::new(coder_phase::CoderPhaseContext {
        repo_root,
        wt_path: workdir.clone(),
        branch: branch.clone(),
        base,
        mission_id: mission_id.to_string(),
        phase_id: real_phase_id.clone(),
        session_id: session_id.clone(),
        role,
    });

    Ok(CoderPhaseHandles {
        coder_slot,
        verify_slot,
        context,
        workdir,
        branch,
        real_phase_id,
        session_id,
    })
}

/// (#1433 follow-up) Whether a coder-phase gate outcome is a FAILURE exit
/// that never reached a reviewable gate — a worktree-creation bail (`Err`) or
/// a coder dispatch error (`Ok(1)`). ONLY these reconcile+finalize the
/// mission: leaving them `Running`/`Active` is the stranded drift the
/// scheduler-error path reconciles. The gate exits PROPER — `Ok(0)` clean,
/// `Ok(2)` QA found blockers, `Ok(3)` QA unavailable — must return `false`:
/// they deliberately hold the phase `Running` at the sign-off gate so
/// `mission finalize`/`mission abort` finish or tear down the loop (finalizing
/// `Ok(2)` would close the mission under QA-blockers before sign-off).
fn gate_outcome_reached_no_gate(outcome: &Result<i32>) -> bool {
    matches!(outcome, Err(_) | Ok(1))
}

/// (#1877 QA must-fix 3) The coder branch's terminal mission-level bookend
/// record — extracted out of `launch`'s `if let Some(handles) = &coder_
/// handles` arm so the `reached_gate` decision and the record it produces
/// are unit-testable directly against a scripted `outcome`, without driving
/// a real coder-phase dispatch through the scheduler (`launch` hardcodes
/// `crew::concurrent_dispatch::lms_host_factory` — there is no injectable
/// mock host on this path, so a full `launch()` integration test can only
/// ever reach an `Err` outcome here via a cheap, real failure like an
/// invalid worktree `base`, never `Ok(2)`/`Ok(3)`, which need a real
/// dispatch to produce).
///
/// Returns `(reached_gate, record)` — `launch` still owns EMITTING it
/// (`bookend.close`, the telemetry drain immediately before, and the
/// command-gate audit call after), this function owns only the DECISION: same
/// split responsibility `mission_bookend_record` itself already has
/// relative to its callers.
///
/// The complete-vs-error split is `reached_gate`
/// (`!gate_outcome_reached_no_gate(outcome)`), NOT `outcome == Ok(0)` —
/// see `gate_outcome_reached_no_gate`'s own doc for why: `Ok(2)` (QA found
/// blockers) and `Ok(3)` (QA unavailable) both leave the mission Active at
/// the sign-off gate, which is real dispatch work that started and
/// FINISHED, same as a clean `Ok(0)`. Reading `outcome == Ok(0)` here
/// instead would mismark `Ok(2)`/`Ok(3)` as `dispatch error`, flipping
/// those missions to Error on the Runs lens even though they are the
/// expected "QA has findings, come look" outcome — the exact regression
/// this function's own tests pin against.
fn coder_branch_terminal_bookend(
    outcome: &Result<i32>,
    config_id: &str,
    mission_id: &str,
) -> (bool, flow::FlowRecord) {
    let reached_gate = !gate_outcome_reached_no_gate(outcome);
    let record = mission_bookend_record(
        if reached_gate { flow::Level::Info } else { flow::Level::Error },
        if reached_gate { "dispatch complete" } else { "dispatch error" },
        config_id,
        mission_id,
        serde_json::json!({
            "runtime": "mission",
            "result_class": if reached_gate { "ok" } else { "error" },
            "gate": "coder-phase",
        }),
    );
    (reached_gate, record)
}

/// (#1530 Packet 2) Resolve WHICH step in `steps` is the graph's declared
/// sign-off gate (`StepKind::is_gate`) by asking `registry` for each step's
/// registered kind, rather than a hardcoded step-id naming convention. This
/// is the declaration-driven half of the gate mechanism — see
/// `StepKind::is_gate`'s own doc for the full reasoning, and
/// `coder_phase_gate_outcome`'s doc for the one production consumer today.
///
/// Falls back to `default_id` (the historical `"<phase>-verify-step"`
/// convention) when no step's kind resolves to `is_gate() == true` —
/// best-effort, fails open, the same shape `StepKind::residency`'s own doc
/// documents for a structurally analogous lookup. This covers a
/// hand-scripted test graph that never registers a real kind (this
/// module's own `scripted_gate_fixture` uses a placeholder `"mission.test"`
/// kind id) without forcing every test to stand up a full registry just to
/// exercise the gate-outcome MAP, which is what those tests are actually
/// pinning.
fn resolve_gate_step_id(
    registry: &crew::step_kinds::StepKindRegistry,
    steps: &BTreeMap<String, crew::types::Step>,
    default_id: String,
) -> String {
    steps
        .values()
        .find(|s| registry.get(&s.kind).map(|k| k.is_gate()).unwrap_or(false))
        .map(|s| s.id.clone())
        .unwrap_or(default_id)
}

/// The post-scheduler gate decision for a coder-phase graph — a faithful
/// mirror of `coder_phase::run`'s own post-graph sequence (#1284 review
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
/// and `mission finalize <mission-id>` (or `mission abort <mission-id>`)
/// is the operator's next move, after the frontier ships the git work by hand.
///
/// (#1530 Packet 2) The VERIFY step — the row this table calls "the gate" —
/// is located via [`resolve_gate_step_id`] against `registry`'s
/// `StepKind::is_gate()` declaration (`MissionVerifyStepKind` is the one
/// kind that returns `true`), not a hardcoded `"<phase>-verify-step"` id.
/// The worktree/coder step ids stay convention-derived — they are
/// PRE-gate stages whose errors bypass the gate entirely, not the gate
/// itself. Packet 3's generic runner is expected to apply this SAME
/// declaration-driven lookup to hold ANY graph's declared gate step,
/// rather than reimplementing coder-phase's own hardcoded logic.
fn coder_phase_gate_outcome(
    mission_id: &str,
    handles: &CoderPhaseHandles,
    steps: &BTreeMap<String, crew::types::Step>,
    registry: &crew::step_kinds::StepKindRegistry,
) -> Result<i32> {
    let worktree_step_id = format!("{}-worktree-step", handles.real_phase_id);
    let coder_step_id = format!("{}-coder-step", handles.real_phase_id);
    let verify_step_id =
        resolve_gate_step_id(registry, steps, format!("{}-verify-step", handles.real_phase_id));

    // (#1530) These three ids are a NAMING CONTRACT, and until this check
    // every read below was a raw `steps[&id]` — a bare `BTreeMap` panic with
    // no step id, no config, and no fix. Two plausible compositions reached
    // it, both newly expressible now that launching routes on step KINDS
    // rather than a config id: a coder-phase graph that declares no
    // `mission.verify` step at all (`resolve_gate_step_id` is documented as
    // failing OPEN, so it hands back the fallback id that isn't in the map),
    // and a worktree step named anything else.
    //
    // Generalizing the launcher is what made an operator's own graph able to
    // be wrong here, so the error has to say what's wrong and how to fix it.
    for (label, id) in [
        ("worktree", &worktree_step_id),
        ("coder", &coder_step_id),
        ("verify (the sign-off gate)", &verify_step_id),
    ] {
        if !steps.contains_key(id) {
            let declared: Vec<&str> = steps.keys().map(String::as_str).collect();
            bail!(
                "mission launch (`{mission_id}`): this coder-phase graph is missing its {label} \
                 step — the launcher reads it at the fixed step id `{id}`, and the interpreted \
                 graph has no such step (declared: {}). A coder-phase graph needs three steps \
                 named `<phase-id>-worktree-step`, `<phase-id>-coder-step` and \
                 `<phase-id>-verify-step`, carrying the kinds `mission.worktree`, `mission.coder` \
                 and `mission.verify`. Copy templates/builtin/mission-configs/coder-phase.json as \
                 a starting point, or rename the step to match.",
                declared.join(", ")
            );
        }
    }

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
        coder_phase::emit_step_result(
            flow::Level::Warn,
            "mission.verify",
            &verify_step_id,
            mission_id,
            phase_id,
            session_id,
            serde_json::json!({ "error": err_text, "total_tokens": tokens_total }),
        );
        println!("\n{}", style::header("▶ gate — QA unavailable, manual review required"));
        coder_phase::print_unverified_banner(&failed_verifiers);
        println!("  {} {}", style::dim("worktree:"), handles.workdir.display());
        println!("  {} {}", style::dim("branch:  "), style::accent(&handles.branch));
        println!(
            "\n{}",
            style::warn(&format!(
                "review the diff manually, ship the git work by hand (commit/push/PR), then \
                 finalize:  darkmux mission finalize {mission_id}   (or tear it down: \
                 darkmux mission abort {mission_id})"
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
        // Unreachable in practice — see `coder_phase::run`'s identical arm.
        _ => bail!("internal error: mission.verify step completed without a review result"),
    };

    // Stop at the gate. The frontier ships the git work by hand, then
    // `mission finalize` closes out; never commit/PR/merge here (#1463).
    println!("\n{}", style::header("▶ gate — awaiting frontier/operator sign-off"));
    println!("  {} {}", style::dim("worktree:"), handles.workdir.display());
    println!("  {} {}", style::dim("branch:  "), style::accent(&handles.branch));
    coder_phase::print_unverified_banner(&failed_verifiers);

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
            style::dim(&format!(
                "re-run QA after fixing:  darkmux mission launch review --param worktree={} \
                 --param diff_file=<diff>",
                handles.workdir.display()
            ))
        );
        println!(
            "  {}",
            style::dim(&format!("or tear this mission down: darkmux mission abort {mission_id}"))
        );
        coder_phase::emit_step_result(
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

    // An UNREADABLE review is not a clean review. `parse_signoff` yields
    // `indeterminate` when the dispatch text carried NO recognizable
    // severity markers AND no explicit clean marker — an empty/truncated
    // local-model response, or a marker style the parser doesn't know. That
    // outcome has been observed in production once already (#66's dogfood,
    // documented at its parse site), and before this arm it fell straight
    // through to the same "✓ ready for sign-off" + exit 0 as a genuinely
    // clean review, because the gate checked only `block > 0`. A broken QA
    // response must never read as a green check (#1113's contract, applied
    // to the coder gate) — so it takes the SAME exit-3 posture as a failed
    // QA dispatch: gate holds, manual review required.
    if review.verdict == "indeterminate" {
        println!(
            "\n{}",
            style::warn(
                "⚠ QA response was unreadable — no severity markers and no clean marker. \
                 The review may not have engaged with the format (empty/truncated reply, \
                 or an unrecognized marker style). This is NOT a pass."
            )
        );
        println!("  {}", style::dim("inspect the review output above, then either:"));
        println!(
            "  {} darkmux mission launch review --param worktree={} --param diff_file=<diff>",
            style::dim("→ re-run QA: "),
            handles.workdir.display()
        );
        println!(
            "  {} darkmux mission finalize {mission_id}",
            style::dim("→ or adjudicate manually and finalize: ")
        );
        coder_phase::emit_step_result(
            flow::Level::Warn,
            "mission.verify",
            &verify_step_id,
            mission_id,
            phase_id,
            session_id,
            serde_json::json!({
                "verdict": review.verdict,
                "blockers": 0,
                "flags": review.by_severity.flag,
                "total_tokens": tokens_total,
            }),
        );
        return Ok(3);
    }

    println!(
        "\n{}",
        style::success(&format!(
            "✓ ready for sign-off. Ship the git work by hand (commit/push/PR), then:  \
             darkmux mission finalize {mission_id}"
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
    coder_phase::emit_step_result(
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

// (#1546) No longer called by production code — brief composition (the
// mission/phase disk load it used to feed) moved into `coder_phase.rs`'s own
// `load_mission_record`, called from `MissionCoderStepKind::run_streaming`.
// Kept `#[cfg(test)]`-only: this module's own tests still use it as a
// read-back helper (`mission_status_on_disk`, the source_input/ticket
// hydration test) unrelated to brief composition.
#[cfg(test)]
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
/// loud dim warning, never a hard error — the same "continue, the whole-mission
/// terminal (`mission finalize` / `mission abort`) reconciles phase state"
/// posture the pre-#1400 eager loop used (#1463).
/// Returns `true` when this call is the one that brought `phase_id` live —
/// the caller's cue that the bands BEFORE it are over (#1620, see
/// [`lazy_close_prior_phases`]). `false` on every later step of an
/// already-started phase, so the advance fires exactly once per band.
pub(crate) fn lazy_start_phase_for_step(
    mission_id: &str,
    phase_id: &str,
    step_status: crew::types::NodeStatus,
    started: &mut std::collections::HashSet<String>,
) -> bool {
    use crew::types::NodeStatus;
    if step_status != NodeStatus::Running {
        return false;
    }
    if phase_id.is_empty() || !started.insert(phase_id.to_string()) {
        return false;
    }
    let status = load_phase_for_brief(mission_id, phase_id)
        .map(|p| p.status)
        .unwrap_or(PhaseStatus::Planned);
    if matches!(status, PhaseStatus::Planned | PhaseStatus::Abandoned) {
        if let Err(e) = crew::lifecycle::phase_start(phase_id) {
            eprintln!(
                "{}",
                style::dim(&format!(
                    "mission launch: phase_start({phase_id}) failed: {e:#} — continuing; the \
                     whole-mission terminal (`mission finalize` / `mission abort`) reconciles \
                     phase state."
                ))
            );
        }
    }
    true
}

/// (#1620) The counterpart to [`lazy_start_phase_for_step`]: close the phases
/// a newly-started one has moved past.
///
/// Without this, a launcher that starts phases lazily never CLOSES them, so
/// every phase it touched sat `Running` until the whole mission finalized and
/// reconciled them in bulk. The board counts `Complete` phases, so a review
/// read `0/3` for its entire run and then jumped to `3/3` — no intermediate
/// state at all, on the one column an operator consults to see how far along a
/// run is. `0/3` on a mission whose judge is working is not stale, it is
/// indistinguishable from a mission that never started. Two phases also read
/// `Running` at once, contradicting the strictly-linear phase model (#1341).
///
/// The advance itself is the evidence: phases are strictly sequential, so
/// reaching phase N means N-1's work is over. What that phase EARNED is not
/// assumed — it is derived from its own steps by the same
/// [`phase_finalization`] rules `finalize` would apply later, so closing early
/// yields the identical verdict, just sooner.
///
/// Deliberately conservative in one place: a prior phase with any step still
/// non-terminal is LEFT ALONE rather than abandoned. Concurrency could in
/// principle overlap bands, and an early wrong `Abandoned` is far worse than a
/// late-but-correct one — finalize still reconciles whatever this skips.
pub(crate) fn lazy_close_prior_phases(
    mission_id: &str,
    phase_order: &[String],
    newly_started: &str,
    closed: &mut std::collections::HashSet<String>,
) {
    use crew::types::NodeStatus;
    let Some(at) = phase_order.iter().position(|p| p == newly_started) else {
        return;
    };
    for prior in &phase_order[..at] {
        if !closed.insert(prior.clone()) {
            continue;
        }
        let status = load_phase_for_brief(mission_id, prior).map(|p| p.status);
        if !matches!(status, Ok(PhaseStatus::Running)) {
            continue;
        }
        let Ok(steps) = crew::lifecycle::load_steps_for_phase(mission_id, prior) else {
            closed.remove(prior);
            continue;
        };
        // (#1620) NO steps on disk is absence of evidence, not evidence of
        // failure — and `phase_finalization` reads an empty slice as "the
        // scheduler never reached this phase", i.e. Abandoned. Deriving a
        // verdict from nothing is the same defect as rendering an unknown run
        // as `running` (#1621); it just fails in the pessimistic direction.
        // Caught by `advancing_a_phase_closes_the_ones_before_it`, which
        // abandoned a healthy phase on the first advance.
        //
        // Likewise a phase with any step still live is not ours to close.
        // Both cases defer to finalize, which reconciles with full information.
        if steps.is_empty()
            || steps.iter().any(|s| matches!(s.status, NodeStatus::Planned | NodeStatus::Running))
        {
            closed.remove(prior);
            continue;
        }
        let refs: Vec<&crew::types::Step> = steps.iter().collect();
        let (outcome, _reason) = phase_finalization(&refs);
        let result = match outcome {
            crew::envelope::PhaseOutcomeKind::Complete => crew::lifecycle::phase_complete(prior),
            _ => crew::lifecycle::phase_abandon(prior),
        };
        if let Err(e) = result {
            closed.remove(prior);
            eprintln!(
                "{}",
                style::dim(&format!(
                    "mission launch: closing phase `{prior}` on advance failed: {e:#} — \
                     continuing; the whole-mission terminal reconciles phase state."
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
/// refuses, leaving a Finalized mission with a permanently `Planned` phase whose
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
///
/// Reached ONLY by a gate-less generic Tier-1-only graph — a coder-phase
/// config's `coder_handles` branch (see `launch`, above) always `return`s
/// before this call. See the `MissionOutcomeStatus::from_outcome` /
/// `RunOutcome` deferral note attached to `launch`'s exit-code match, right
/// after this call site, for why this function still constructs `status`
/// directly and the real (not "different shape") reason it hasn't adopted
/// `RunOutcome` yet.
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
///
/// (#1877 item 4) THIS is the site that actually owns a coder-phase
/// `launch`'s Error-status envelope on a pre-gate failure — hand-
/// constructs `MissionOutcomeStatus::Error` directly, same as
/// [`build_envelope`] and `coder_phase.rs`'s `finalize_mission_if_complete`.
/// Same deferred blocker as both: `MissionOutcomeStatus::from_outcome` has
/// no route to `Error`, so adopting `RunOutcome` here is a status change,
/// not a pure refactor — see `build_envelope`'s doc.
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

    // ── (#1685) command allowlist gate on the DIRECT CLI launch path ────
    // `acp_panel::run_ephemeral`'s own tests cover the ACP route; these
    // cover the SAME `mission_config::check_cmd` gate on a bare
    // `darkmux mission launch <id>` invocation, proving the allowlist holds
    // on both entry points named in the #1685 spec.

    const CMD_GATE_CONFIG: &str = r#"{
        "id": "cmd-gate-test-mission",
        "name": "GH Verb Test Mission",
        "schema_version": "2.3",
        "cmd": "pr-merge",
        "phases": [
            {"id": "p1", "tasks": [{"id": "t1", "steps": [{"id": "s1", "kind": "procedural.noop"}]}]}
        ]
    }"#;

    #[test]
    #[serial_test::serial]
    fn launch_refuses_a_cmd_config_when_the_allowlist_gate_is_off() {
        let guard = LaunchTestGuard::new();
        guard.write_config("cmd-gate-test-mission", CMD_GATE_CONFIG);
        let prev_enabled = env::var("DARKMUX_CMD_ENABLED").ok();
        let prev_allowed = env::var("DARKMUX_CMD_ALLOWED").ok();
        unsafe {
            env::remove_var("DARKMUX_CMD_ENABLED");
            env::remove_var("DARKMUX_CMD_ALLOWED");
        }

        let err = launch("cmd-gate-test-mission", None, &[], None)
            .expect_err("a cmd config must be refused with the allowlist gate off");
        assert!(err.to_string().contains("pr-merge"), "{err}");
        assert!(all_mission_ids().is_empty(), "a refused cmd-gate config must mint NOTHING, not a half-launched instance");

        unsafe {
            match prev_enabled {
                Some(v) => env::set_var("DARKMUX_CMD_ENABLED", v),
                None => env::remove_var("DARKMUX_CMD_ENABLED"),
            }
            match prev_allowed {
                Some(v) => env::set_var("DARKMUX_CMD_ALLOWED", v),
                None => env::remove_var("DARKMUX_CMD_ALLOWED"),
            }
        }
        drop(guard);
    }

    const PANEL_ARGS_CONFIG: &str = r#"{
        "id": "panel-args-test-mission",
        "name": "Panel Args Test Mission",
        "schema_version": "2.3",
        "inputs": [{"name": "args", "required": false}],
        "phases": [
            {"id": "p1", "tasks": [{
                "id": "echo-args",
                "reads": ["__panel_args__"],
                "steps": [{
                    "id": "echo-args-step",
                    "kind": "procedural.shell",
                    "config": {"command": "echo got: $DARKMUX_STEP_INPUT___PANEL_ARGS__"}
                }]
            }]}
        ]
    }"#;

    /// (#1685) Direct `darkmux mission launch <id> --param args=<value>`
    /// must deliver the value into a config's `reads: ["__panel_args__"]`
    /// task exactly like the ACP ephemeral route does (`acp_panel::
    /// run_ephemeral`'s own `ephemeral_run_seeds_args_when_a_task_reads_
    /// the_synthetic_args_task`). Before this fix, ANY config declaring
    /// that read hard-failed `interpret` on this CLI path — this is the
    /// regression test for the fix, not just new-feature coverage.
    #[test]
    #[serial_test::serial]
    fn launch_delivers_param_args_into_a_task_reading_the_reserved_panel_args_id() {
        let guard = LaunchTestGuard::new();
        guard.write_config("panel-args-test-mission", PANEL_ARGS_CONFIG);

        let exit = launch("panel-args-test-mission", None, &["args=hello-world".to_string()], None)
            .expect("a config reading __panel_args__ must launch and run cleanly from the CLI too");
        assert_eq!(exit, 0);

        let mission_id = single_mission_id();
        let steps_dir = crew::loader::missions_dir().join(&mission_id).join("steps");
        let step_path = walk_for_file(&steps_dir, "echo-args-step.json").expect("step json must exist");
        let step: crew::types::Step =
            serde_json::from_str(&std::fs::read_to_string(&step_path).unwrap()).unwrap();
        assert_eq!(step.output.as_deref().map(str::trim), Some("got: hello-world"));
    }

    /// The no-args case must still resolve cleanly (an empty string, not a
    /// dangling-reference failure) — mirrors `ephemeral_run_with_empty_
    /// args_still_resolves_a_task_that_reads_the_reserved_id` on the ACP side.
    #[test]
    #[serial_test::serial]
    fn launch_with_no_args_param_still_resolves_a_task_reading_the_reserved_id() {
        let guard = LaunchTestGuard::new();
        guard.write_config("panel-args-test-mission", PANEL_ARGS_CONFIG);

        let exit = launch("panel-args-test-mission", None, &[], None)
            .expect("no --param args= at all must still resolve to an empty string, not fail");
        assert_eq!(exit, 0);
        drop(guard);
    }

    /// Find a file named `name` anywhere under `dir` (the step json lives
    /// one phase-subdirectory deep — `steps/<phase-task-id>/<step-id>.json`
    /// — and this test doesn't want to hardcode that shape).
    fn walk_for_file(dir: &std::path::Path, name: &str) -> Option<std::path::PathBuf> {
        for entry in std::fs::read_dir(dir).ok()?.filter_map(|e| e.ok()) {
            let path = entry.path();
            if path.is_dir() {
                if let Some(found) = walk_for_file(&path, name) {
                    return Some(found);
                }
            } else if path.file_name().and_then(|n| n.to_str()) == Some(name) {
                return Some(path);
            }
        }
        None
    }

    #[test]
    #[serial_test::serial]
    fn launch_runs_a_cmd_config_once_allowlisted() {
        let guard = LaunchTestGuard::new();
        guard.write_config("cmd-gate-test-mission", CMD_GATE_CONFIG);
        let prev_enabled = env::var("DARKMUX_CMD_ENABLED").ok();
        let prev_allowed = env::var("DARKMUX_CMD_ALLOWED").ok();
        unsafe {
            env::set_var("DARKMUX_CMD_ENABLED", "true");
            env::set_var("DARKMUX_CMD_ALLOWED", "pr-merge");
        }

        let exit = launch("cmd-gate-test-mission", None, &[], None)
            .expect("an allowlisted cmd config must launch normally");
        assert_eq!(exit, 0);
        assert_eq!(all_mission_ids().len(), 1, "the allowlisted config must actually mint + run");

        unsafe {
            match prev_enabled {
                Some(v) => env::set_var("DARKMUX_CMD_ENABLED", v),
                None => env::remove_var("DARKMUX_CMD_ENABLED"),
            }
            match prev_allowed {
                Some(v) => env::set_var("DARKMUX_CMD_ALLOWED", v),
                None => env::remove_var("DARKMUX_CMD_ALLOWED"),
            }
        }
        drop(guard);
    }

    const GH_VERB_GATED_CONFIG: &str = r#"{
        "id": "cmd-gate-gated-test-mission",
        "name": "GH Verb Gated Test Mission",
        "schema_version": "2.3",
        "cmd": "pr-merge",
        "inputs": [{"name": "args", "required": false}],
        "phases": [
            {"id": "p1", "tasks": [{"id": "t1", "steps": [{
                "id": "s1", "kind": "procedural.noop", "gate": "operator",
                "config": {"output": "merged"}
            }]}]}
        ]
    }"#;

    /// (#1685 QA MUST-FIX 2) `darkmux mission launch pr-merge` must leave
    /// the SAME `gh.verb.executed` audit record `acp_panel::run_ephemeral`
    /// emits for the identical config launched from the panel — before this
    /// fix, `check_cmd` and the args-injection wiring covered this
    /// entry point, but NOTHING emitted the audit record after: a bare
    /// `darkmux mission launch pr-merge` executed the merge, gated only by
    /// the tty prompt, with zero trace in `/flow`. The gated step here is
    /// DECLINED (cargo test's stdin/stdout are never real terminals, so
    /// `cli_gate_handler` resolves to `refusal_handler`, which fails
    /// closed) — proving `confirmed: false` and `success: false` land in
    /// the payload for a real, non-approved attempt, not just a happy path
    /// this launcher never actually exercises without Docker.
    #[test]
    #[serial_test::serial]
    fn launch_emits_one_audit_flow_record_for_an_executed_cmd_command() {
        let guard = LaunchTestGuard::new();
        guard.write_config("cmd-gate-gated-test-mission", GH_VERB_GATED_CONFIG);
        let prev_enabled = env::var("DARKMUX_CMD_ENABLED").ok();
        let prev_allowed = env::var("DARKMUX_CMD_ALLOWED").ok();
        unsafe {
            env::set_var("DARKMUX_CMD_ENABLED", "true");
            env::set_var("DARKMUX_CMD_ALLOWED", "pr-merge");
        }

        let exit = launch("cmd-gate-gated-test-mission", None, &["args=123".to_string()], None)
            .expect("an allowlisted cmd config must launch — a declined gate fails the STEP, not the scheduler");
        assert_eq!(exit, 1, "the only step declined (non-interactive refusal_handler) so the mission ends Error");

        let records = read_all_flow_records();
        let audit = records
            .iter()
            .find(|r| r["action"] == "gh.verb.executed")
            .expect("exactly one gh.verb.executed audit record must be emitted on the launch() path too");
        assert_eq!(audit["category"], "audit");
        let payload = &audit["payload"];
        assert_eq!(payload["verb"], "pr-merge");
        assert_eq!(payload["pr"], "123", "best-effort PR extraction from --param args");
        assert_eq!(
            payload["worktree"],
            std::env::current_dir().unwrap().to_string_lossy().to_string(),
            "a direct CLI launch audits the process's own cwd — it has no separate session cwd"
        );
        assert_eq!(payload["confirmed"], false, "non-interactive refusal_handler declines every gated step");
        assert_eq!(payload["success"], false);

        unsafe {
            match prev_enabled {
                Some(v) => env::set_var("DARKMUX_CMD_ENABLED", v),
                None => env::remove_var("DARKMUX_CMD_ENABLED"),
            }
            match prev_allowed {
                Some(v) => env::set_var("DARKMUX_CMD_ALLOWED", v),
                None => env::remove_var("DARKMUX_CMD_ALLOWED"),
            }
        }
        drop(guard);
    }

    /// The inverted case (red-prove): a config with NO `cmd` running
    /// through the SAME generic tail path (`emit_launch_cmd_audit` is
    /// called unconditionally there and must no-op) never emits this
    /// record — mirrors `acp_panel::run_ephemeral_emits_no_audit_record_
    /// for_a_non_cmd_config`.
    #[test]
    #[serial_test::serial]
    fn launch_emits_no_audit_record_for_a_non_cmd_config() {
        let guard = LaunchTestGuard::new();
        guard.write_config("panel-args-test-mission", PANEL_ARGS_CONFIG);

        let exit = launch("panel-args-test-mission", None, &[], None).expect("an ordinary launch must succeed");
        assert_eq!(exit, 0);

        assert!(
            read_all_flow_records().iter().all(|r| r["action"] != "gh.verb.executed"),
            "no cmd declared -> no audit record, ever, on the launch() path either"
        );
        drop(guard);
    }

    const MIXED_OUTCOME_CONFIG: &str = r#"{
        "id": "mixed-outcome-test-mission",
        "name": "Mixed Outcome Test Mission",
        "schema_version": "2.3",
        "phases": [{
            "id": "p1",
            "tasks": [
                {"id": "t-ok", "steps": [{"id": "ok-step", "kind": "procedural.shell", "config": {"command": "true"}}]},
                {"id": "t-fail", "steps": [{"id": "fail-step", "kind": "procedural.shell", "config": {"command": "exit 1"}}]}
            ]
        }]
    }"#;

    /// (#1893) The generic gate-less launch path's exit code for
    /// `MissionOutcomeStatus::Degraded` is unpinned: `Clean` and `Error`
    /// both have coverage (this file's cmd-gate tests drive `Error` to exit
    /// 1 already), but nothing drives `launch()` with a config that mixes a
    /// completed step and an errored one in the same run — the shape
    /// `build_envelope` maps to `Degraded`, and the shape every
    /// operator-authored `cmd` panel config (`pr-list`, `pr-info`,
    /// `pr-approve`, `pr-merge`) actually produces on a partial run. Two
    /// independent, ungated tasks in one phase — one succeeds, one fails —
    /// so the scheduler runs both to completion regardless of the other's
    /// outcome.
    ///
    /// Mutating the `Degraded` arm from `=> 0` to `=> 1` in `mission_launch`'s
    /// exit-code match must fail this test.
    #[test]
    #[serial_test::serial]
    fn launch_a_mixed_complete_and_errored_generic_run_exits_zero_for_degraded() {
        let guard = LaunchTestGuard::new();
        guard.write_config("mixed-outcome-test-mission", MIXED_OUTCOME_CONFIG);

        let exit = launch("mixed-outcome-test-mission", None, &[], None)
            .expect("a mixed complete/errored generic run must still return an exit code, not an Err");
        assert_eq!(
            exit, 0,
            "MissionOutcomeStatus::Degraded (some steps completed, some errored) must exit 0 — \
             a partially-constrained run is not a failed one"
        );

        let mission_id = single_mission_id();
        let envelope = crew::lifecycle::load_envelope(&mission_id)
            .expect("envelope must be readable")
            .expect("a finalized generic mission must have written an envelope");
        assert_eq!(
            envelope.status,
            crew::envelope::MissionOutcomeStatus::Degraded,
            "sanity: this config's shape (one completed step + one errored step) must actually \
             produce Degraded, or this test isn't exercising the arm it claims to"
        );
        drop(guard);
    }

    // ── Generic launch path — freeform (no dispatch, no Docker) ────────

    /// All mission ids currently minted under the test's isolated crew root
    /// (`missions_dir()`'s per-mission SUBDIRECTORIES — see
    /// `crew::lifecycle::mission_path`'s own doc), sorted for a stable
    /// comparison order. (#1503) Since a mission id is no longer derivable
    /// from its inputs, tests recover the minted id(s) by reading the
    /// isolated crew root back, rather than re-deriving what `launch`
    /// itself minted.
    fn all_mission_ids() -> Vec<String> {
        let dir = crew::loader::missions_dir();
        let mut ids: Vec<String> = std::fs::read_dir(&dir)
            .map(|rd| {
                rd.filter_map(|e| e.ok())
                    .filter(|e| e.path().is_dir())
                    .map(|e| e.file_name().to_string_lossy().to_string())
                    .collect()
            })
            .unwrap_or_default();
        ids.sort();
        ids
    }

    /// The single mission id minted so far — panics if zero or more than
    /// one exists, so a test asserting "exactly one launch happened" fails
    /// loud on a miscount rather than silently reading the wrong mission.
    fn single_mission_id() -> String {
        let ids = all_mission_ids();
        assert_eq!(ids.len(), 1, "expected exactly one minted mission, found {ids:?}");
        ids.into_iter().next().unwrap()
    }

    /// (#1641) Every flow record written to the isolated `DARKMUX_FLOWS_DIR`
    /// so far, across every per-day JSONL file `LaunchTestGuard` set up —
    /// read raw off disk rather than through any in-process buffer, so this
    /// exercises the SAME on-disk shape a real `mission-graph` page's
    /// backfill fetch would see.
    fn read_all_flow_records() -> Vec<serde_json::Value> {
        let dir = std::env::var("DARKMUX_FLOWS_DIR")
            .expect("DARKMUX_FLOWS_DIR must be set by an active LaunchTestGuard");
        let mut out = Vec::new();
        let rd = match std::fs::read_dir(&dir) {
            Ok(rd) => rd,
            Err(_) => return out,
        };
        for entry in rd.filter_map(|e| e.ok()) {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
                continue;
            }
            let contents = std::fs::read_to_string(&path).unwrap_or_default();
            for line in contents.lines() {
                if line.trim().is_empty() {
                    continue;
                }
                if let Ok(v) = serde_json::from_str::<serde_json::Value>(line) {
                    out.push(v);
                }
            }
        }
        out
    }

    #[test]
    #[serial_test::serial]
    fn freeform_launch_mints_instance_and_leaves_phases_manual() {
        let guard = LaunchTestGuard::new();
        guard.write_config("freeform-test-mission", FREEFORM_CONFIG);

        let exit = launch("freeform-test-mission", None, &[], None).expect("launch should succeed");
        assert_eq!(exit, 0);

        // (#1503) The run id is minted fresh, never derived from (the
        // empty) inputs — recover it from disk rather than re-deriving it.
        let mission_id = single_mission_id();
        assert!(
            mission_id.starts_with("freeform-test-mission-"),
            "a minted run id is still config-id-prefixed, got {mission_id}"
        );

        let mission_path = crew::lifecycle::mission_path(&mission_id);
        assert!(mission_path.is_file(), "mission.json must exist at {}", mission_path.display());
        let mission: Mission = serde_json::from_str(&std::fs::read_to_string(&mission_path).unwrap()).unwrap();
        assert_eq!(mission.status, MissionStatus::Active);
        assert!(mission.started_ts.is_some(), "launch drives mission_start_with_reasoning, not a bare Active write");
        assert_eq!(mission.phase_ids.len(), 2);
        assert_eq!(mission.phase_ids[0], format!("{mission_id}-p1"));
        assert_eq!(mission.phase_ids[1], format!("{mission_id}-p2"));

        // (#1503) `spec` records the grouping key: this config, zero inputs.
        let expected_fingerprint = spec_fingerprint(&BTreeMap::new()).unwrap();
        assert_eq!(
            mission.spec,
            Some(MissionSpec {
                config_id: "freeform-test-mission".to_string(),
                inputs_fingerprint: expected_fingerprint.clone(),
                // (#1562) This test launches a USER-tier config (it writes the
                // config into the user mission-configs dir), so the recorded
                // origin must be UserConfig — the named-work classification.
                origin: Some(darkmux_crew::types::MissionSpecOrigin::UserConfig),
            }),
            "spec must record the config id + inputs fingerprint + origin as grouping metadata"
        );

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

        // (#1503) The core behavior change: a relaunch with IDENTICAL
        // (empty) inputs mints a DISTINCT run — never reuses/reopens the
        // prior one — but the two runs still GROUP (equal `spec`).
        let exit2 = launch("freeform-test-mission", None, &[], None).expect("relaunch should succeed");
        assert_eq!(exit2, 0);
        let ids = all_mission_ids();
        assert_eq!(ids.len(), 2, "two launches must mint TWO missions, never collapse onto one id");
        let mission_id2 = ids.into_iter().find(|id| id != &mission_id).unwrap();
        assert_ne!(mission_id, mission_id2, "identical inputs must still mint a DIFFERENT run id per launch");

        let mission2: Mission =
            serde_json::from_str(&std::fs::read_to_string(crew::lifecycle::mission_path(&mission_id2)).unwrap())
                .unwrap();
        assert_eq!(
            mission2.spec, mission.spec,
            "same config + same inputs must fingerprint identically — the two runs group"
        );
    }

    /// (#1641) Two missions launched from the SAME config share every
    /// CONFIG-scoped identity a step-lifecycle record carries —
    /// `session_id::task(&step.task_id)` and `handle: step.id` are literal
    /// strings straight out of the document, byte-identical across both
    /// runs (`crates/darkmux-crew/src/scheduler.rs`'s `step_lifecycle_record`
    /// doc names this exact collision — real flow data measured only 0.2%
    /// of dispatch/token records carrying `mission_id` before this fix).
    /// This proves the launcher's `emit`-wrap closes it: same session_id and
    /// handle across both runs, but `mission_id` disambiguates them.
    #[test]
    #[serial_test::serial]
    fn two_launches_of_the_same_config_produce_step_lifecycle_records_distinguishable_by_mission_id() {
        const STEPPED_CONFIG: &str = r#"{
            "id": "twin-mission-test",
            "name": "Twin Mission Test",
            "schema_version": "1.1",
            "phases": [
                {
                    "id": "p1",
                    "description": "one real, hermetic step — no model, no dispatch",
                    "tasks": [
                        { "id": "t1", "steps": [{ "id": "s1", "kind": "procedural.noop" }] }
                    ]
                }
            ]
        }"#;
        let guard = LaunchTestGuard::new();
        guard.write_config("twin-mission-test", STEPPED_CONFIG);

        let exit1 = launch("twin-mission-test", None, &[], None).expect("first launch should succeed");
        assert_eq!(exit1, 0);
        let first_id = single_mission_id();

        let exit2 = launch("twin-mission-test", None, &[], None).expect("second launch should succeed");
        assert_eq!(exit2, 0);
        let ids = all_mission_ids();
        assert_eq!(ids.len(), 2, "two launches of the same config must mint two missions");
        let second_id = ids.into_iter().find(|id| id != &first_id).unwrap();
        assert_ne!(first_id, second_id, "two launches must never collapse onto one mission id");

        // Every "step start"/"step complete" record the scheduler emitted
        // across BOTH runs, straight off the isolated flow sink.
        let flow_records = read_all_flow_records();
        let step_records: Vec<&serde_json::Value> = flow_records
            .iter()
            .filter(|r| {
                let action = r.get("action").and_then(|v| v.as_str()).unwrap_or("");
                let source = r.get("source").and_then(|v| v.as_str()).unwrap_or("");
                source == "scheduler" && (action == "step start" || action == "step complete")
            })
            .collect();
        assert!(
            step_records.len() >= 4,
            "expected `step start`+`step complete` for `s1` from BOTH runs (>=4 records total), \
             got {}: {step_records:#?}",
            step_records.len()
        );

        // The config-scoped collision that made the bug possible: BOTH runs'
        // step-lifecycle records share the exact same `session_id`/`handle`.
        let session_ids: std::collections::BTreeSet<&str> = step_records
            .iter()
            .filter_map(|r| r.get("session_id").and_then(|v| v.as_str()))
            .collect();
        assert_eq!(
            session_ids,
            std::collections::BTreeSet::from(["task-t1"]),
            "both runs' step-lifecycle records must share the SAME config-scoped session_id \
             (session_id::task(\"t1\") = \"task-t1\") — this is the collision surface, got {session_ids:?}"
        );
        let handles: std::collections::BTreeSet<&str> =
            step_records.iter().filter_map(|r| r.get("handle").and_then(|v| v.as_str())).collect();
        assert_eq!(
            handles,
            std::collections::BTreeSet::from(["s1"]),
            "both runs' step-lifecycle records must share the SAME handle, got {handles:?}"
        );

        // ...but `mission_id` DOES distinguish them now — the actual fix
        // under test. Assert on `mission_id` itself, not any downstream or
        // coincidental value.
        for r in &step_records {
            assert!(
                r.get("mission_id").and_then(|v| v.as_str()).is_some(),
                "every step-lifecycle record must carry a non-null mission_id, got {r:#?}"
            );
        }
        let mission_ids_seen: std::collections::BTreeSet<&str> = step_records
            .iter()
            .filter_map(|r| r.get("mission_id").and_then(|v| v.as_str()))
            .collect();
        assert_eq!(
            mission_ids_seen,
            std::collections::BTreeSet::from([first_id.as_str(), second_id.as_str()]),
            "step-lifecycle records must carry BOTH real, distinct mission ids — same \
             session_id/handle, but mission_id tells the two runs apart"
        );
    }

    /// (#1877, final wiring step) A generic Tier-1 mission launched the
    /// ordinary way (through `launch()` itself, not a hand-rolled
    /// scheduler call) also reaches the flow stream with a "step timing"
    /// record, and it carries this run's real `mission_id` the same way
    /// every other scheduler-authored record does (`get_or_insert`, see
    /// `launch`'s own `emit` closure). This is the sibling of the
    /// coder-phase acceptance test above, proving the wiring is generic
    /// (every mission through `run_step_graph`), not coder-phase-specific.
    ///
    /// **Proved failing first**: same mechanism as the coder-phase test.
    /// With `apply_step_terminal`'s `emit(step_timing_record(...))` call
    /// removed, `timing` here is empty and the length assertion fails.
    /// Observed directly while writing this test.
    #[test]
    #[serial_test::serial]
    fn a_generic_tier1_mission_also_gets_step_timing_records_through_launch() {
        const ONE_STEP_CONFIG: &str = r#"{
            "id": "step-timing-test-mission",
            "name": "Step Timing Test Mission",
            "schema_version": "1.1",
            "phases": [
                {
                    "id": "p1",
                    "tasks": [
                        { "id": "t1", "steps": [{ "id": "s1", "kind": "procedural.shell", "config": {"command": "true"} }] }
                    ]
                }
            ]
        }"#;
        let guard = LaunchTestGuard::new();
        guard.write_config("step-timing-test-mission", ONE_STEP_CONFIG);

        let exit = launch("step-timing-test-mission", None, &[], None).expect("launch should succeed");
        assert_eq!(exit, 0);
        let mission_id = single_mission_id();

        let records = read_all_flow_records();
        let timing: Vec<&serde_json::Value> = records
            .iter()
            .filter(|r| r.get("action").and_then(|v| v.as_str()) == Some("step timing"))
            .collect();
        assert_eq!(
            timing.len(),
            1,
            "expected exactly one \"step timing\" record for the one step that ran, got: {records:#?}"
        );
        assert_eq!(timing[0]["source"], serde_json::json!("scheduler"));
        assert_eq!(timing[0]["mission_id"], serde_json::json!(mission_id));
        assert_eq!(timing[0]["payload"]["step_id"], serde_json::json!("s1"));
        assert_eq!(timing[0]["payload"]["kind"], serde_json::json!("procedural.shell"));
        assert!(
            timing[0]["payload"].get("wall_ms").and_then(|v| v.as_u64()).is_some(),
            "the payload must carry a real wall_ms field, got: {:#?}",
            timing[0]["payload"]
        );
        // `procedural.shell` never reports item counts. The honesty
        // contract (`StepRecord::items_in`/`items_out`, both `Option`)
        // means they must be ABSENT from the wire shape, not a fabricated
        // zero.
        assert!(timing[0]["payload"].get("items_in").is_none());
        assert!(timing[0]["payload"].get("items_out").is_none());
        drop(guard);
    }

    #[test]
    #[serial_test::serial]
    fn freeform_relaunch_after_close_mints_a_new_run_never_reopens_the_terminal_one() {
        let guard = LaunchTestGuard::new();
        guard.write_config("freeform-test-mission", FREEFORM_CONFIG);

        launch("freeform-test-mission", None, &[], None).unwrap();
        let mission_id = single_mission_id();

        crew::lifecycle::mission_close_with_reasoning(&mission_id, Some("test close")).unwrap();
        let closed: Mission =
            serde_json::from_str(&std::fs::read_to_string(crew::lifecycle::mission_path(&mission_id)).unwrap())
                .unwrap();
        assert_eq!(closed.status, MissionStatus::Finalized);

        // (#1503) Relaunch: same config, same (empty) inputs -> mints a
        // FRESH run id. The implicit reopen-of-a-terminal-mission path is
        // gone — a relaunch is a new run, never a rendezvous with a prior
        // one.
        let exit = launch("freeform-test-mission", None, &[], None).unwrap();
        assert_eq!(exit, 0);
        let ids = all_mission_ids();
        assert_eq!(ids.len(), 2, "relaunch after close must mint a SECOND mission, never reopen the first");
        let new_id = ids.into_iter().find(|id| id != &mission_id).unwrap();
        assert_ne!(new_id, mission_id, "relaunch must mint a NEW id, never reuse the terminal one");

        let new_mission: Mission =
            serde_json::from_str(&std::fs::read_to_string(crew::lifecycle::mission_path(&new_id)).unwrap()).unwrap();
        assert_eq!(new_mission.status, MissionStatus::Active, "the new run starts Active");

        // The prior (closed) mission is untouched by the relaunch.
        let still_closed: Mission =
            serde_json::from_str(&std::fs::read_to_string(crew::lifecycle::mission_path(&mission_id)).unwrap())
                .unwrap();
        assert_eq!(
            still_closed.status,
            MissionStatus::Finalized,
            "a later relaunch must never mutate the prior terminal mission"
        );
    }

    #[test]
    #[serial_test::serial]
    fn different_inputs_produce_distinct_run_ids_and_distinct_spec_fingerprints() {
        let guard = LaunchTestGuard::new();
        guard.write_config("freeform-test-mission", FREEFORM_CONFIG);

        launch("freeform-test-mission", None, &["note=first".to_string()], None).unwrap();
        launch("freeform-test-mission", None, &["note=second".to_string()], None).unwrap();

        let ids = all_mission_ids();
        assert_eq!(ids.len(), 2, "two launches must mint two missions");
        assert_ne!(ids[0], ids[1], "distinct launches always mint distinct run ids (#1503)");

        // (#1503) The load-bearing behavior different inputs now drive:
        // distinct SPEC fingerprints (distinct groups), not distinct ids
        // (every launch already gets a distinct id regardless of inputs).
        let specs: Vec<Option<MissionSpec>> = ids
            .iter()
            .map(|id| {
                let text = std::fs::read_to_string(crew::lifecycle::mission_path(id)).unwrap();
                serde_json::from_str::<Mission>(&text).unwrap().spec
            })
            .collect();
        assert_ne!(
            specs[0], specs[1],
            "different inputs must fingerprint differently — distinct spec groups"
        );
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

    #[test]
    #[serial_test::serial]
    fn dry_run_mints_nothing_for_the_generic_step_graph_path() {
        // (#1959) `coder-phase` names real inputs so the required-inputs
        // gate above passes; `--dry-run` (a synthetic `dry_run=true`
        // param, exactly what the CLI layer injects) must then resolve +
        // validate everything a real launch would, print the graph, and
        // mint NOTHING — no mission.json, no flow records.
        let guard = LaunchTestGuard::new();
        let exit = launch(
            "coder-phase",
            None,
            &[
                "workdir=/tmp/darkmux-dry-run-does-not-need-to-exist".to_string(),
                "branch=feature/x".to_string(),
                "base=main".to_string(),
                "dry_run=true".to_string(),
            ],
            None,
        )
        .expect("a dry run must succeed even though workdir was never touched");
        assert_eq!(exit, 0);
        assert!(all_mission_ids().is_empty(), "a dry run must mint no mission");
        let _ = guard;
    }

    #[test]
    #[serial_test::serial]
    fn dry_run_still_bails_on_a_missing_required_input_for_the_generic_step_graph_path() {
        // (#1959) `--dry-run` short-circuits AFTER input validation, not
        // before — the same missing-input bail a real launch would hit.
        let guard = LaunchTestGuard::new();
        let err = launch("coder-phase", None, &["dry_run=true".to_string()], None)
            .expect_err("a dry run must still bail on missing required inputs");
        let msg = err.to_string();
        assert!(msg.contains("workdir"), "{msg}");
        assert!(all_mission_ids().is_empty());
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

        let mission_id = mint_run_id("coder-phase").unwrap();
        let real_phase_ids = ensure_mission_and_phases_with_provenance(&mission_id, config, None, None).unwrap();

        // (#1530 Packet 3b-1) `register_coder_phase_kinds` now stamps the
        // coder step's own `message`/`timeout_seconds`/`image` onto
        // `Step.config` — it needs `steps` to contain the interpreted
        // graph's `<real-phase-id>-coder-step` entry, the same convention
        // `coder_phase_gate_outcome` already assumes. The built-in
        // `coder-phase` config's only phase doc id is `"build"`.
        let real_phase_id = real_phase_ids["build"].clone();
        let coder_step_id = format!("{real_phase_id}-coder-step");
        let mut steps = BTreeMap::new();
        steps.insert(
            coder_step_id.clone(),
            crew::types::Step {
                id: coder_step_id,
                task_id: format!("{real_phase_id}-coder"),
                gate: None,
                kind: "mission.coder".to_string(),
                status: NodeStatus::Planned,
                config: serde_json::Value::Null,
                started_ts: None,
                completed_ts: None,
                output: None,
            },
        );

        let registry = crew::step_kinds::StepKindRegistry::with_builtins();
        // (#1530 — one global step-kind registry) `register_coder_phase_kinds`
        // no longer registers the three kinds itself — it now assumes the
        // caller already did, exactly what `all_step_kinds` does in
        // production. Mirror that here.
        register_coder_phase_step_kinds(&registry).unwrap();
        let handles = register_coder_phase_kinds(
            &registry,
            &mission_id,
            config,
            &real_phase_ids,
            &collected,
            600,
            &mut steps,
        )
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
        // (#1546) The build-time-stamp ↔ run-time-read seam. The launcher
        // stamps `injected_budget_chars` here; `MissionCoderStepKind::
        // run_streaming` `.expect()`s it. Nothing else pins the two sides to
        // the same key, so a rename on either one is a worker-thread panic
        // inside a live mission — after the worktree exists and the mission
        // has minted. Pin the key at the point it's written.
        assert!(
            steps[&format!("{real_phase_id}-coder-step")]
                .config
                .get("injected_budget_chars")
                .and_then(|v| v.as_u64())
                .is_some(),
            "the launcher must stamp the `injected_budget_chars` key that run_streaming reads back; \
             got config {:?}",
            steps[&format!("{real_phase_id}-coder-step")].config
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
        let mission_id = mint_run_id("coder-phase").unwrap();
        let real_phase_ids = ensure_mission_and_phases_with_provenance(&mission_id, config, None, None).unwrap();
        let registry = crew::step_kinds::StepKindRegistry::with_builtins();
        // (#1530 — one global step-kind registry) The precondition check
        // this function now runs first needs these ids present — see the
        // sibling test above — so the missing-input bail (what this test
        // actually pins) is the one that fires, not the precondition one.
        register_coder_phase_step_kinds(&registry).unwrap();
        // (#1530 Packet 3b-1) The missing-input bail fires before
        // `register_coder_phase_kinds` ever touches `steps` — an empty map
        // is sufficient here.
        let mut steps = BTreeMap::new();
        let err = match register_coder_phase_kinds(
            &registry,
            &mission_id,
            config,
            &real_phase_ids,
            &collected,
            600,
            &mut steps,
        ) {
            Err(e) => e,
            Ok(_) => panic!("must bail without workdir/branch/base supplied"),
        };
        assert!(err.to_string().contains("workdir"), "{err}");
        let _ = guard;
    }

    #[test]
    #[serial_test::serial]
    fn register_coder_phase_kinds_err_arm_closes_the_guard_so_drop_never_clobbers_the_informative_envelope() {
        // (#2131 review round 2, MUST-FIX 1 — regression proof) `launch()`'s
        // `register_coder_phase_kinds` `Err` arm used to call
        // `reconcile_and_finalize_on_error` directly and `return Err(e)`
        // WITHOUT going through `guard.close` — the still-armed guard's
        // `Drop` then ran the abort writer a SECOND time, and
        // `finalize_mission` (inside `reconcile_and_finalize_on_error`)
        // overwrites `envelope.json` unconditionally — the operator lost
        // the real registration error in favor of the guard's generic
        // "aborted before a terminal outcome was recorded" text. This test
        // mirrors that exact arm against two separate mission instances:
        // one exercising the buggy shape (reconcile called directly, guard
        // left armed to Drop) to prove it clobbers, and one exercising the
        // fixed shape (`guard.close(...)`, this function's own current
        // code) to prove the informative envelope survives.
        let guard = LaunchTestGuard::new();
        let loaded = mission_config::load("coder-phase").unwrap();
        let config = &loaded.config;
        let collected: BTreeMap<String, serde_json::Value> = BTreeMap::new();
        let registry = crew::step_kinds::StepKindRegistry::with_builtins();
        register_coder_phase_step_kinds(&registry).unwrap();
        let no_tasks: Vec<crew::types::Task> = vec![];

        // ── Buggy shape: reconcile called directly, guard left armed. ──
        {
            let mission_id = mint_run_id("coder-phase").unwrap();
            let real_phase_ids =
                ensure_mission_and_phases_with_provenance(&mission_id, config, None, None).unwrap();
            let mut steps = BTreeMap::new();
            let abort_mission_id = mission_id.clone();
            let abort_config = config.clone();
            let abort_phase_ids = real_phase_ids.clone();
            let armed_guard = crate::launch_guard::LaunchFinalizeGuard::new(move || {
                let mut no_steps = BTreeMap::new();
                reconcile_and_finalize_on_error(
                    &abort_mission_id,
                    &abort_config,
                    &abort_phase_ids,
                    &[],
                    &mut no_steps,
                    &anyhow!(
                        "mission aborted — the launcher exited before a terminal outcome was recorded"
                    ),
                );
            });
            let err = match register_coder_phase_kinds(
                &registry,
                &mission_id,
                config,
                &real_phase_ids,
                &collected,
                600,
                &mut steps,
            ) {
                Err(e) => e,
                Ok(_) => panic!("must bail without workdir/branch/base supplied"),
            };
            // Pre-fix shape: reconcile runs with the REAL error, then the
            // still-armed guard drops and its generic abort writer runs a
            // second time on top of it.
            reconcile_and_finalize_on_error(&mission_id, config, &real_phase_ids, &no_tasks, &mut steps, &err);
            drop(armed_guard);

            let persisted =
                crew::lifecycle::load_envelope(&mission_id).unwrap().expect("envelope.json persisted");
            assert!(
                persisted.reason.as_deref().unwrap_or("").contains("before a terminal outcome was recorded"),
                "demonstrates the regression: Drop's generic abort writer clobbers the informative \
                 reconcile that ran just before it, got {:?}",
                persisted.reason
            );
        }

        // ── Fixed shape: guard.close(|| reconcile_and_finalize_on_error(...)). ──
        {
            let mission_id = mint_run_id("coder-phase").unwrap();
            let real_phase_ids =
                ensure_mission_and_phases_with_provenance(&mission_id, config, None, None).unwrap();
            let mut steps = BTreeMap::new();
            let abort_mission_id = mission_id.clone();
            let abort_config = config.clone();
            let abort_phase_ids = real_phase_ids.clone();
            let mut fixed_guard = crate::launch_guard::LaunchFinalizeGuard::new(move || {
                let mut no_steps = BTreeMap::new();
                reconcile_and_finalize_on_error(
                    &abort_mission_id,
                    &abort_config,
                    &abort_phase_ids,
                    &[],
                    &mut no_steps,
                    &anyhow!(
                        "mission aborted — the launcher exited before a terminal outcome was recorded"
                    ),
                );
            });
            let err = match register_coder_phase_kinds(
                &registry,
                &mission_id,
                config,
                &real_phase_ids,
                &collected,
                600,
                &mut steps,
            ) {
                Err(e) => e,
                Ok(_) => panic!("must bail without workdir/branch/base supplied"),
            };
            fixed_guard.close(|| {
                reconcile_and_finalize_on_error(&mission_id, config, &real_phase_ids, &no_tasks, &mut steps, &err)
            });
            drop(fixed_guard); // already disarmed by close() — Drop is a no-op.

            let persisted =
                crew::lifecycle::load_envelope(&mission_id).unwrap().expect("envelope.json persisted");
            assert!(
                persisted.reason.as_deref().unwrap_or("").contains("no `workdir` input was supplied"),
                "the fix: guard.close survives the informative reconcile instead of losing it to \
                 Drop's generic text, got {:?}",
                persisted.reason
            );
        }
        let _ = guard;
    }

    // ── Gate outcome map (#1284 review round 1, must-fix 1) — mirrors ──
    // coder_phase::run's own post-graph decision, pinned per condition.
    // Slots + step statuses are scripted (mocked dispatches only); the
    // load-bearing assertions are the exit code AND the phase staying
    // Running (ship-able), never auto-finalized past the sign-off gate.

    fn scripted_step(id: &str, status: NodeStatus) -> crew::types::Step {
        crew::types::Step {
            id: id.to_string(),
            task_id: format!("{id}-task"),
            gate: None,
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
            coder_slot: Arc::new(Mutex::new(Some(coder_phase::CoderStepResult {
                failed_verifiers: Vec::new(),
                tokens_total: 123,
            }))),
            verify_slot: Arc::new(Mutex::new(None)),
            // (#1530 Packet 3b-1) `coder_phase_gate_outcome` (the only
            // consumer of `scripted_gate_fixture`'s output) never reads
            // `context` — it's scripted here purely to satisfy the struct's
            // field list.
            context: Arc::new(coder_phase::CoderPhaseContext::default()),
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
            finalized_ts: None,
            paused_ts: None,
            source_input: None,
            ticket: None,
            spec: None,
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

        // (#1530 Packet 2) A fixture-scripted registry — `scripted_step`'s
        // placeholder `"mission.test"` kind resolves to nothing real, so
        // `resolve_gate_step_id` falls back to the `"<phase>-verify-step"`
        // convention (its own doc's best-effort clause).
        let registry = crew::step_kinds::StepKindRegistry::new();
        let exit = coder_phase_gate_outcome("gate-test-mission", &handles, &steps, &registry).unwrap();
        assert_eq!(exit, 2, "QA blockers must exit 2, mirroring `mission run`"); // drift-guard:allow mission run — test names the retired verb whose exit code this preserves (#1469)
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

        let registry = crew::step_kinds::StepKindRegistry::new();
        let exit = coder_phase_gate_outcome("gate-test-mission", &handles, &steps, &registry).unwrap();
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

        let registry = crew::step_kinds::StepKindRegistry::new();
        let exit = coder_phase_gate_outcome("gate-test-mission", &handles, &steps, &registry).unwrap();
        assert_eq!(exit, 3, "QA-unavailable must exit 3, mirroring `mission run`"); // drift-guard:allow mission run — test names the retired verb whose exit code this preserves (#1469)
        assert_eq!(phase_status_on_disk("gate-test-mission", phase_id), PhaseStatus::Running);
    }

    #[test]
    #[serial_test::serial]
    fn gate_outcome_indeterminate_verdict_exit_3_never_reads_as_a_pass() {
        // (#swarm-4) An UNREADABLE review — `parse_signoff` yields
        // `indeterminate` when the dispatch text carried no severity
        // markers and no clean marker (empty/truncated reply, or a marker
        // style the parser doesn't know; observed in production during
        // #66's dogfood). Before this arm the gate checked only
        // `block > 0`, so this fell through to the SAME "✓ ready for
        // sign-off" + exit 0 as a genuinely clean review — the operator
        // shipped code that was never actually reviewed. It must take the
        // exit-3 "manual review required" posture, with the phase left
        // Running so the operator's next move (re-run QA or adjudicate)
        // stays open.
        let _guard = LaunchTestGuard::new();
        let phase_id = "gate-test-mission-build";
        seed_running_instance("gate-test-mission", phase_id);
        let (handles, steps) =
            scripted_gate_fixture(phase_id, NodeStatus::Complete, NodeStatus::Complete, NodeStatus::Complete);
        *handles.verify_slot.lock().unwrap() = Some(Ok(review_output(0, 0, "indeterminate")));

        let registry = crew::step_kinds::StepKindRegistry::new();
        let exit = coder_phase_gate_outcome("gate-test-mission", &handles, &steps, &registry).unwrap();
        assert_eq!(exit, 3, "an unreadable QA response must exit 3 (manual review), never 0");
        assert!(
            !gate_outcome_reached_no_gate(&Ok(3)),
            "the gate HOLDS on indeterminate — same posture as QA-unavailable"
        );
        assert_eq!(
            phase_status_on_disk("gate-test-mission", phase_id),
            PhaseStatus::Running,
            "phase stays Running — the operator re-runs QA or adjudicates manually"
        );
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

        let registry = crew::step_kinds::StepKindRegistry::new();
        let exit = coder_phase_gate_outcome("gate-test-mission", &handles, &steps, &registry).unwrap();
        assert_eq!(exit, 0);
        assert_eq!(
            phase_status_on_disk("gate-test-mission", phase_id),
            PhaseStatus::Running,
            "even a clean run stops at the gate — `mission finalize` completes the phase, not launch"
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

        let registry = crew::step_kinds::StepKindRegistry::new();
        let err = coder_phase_gate_outcome("gate-test-mission", &handles, &steps, &registry).unwrap_err();
        assert!(err.to_string().contains("worktree already exists"), "{err}");
    }

    /// (#1530) A coder-phase graph that declares no verify step must ERROR
    /// with the naming contract named — not panic.
    ///
    /// `resolve_gate_step_id` fails OPEN by design (no step declares
    /// `is_gate()` -> it returns the `<phase>-verify-step` fallback), so
    /// before this check the fallback id went straight into `steps[&id]` and
    /// the operator got a bare `BTreeMap` panic: no step id, no config, no
    /// fix. "A coder phase without QA" is an ordinary composition now that
    /// launching routes on step KINDS rather than a config id.
    #[test]
    #[serial_test::serial]
    fn gate_outcome_missing_verify_step_errors_with_the_naming_contract() {
        let _guard = LaunchTestGuard::new();
        let phase_id = "gate-noverify-build";
        seed_running_instance("gate-noverify", phase_id);
        let (handles, mut steps) = scripted_gate_fixture(
            phase_id,
            NodeStatus::Complete,
            NodeStatus::Complete,
            NodeStatus::Planned,
        );
        // The composition under test: the verify step simply isn't there.
        steps.remove(&format!("{phase_id}-verify-step"));

        let registry = crew::step_kinds::StepKindRegistry::new();
        let err = coder_phase_gate_outcome("gate-noverify", &handles, &steps, &registry)
            .expect_err("a coder-phase graph with no verify step must error, not panic");
        let msg = format!("{err:#}");
        assert!(msg.contains("verify"), "must name which step is missing: {msg}");
        assert!(
            msg.contains(&format!("{phase_id}-verify-step")),
            "must name the exact id the launcher looked for: {msg}"
        );
        assert!(
            msg.contains("coder-phase.json"),
            "must point at the template to copy — the naming convention is documented nowhere \
             else: {msg}"
        );
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
                "ticket": "SAMPLE-4242",
                "phases": [{"id": "p1", "description": "only phase"}]
            }"#,
        );

        let exit = launch("hydration-test", None, &[], None).unwrap();
        assert_eq!(exit, 0);

        // (#1503) The run id is minted fresh, not the bare config id —
        // recover it from disk.
        let mission_id = single_mission_id();
        let mission = load_mission_for_brief(&mission_id).unwrap();
        assert_eq!(
            mission.source_input.as_deref(),
            Some("the operator's original unabridged words"),
            "source_input must ride config extras onto the mission record (#815)"
        );
        assert_eq!(
            mission.ticket.as_deref(),
            Some("SAMPLE-4242"),
            "ticket must ride config extras onto the mission record (#816)"
        );
    }

    // ── run id minting is unique, never derived from inputs (#1503) ─────

    #[test]
    fn zero_input_launch_still_mints_a_config_id_prefixed_run_id() {
        // (#1503) A run id is never derived from inputs at all now — zero
        // inputs no longer collapses onto the bare config id (that
        // must-fix-3 concern doesn't apply once every launch mints
        // uniquely); it's still config-id-PREFIXED for readability.
        let id = mint_run_id("draft-blog-post").unwrap();
        assert!(id.starts_with("draft-blog-post-"), "a minted id is still config-id-prefixed, got {id}");
        assert_ne!(id, "draft-blog-post", "a minted id is never the bare config id — every launch is unique");
    }

    #[test]
    fn launch_session_id_carries_the_viewer_mission_run_prefix() {
        // (#1284 review round 1, must-fix 4) the viewer's mission lens keys
        // per-run session grouping on the `mission-run-` prefix (originally
        // the legacy `viewer.html`'s convention; that file retired #1806).
        let sid = launch_session_id("m1", "m1-build");
        assert_eq!(sid, "mission-run-m1-m1-build");
        assert!(sid.starts_with("mission-run-"));
    }

    // ── gate-arm failure predicate (#1433 follow-up) ────────────────────

    #[test]
    fn gate_predicate_finalizes_only_the_no_gate_failures() {
        // Failure exits that never reached a reviewable gate: reconcile.
        assert!(gate_outcome_reached_no_gate(&Err(anyhow!("worktree bail"))));
        assert!(gate_outcome_reached_no_gate(&Ok(1)), "coder dispatch error finalizes");
        // Gate exits PROPER must NOT finalize. Ok(2) is the load-bearing pin:
        // a regression to `Err(_) | Ok(1) | Ok(2)` would close the mission
        // under QA-blockers before sign-off (#1463) — this line is what
        // fails on that regression.
        assert!(!gate_outcome_reached_no_gate(&Ok(0)), "clean gate holds for sign-off");
        assert!(!gate_outcome_reached_no_gate(&Ok(2)), "QA-blockers gate holds for fix, never finalizes");
        assert!(!gate_outcome_reached_no_gate(&Ok(3)), "QA-unavailable gate holds for manual review");
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
    fn mint_run_id_is_unique_per_call_and_charset_safe() {
        // (#1503) `mint_run_id` takes no inputs at all — it can't derive
        // anything from them. Two mints of the SAME config id must never
        // collide, and the result must satisfy `fleet::validate_identifier`'s
        // charset ([a-z0-9_-]).
        let a = mint_run_id("coder-phase").unwrap();
        let b = mint_run_id("coder-phase").unwrap();
        assert_ne!(a, b, "two mints of the same config id must never collide");
        assert!(a.starts_with("coder-phase-"));
        assert!(
            a.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_'),
            "minted id must satisfy fleet::validate_identifier's charset: {a}"
        );
    }

    #[test]
    fn spec_fingerprint_is_deterministic_and_differs_on_different_inputs() {
        // (#1503) The grouping key: same inputs -> same fingerprint (runs
        // group); different inputs -> different fingerprint (distinct
        // groups) — the exact behavior the old `derive_mission_id` used to
        // give the mission ITSELF, now demoted to metadata.
        let mut m = BTreeMap::new();
        m.insert("workdir".to_string(), serde_json::json!("/tmp/x"));
        let a = spec_fingerprint(&m).unwrap();
        let b = spec_fingerprint(&m).unwrap();
        assert_eq!(a, b, "same inputs must fingerprint identically (so runs group)");

        let mut m2 = m.clone();
        m2.insert("workdir".to_string(), serde_json::json!("/tmp/y"));
        let c = spec_fingerprint(&m2).unwrap();
        assert_ne!(a, c, "different inputs must fingerprint differently (distinct groups)");
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
            panel: None,
            cmd: None,
            extras: BTreeMap::new(),
        };
        let missing = missing_required_inputs(&cfg, &BTreeMap::new());
        let names: Vec<&str> = missing.iter().map(|i| i.name.as_str()).collect();
        assert_eq!(names, vec!["workdir"], "mission_id (launcher-supplied) and image (optional) must not appear");
    }

    /// (#1530) Structural conformance: every step kind the SHIPPED mission
    /// configs declare must be in the known-kind union `launch` feeds to
    /// `MissionConfig::validate` — the Tier-1 builtins plus
    /// `CODER_PHASE_TIER3_KINDS` plus `REVIEW_TIER3_KINDS`. A kind missing
    /// from that union doesn't fail: it emits a `Warning` per step, so every
    /// launch of that config prints a spurious "unknown step kind" line and
    /// erodes a loud-validation surface. Adding `review.probe-render` hit
    /// exactly that, caught only by review; this pins the lists against the
    /// documents instead of against a reviewer noticing. Reads the embedded
    /// templates directly (never `mission_config::load`, which would prefer a
    /// user-tier copy and make the test machine-dependent).
    #[test]
    fn every_builtin_config_step_kind_is_in_the_launchers_known_set() {
        const BUILTIN_CONFIG_DOCS: &[(&str, &str)] = &[
            (
                "review",
                include_str!(concat!(
                    env!("CARGO_MANIFEST_DIR"),
                    "/templates/builtin/mission-configs/review.json"
                )),
            ),
            (
                "coder-phase",
                include_str!(concat!(
                    env!("CARGO_MANIFEST_DIR"),
                    "/templates/builtin/mission-configs/coder-phase.json"
                )),
            ),
        ];

        // (#1530 — one global step-kind registry) `known` now comes straight
        // from `all_step_kinds`'s real registry — the SAME one `launch`
        // validates every config against — rather than a hand-maintained
        // union of `CODER_PHASE_TIER3_KINDS`/`REVIEW_TIER3_KINDS` that could
        // silently drift from what the registry can actually construct.
        // This makes the test STRICTLY stronger: it now also fails if a kind
        // is listed in one of those consts but was never actually
        // registered (previously invisible to this test, since the old
        // `known` list was built from the consts directly, not the registry).
        let registry = all_step_kinds().expect("all_step_kinds must build cleanly in a test process");
        let known: Vec<String> = registry.ids();

        for (config_id, doc) in BUILTIN_CONFIG_DOCS {
            let cfg: MissionConfig = serde_json::from_str(doc)
                .unwrap_or_else(|e| panic!("built-in config \"{config_id}\" must parse: {e}"));
            for phase in &cfg.phases {
                for task in &phase.tasks {
                    for step in &task.steps {
                        assert!(
                            known.contains(&step.kind),
                            "built-in config \"{config_id}\" declares step kind `{}` (step \
                             `{}`), which is absent from `all_step_kinds`'s registry (Tier-1 \
                             builtins + darkmux-lab's review kinds + this crate's coder-phase \
                             kinds) — every `mission launch {config_id}` would print a spurious \
                             \"unknown step kind\" warning. Register it.",
                            step.kind,
                            step.id
                        );
                    }
                }
            }
        }
    }

    /// (#1530 — one global step-kind registry) The payoff this packet
    /// exists to deliver: a config whose GRAPH names BOTH a coder-phase step
    /// (`mission.coder`) and a review step (`review.judge`) resolves EVERY
    /// kind against `all_step_kinds`'s single shared registry — never a real
    /// built-in document shape (each pipeline's dedicated launcher owns its
    /// own document), but exactly the capability that was structurally
    /// impossible before this packet: `review`'s dedicated launcher built
    /// its own registry via `StepKindRegistry::with_builtins()` +
    /// `register_review_kinds` (nothing of `mission.*`), and this module's
    /// execution registry built its own via `with_builtins()` +
    /// `register_coder_phase_kinds` (nothing of `review.*`) — no single
    /// registry instance could ever have resolved a graph naming both
    /// families at once.
    #[test]
    fn both_families_resolve_against_one_registry() {
        let cfg: MissionConfig = serde_json::from_value(serde_json::json!({
            "id": "mixed-families-test",
            "name": "mixed families",
            "phases": [{
                "id": "p1",
                "name": "p1",
                "tasks": [
                    { "id": "t1", "steps": [{ "id": "s1", "kind": "mission.coder" }] },
                    { "id": "t2", "steps": [{ "id": "s2", "kind": "review.judge" }] },
                ]
            }]
        }))
        .expect("minimal mixed-family config deserializes");

        let registry = all_step_kinds().expect("all_step_kinds must build cleanly in a test process");
        let known_ids = registry.ids();
        let known_kinds: Vec<&str> = known_ids.iter().map(String::as_str).collect();

        // The SAME validation pass `launch` runs on every config: a step
        // whose kind isn't in `known_kinds` produces an "unknown step kind"
        // warning (`mission_config::validate`'s own doc). Neither
        // `mission.coder` nor `review.judge` should trigger one — both
        // resolve against this ONE registry.
        let findings = cfg.validate(&known_kinds);
        let kind_warnings: Vec<_> =
            findings.iter().filter(|f| f.message.contains("unknown step kind")).collect();
        assert!(
            kind_warnings.is_empty(),
            "a graph naming both a coder-phase kind and a review kind must produce no \
             \"unknown step kind\" warnings against ONE shared registry — got: {kind_warnings:?}"
        );

        // And each kind resolves to a real, constructible `StepKind` through
        // that SAME registry instance (not merely absent from the warning
        // list — actually gettable).
        assert!(registry.get("dispatch.internal").is_ok(), "dispatch.internal must resolve");
        assert!(registry.get("mission.coder").is_ok(), "mission.coder must resolve");
        assert!(registry.get("review.judge").is_ok(), "review.judge must resolve");
        // The legacy funnel.* alias review registers alongside review.judge.
        assert!(registry.get("funnel.judge").is_ok(), "funnel.judge legacy alias must resolve");
    }

    /// (#1549) The overrides must reach a coder-phase-shaped graph even when
    /// the config is NOT named `coder-phase` and its tasks are NOT named
    /// `build-coder`/`build-verify`. Both literals used to gate this, while
    /// EXECUTION already routed structurally — so a copied config ran fine
    /// but persisted no `Task.workdir`, and `resolve_run_workdir` then sent
    /// `mission finalize`/`abort` to the derived path instead of the
    /// operator's actual worktree. Silent, and worse for `abort`.
    #[test]
    fn task_overrides_reach_a_renamed_coder_phase_config() {
        let cfg: MissionConfig = serde_json::from_value(serde_json::json!({
            "id": "coder-phase-lean",
            "name": "Lean coder phase",
            "phases": [{
                "id": "build",
                "name": "build",
                "tasks": [
                    { "id": "wt",  "steps": [{ "id": "s-wt",  "kind": "mission.worktree" }] },
                    { "id": "code", "depends_on": ["wt"],
                      "steps": [{ "id": "s-code", "kind": "mission.coder" }] },
                    { "id": "qa",  "depends_on": ["code"],
                      "steps": [{ "id": "s-qa",  "kind": "mission.verify" }] }
                ]
            }]
        }))
        .expect("config parses");

        let mut collected = BTreeMap::new();
        collected.insert("workdir".to_string(), serde_json::json!("/tmp/some-other-tree"));
        collected.insert("role".to_string(), serde_json::json!("coder"));

        let params = build_launch_params(&cfg, &BTreeMap::new(), &collected);

        let coder = params.task_overrides.get("code").expect(
            "the task declaring `mission.coder` must receive the overrides, whatever it is named",
        );
        assert_eq!(coder.workdir.as_deref(), Some(std::path::Path::new("/tmp/some-other-tree")));
        assert_eq!(coder.role_id.as_deref(), Some("coder"));

        // The verify task's workdir is the one `resolve_run_workdir` reads
        // back, so finalize/abort target the real worktree.
        let verify = params
            .task_overrides
            .get("qa")
            .expect("the task declaring `mission.verify` must receive the workdir override");
        assert_eq!(verify.workdir.as_deref(), Some(std::path::Path::new("/tmp/some-other-tree")));

        // And a graph with none of the coder-phase kinds still gets nothing.
        let plain = config_with_kind("something", "dispatch.internal");
        assert!(build_launch_params(&plain, &BTreeMap::new(), &collected).task_overrides.is_empty());
    }

    // ── #1530: review routes STRUCTURALLY (by its kinds), not by id ──

    fn config_with_kind(id: &str, kind: &str) -> MissionConfig {
        // Minimal graph carrying one step of `kind`; other fields default.
        serde_json::from_value(serde_json::json!({
            "id": id,
            "name": id,
            "phases": [{
                "id": "p1",
                "name": "p1",
                "tasks": [{ "id": "t1", "steps": [{ "id": "s1", "kind": kind }] }]
            }]
        }))
        .expect("minimal config deserializes")
    }

    #[test]
    fn review_routes_by_kind_not_by_the_literal_id() {
        // The canonical `review` config still routes (kinds present).
        assert!(config_uses_review_kinds(&config_with_kind("review", "review.judge")));
        // The whole point of #1530: a differently-NAMED review variant
        // (`review-lean`) routes to the same dedicated driver — the old
        // `id == "review"` literal would have missed it and dropped it onto
        // the generic path, which cannot construct the review kinds.
        assert!(config_uses_review_kinds(&config_with_kind("review-lean", "review.synthesis")));
        // A non-review graph does NOT route to the review driver.
        assert!(!config_uses_review_kinds(&config_with_kind("something", "dispatch.internal")));
        // And a config named "review" with no review kinds is NOT forced to
        // the review driver — routing is the graph's shape, never the name.
        assert!(!config_uses_review_kinds(&config_with_kind("review", "dispatch.single_shot")));
    }

    /// (#1635) Killed a mutant `cargo-mutants` found surviving in
    /// `lazy_start_phase_for_step`: flipping `||` to `&&` in the guard
    ///
    /// ```ignore
    /// if phase_id.is_empty() || !started.insert(phase_id.to_string()) {
    /// ```
    ///
    /// changed when a phase starts, and NO test noticed — every phase test
    /// passes a non-empty id, so the empty-string arm was unconstrained.
    ///
    /// An empty `phase_id` is not hypothetical: the generic launcher's persist
    /// closure derives it via `.unwrap_or_default()` when a step's task is not
    /// in the map, so a graph with a dangling task reference reaches here with
    /// `""`. Under the mutant that would `phase_start("")` and mint lifecycle
    /// records against a phase that does not exist.
    #[test]
    #[serial_test::serial]
    fn an_empty_phase_id_never_starts_anything() {
        let _guard = LaunchTestGuard::new();
        let mut started = std::collections::HashSet::new();

        assert!(
            !lazy_start_phase_for_step("some-mission", "", NodeStatus::Running, &mut started),
            "an empty phase id must never report a phase as newly started"
        );
        assert!(
            started.is_empty(),
            "and must not be recorded as started — a later real phase with a derived empty \
             id would then be silently skipped"
        );

        // The other arm of the same guard: a real id starts exactly once.
        assert!(lazy_start_phase_for_step("some-mission", "p1", NodeStatus::Running, &mut started));
        assert!(
            !lazy_start_phase_for_step("some-mission", "p1", NodeStatus::Running, &mut started),
            "a second step in the same phase must not re-report it as newly started"
        );
    }

    /// (#1632) The invariant, asserted at the LIFECYCLE level rather than for
    /// one launcher's phase count.
    ///
    /// Phases are strictly linear (#1341), so at most ONE may be Running at any
    /// instant. #1620 fixed that for the review launcher and left the generic
    /// one — every non-review config, including coder-phase and anything from
    /// `mission propose` — starting phases it never closed. The board then read
    /// `0/N` for the whole run and jumped to `N/N`, indistinguishable from a
    /// mission that never started.
    ///
    /// The old test walked p1 -> Running, p2 -> Running and asserted nothing
    /// about p1 closing, so it PASSED while demonstrating the defect. This one
    /// asserts the property that makes the defect impossible, and it holds for
    /// any launcher that drives phases through these two helpers.
    #[test]
    #[serial_test::serial]
    fn at_most_one_phase_is_running_as_a_generic_mission_advances() {
        let _guard = LaunchTestGuard::new();
        let config: MissionConfig = serde_json::from_str(FREEFORM_CONFIG).unwrap();
        let mission_id = "one-running-test";
        let real = ensure_mission_and_phases_with_provenance(mission_id, &config, None, None).unwrap();
        let order: Vec<String> =
            config.phases.iter().filter_map(|p| real.get(&p.id).cloned()).collect();
        assert!(order.len() >= 2, "the fixture must have enough phases to advance between");

        let running_count = |ids: &[String]| {
            ids.iter()
                .filter(|id| phase_status_on_disk(mission_id, id) == PhaseStatus::Running)
                .count()
        };

        let mut started = std::collections::HashSet::new();
        let mut closed = std::collections::HashSet::new();

        // Phase 1 goes live.
        assert!(lazy_start_phase_for_step(mission_id, &order[0], NodeStatus::Running, &mut started));
        lazy_close_prior_phases(mission_id, &order, &order[0], &mut closed);
        assert_eq!(running_count(&order), 1, "exactly one phase live after the first advance");

        // Give phase 1 a finished step, then advance. Without a terminal step
        // the close path deliberately leaves it alone (an empty step list is
        // absence of evidence, not evidence of failure — the bug inside
        // #1620's own first draft), so this mirrors a real completed band.
        let step = crew::types::Step {
            id: format!("{}-s1", order[0]),
            task_id: format!("{}-t1", order[0]),
            gate: None,
            kind: "procedural.noop".to_string(),
            status: NodeStatus::Complete,
            config: serde_json::Value::Null,
            started_ts: None,
            completed_ts: None,
            output: None,
        };
        crew::lifecycle::save_step(mission_id, &order[0], &step).unwrap();

        assert!(lazy_start_phase_for_step(mission_id, &order[1], NodeStatus::Running, &mut started));
        lazy_close_prior_phases(mission_id, &order, &order[1], &mut closed);

        assert_eq!(
            running_count(&order),
            1,
            "advancing must CLOSE the band behind it — two Running phases contradicts #1341"
        );
        assert_eq!(
            phase_status_on_disk(mission_id, &order[0]),
            PhaseStatus::Complete,
            "and the closed band keeps the outcome it earned"
        );
    }

    // ── #1400: lazy phase start ("phase 2 stays planned until reached") ──

    #[test]
    #[serial_test::serial]
    fn lazy_start_phase_for_step_only_starts_the_phase_its_step_belongs_to() {
        let _guard = LaunchTestGuard::new();
        let config: MissionConfig = serde_json::from_str(FREEFORM_CONFIG).unwrap();
        let mission_id = "lazy-start-test";
        let real_phase_ids = ensure_mission_and_phases_with_provenance(mission_id, &config, None, None).unwrap();
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
        let real_phase_ids = ensure_mission_and_phases_with_provenance(mission_id, &config, None, None).unwrap();
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

    /// (#1503) The reuse/reopen-by-derived-id path is gone: a mint against
    /// an id that already exists on disk — which should never happen in
    /// practice, since `mint_run_id` mints uniquely — bails loud rather
    /// than silently reopening a terminal mission or mutating live phase
    /// state. This replaces the pre-#1503 reopen-semantics regression test
    /// (`reopen_preserves_terminal_phase_status_and_restarts_only_
    /// abandoned_ones_lazily`), which asserted the removed reopen
    /// behavior — a SECOND `ensure_mission_and_phases` call against the
    /// same id used to reactivate a Finalized mission and restart only
    /// what reruns; now it errors instead, and the original (however
    /// terminal or mid-flight) record is left completely untouched.
    #[test]
    #[serial_test::serial]
    fn ensure_mission_and_phases_bails_loud_on_an_existing_mission_id_never_reopens() {
        let _guard = LaunchTestGuard::new();
        let config: MissionConfig = serde_json::from_str(FREEFORM_CONFIG).unwrap();
        let mission_id = "collision-test";
        let real_phase_ids = ensure_mission_and_phases_with_provenance(mission_id, &config, None, None).unwrap();
        let p1 = &real_phase_ids["p1"];

        crew::lifecycle::phase_start(p1).unwrap();
        crew::lifecycle::phase_complete(p1).unwrap();
        crew::lifecycle::mission_close_with_reasoning(mission_id, Some("test close")).unwrap();
        assert_eq!(mission_status_on_disk(mission_id), MissionStatus::Finalized);

        // A second mint against the SAME id must bail loud, never reopen.
        let err = ensure_mission_and_phases_with_provenance(mission_id, &config, None, None).unwrap_err();
        assert!(
            err.to_string().contains("already exists"),
            "a second mint against an existing id must bail loud, never reopen: {err}"
        );

        // The original terminal mission and its completed phase are
        // completely untouched by the failed second mint.
        assert_eq!(
            mission_status_on_disk(mission_id),
            MissionStatus::Finalized,
            "a bailed-out collision attempt must never mutate the existing record"
        );
        assert_eq!(phase_status_on_disk(mission_id, p1), PhaseStatus::Complete);
    }

    /// (#1504) The strand-accumulation gap: `ensure_mission_and_phases_with_
    /// provenance`'s own `?` in `launch()` had NO reconcile — a partial mint
    /// (mission.json written, a later phase save fails) would strand a
    /// fresh, permanently-Active mission. Since #1503 mints a UNIQUE run id
    /// per launch, a repeated failing launch no longer converges onto one
    /// reused instance — each failure would strand its OWN mission absent
    /// this reconcile. Forces the failure deterministically (no OS-
    /// permission tricks, portable): pre-occupies the `phases/` subdir path
    /// with a plain file so `fs::create_dir_all` can't create a directory
    /// there, failing the very first `save_phase` call right after
    /// `save_mission` already succeeded — exactly the partial-mint shape.
    #[test]
    #[serial_test::serial]
    fn post_mint_phase_write_failure_reconciles_via_mint_failure_helper_not_stranded_active() {
        let _guard = LaunchTestGuard::new();
        let config: MissionConfig = serde_json::from_str(FREEFORM_CONFIG).unwrap();
        let mission_id = "post-mint-strand";

        let mission_dir = crew::lifecycle::mission_path(mission_id).parent().unwrap().to_path_buf();
        std::fs::create_dir_all(&mission_dir).unwrap();
        std::fs::write(mission_dir.join("phases"), b"blocks phase dir creation").unwrap();

        let err = ensure_mission_and_phases_with_provenance(mission_id, &config, None, None).unwrap_err();
        assert_eq!(
            mission_status_on_disk(mission_id),
            MissionStatus::Active,
            "sanity: mission.json WAS written before the phase save failed"
        );

        // Exactly what `launch()`'s error arm now does on this failure.
        crew::lifecycle::reconcile_mint_failure(
            mission_id,
            &format!("mission launch errored during mint: {err:#}"),
        );

        assert_eq!(
            mission_status_on_disk(mission_id),
            MissionStatus::Finalized,
            "a partial mint must reconcile to terminal — one fresh mission, terminal, never an \
             accumulating Active row (#1504)"
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
            reads: Vec::new(),
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
            finalized_ts: None,
            paused_ts: None,
            source_input: None,
            ticket: None,
            spec: None,
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
        // (#1877 item 4 — deferred, pinned) The generic scheduler graph
        // never produces a `RunOutcome` yet — see `build_envelope`'s own
        // doc for why (the `MissionOutcomeStatus::from_outcome` Error-gap,
        // not a "different shape" mismatch). `outcome` stays `None` even
        // on a Degraded run.
        assert!(env.outcome.is_none());

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
    fn phase_finalization_rule4_two_step_phase_mixed_complete_and_planned_abandons() {
        // (#1433 gate coverage) One phase, TWO steps in its task: one Complete,
        // one Planned. Not all-complete, none errored, at least one started →
        // phase_finalization's rule 4 (the non-terminal-leftover branch):
        // Abandoned with the "did not complete" reason, NOT Complete.
        let config: MissionConfig =
            serde_json::from_str(r#"{"id":"r4","name":"r4","phases":[{"id":"p1"}]}"#).unwrap();
        let mid = "r4";
        let real = derive_phase_ids(mid, &config);
        let rp1 = real["p1"].clone();
        let mut task = task_with_step(&rp1, "p1-s1");
        task.step_ids.push("p1-s2".to_string());
        let tasks = vec![task];
        let mut steps = BTreeMap::new();
        steps.insert("p1-s1".to_string(), scripted_step("p1-s1", NodeStatus::Complete));
        steps.insert("p1-s2".to_string(), scripted_step("p1-s2", NodeStatus::Planned));

        let outcomes = derive_phase_outcomes(&config, &real, &tasks, &steps);
        use crew::envelope::PhaseOutcomeKind;
        let o = outcomes.iter().find(|o| o.phase_id == rp1).expect("phase p1 present");
        assert_eq!(o.outcome, PhaseOutcomeKind::Abandoned, "a phase with a leftover non-terminal step abandons");
        assert_eq!(
            o.reason.as_deref(),
            Some("phase did not complete (steps left non-terminal)"),
            "rule 4 (non-terminal leftover), distinct from the errored / never-started reasons"
        );
    }

    #[test]
    fn build_envelope_omits_task_less_phases_in_a_mixed_config() {
        // (#1433 gate coverage) A mixed config: p1 has a task (executed), p2 has
        // NONE (a freeform phase this launcher never drives). Only the executed
        // phase appears in the envelope's per-phase outcomes; the task-less
        // phase is omitted, never stamped with a finalize outcome it didn't run.
        let config: MissionConfig =
            serde_json::from_str(r#"{"id":"z0","name":"z0","phases":[{"id":"p1"},{"id":"p2"}]}"#).unwrap();
        let mid = "z0";
        let real = derive_phase_ids(mid, &config);
        let (rp1, rp2) = (real["p1"].clone(), real["p2"].clone());
        let tasks = vec![task_with_step(&rp1, "p1-s1")]; // no task for p2
        let mut steps = BTreeMap::new();
        steps.insert("p1-s1".to_string(), scripted_step("p1-s1", NodeStatus::Complete));

        let env = build_envelope(mid, &config, &real, &tasks, &steps);
        use crew::envelope::MissionOutcomeStatus;
        assert_eq!(env.status, MissionOutcomeStatus::Clean, "the one executed step completed → Clean");
        assert!(env.phases.iter().any(|p| p.phase_id == rp1), "executed phase p1 is finalized");
        assert!(
            !env.phases.iter().any(|p| p.phase_id == rp2),
            "task-less phase p2 is omitted, not finalized"
        );
    }

    #[test]
    #[serial_test::serial]
    fn reconcile_with_no_tasks_closes_mission_and_reconciles_planned_phases() {
        // (#1433 follow-up, revised #1504) The pre-graph strand window: a `?`
        // before interpret ever produced tasks (a config-snapshot write
        // fault, an interpret fault). Reconcile runs with EMPTY tasks/steps —
        // nothing to flip, no per-phase outcomes to derive — so `envelope.
        // phases` is empty and `finalize_mission`'s own per-phase loop never
        // touches any of the three phases. Before #1504, that left the
        // mission Finalized with its phases stranded Planned — the exact
        // invariant violation #1504 targets, just one level up (mission↔phase
        // instead of phase↔step). `mission_close_with_reasoning`'s own #1504
        // reconcile now catches it: every non-terminal phase (and any step
        // it might contain) rolls to Abandoned as part of the SAME close
        // call, so a Finalized mission never persists with a live phase.
        let _guard = LaunchTestGuard::new();
        let config: MissionConfig = serde_json::from_str(GEN3_CONFIG).unwrap();
        let mid = "gen3strand";
        let real = derive_phase_ids(mid, &config);
        let (rp1, rp2, rp3) = (real["p1"].clone(), real["p2"].clone(), real["p3"].clone());
        seed_mission_with_phases(
            mid,
            &[(&rp1, PhaseStatus::Planned), (&rp2, PhaseStatus::Planned), (&rp3, PhaseStatus::Planned)],
        );

        let err = anyhow::anyhow!("persisting config-snapshot.json: disk full");
        let mut no_steps = BTreeMap::new();
        reconcile_and_finalize_on_error(mid, &config, &real, &[], &mut no_steps, &err);

        assert_eq!(mission_status_on_disk(mid), MissionStatus::Finalized, "no stranded Active mission");
        for rp in [&rp1, &rp2, &rp3] {
            assert_eq!(
                phase_status_on_disk(mid, rp),
                PhaseStatus::Abandoned,
                "no phase may be left Planned inside a Finalized mission (#1504) — `finalize_mission`'s \
                 own empty phase loop leaves them untouched, but `mission_close_with_reasoning`'s \
                 reconcile catches every leftover before the mission closes"
            );
        }
    }

    #[test]
    #[serial_test::serial]
    fn finalize_the_1406_scenario_agrees_with_disk() {
        // (#1406) End to end: build the honest envelope for the issue's
        // scenario and finalize it against seeded disk state, asserting the
        // mission Finalizes with p1 Complete, p2 terminal-not-complete, p3
        // Abandoned (no phase left Planned inside a Finalized mission), and
        // the persisted envelope.json agrees with the phase files.
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
            "p3 must NEVER be left Planned inside a Finalized mission (the #1406 bug)"
        );
        assert_eq!(phase_status_on_disk(mid, &rp3), PhaseStatus::Abandoned);
        assert_eq!(mission_status_on_disk(mid), MissionStatus::Finalized);

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
        assert_eq!(mission_status_on_disk(mid), MissionStatus::Finalized, "the failed run is no longer stranded Active");
        assert_eq!(phase_status_on_disk(mid, &rp1), PhaseStatus::Complete);
        assert_eq!(phase_status_on_disk(mid, &rp2), PhaseStatus::Abandoned);
        assert_eq!(phase_status_on_disk(mid, &rp3), PhaseStatus::Abandoned);

        use crew::envelope::MissionOutcomeStatus;
        let persisted = crew::lifecycle::load_envelope(mid).unwrap().expect("envelope.json persisted");
        assert_eq!(persisted.status, MissionOutcomeStatus::Error, "a hard scheduler Err finalizes to Error status");
    }

    // ── #1877 — telemetry + the whole-run dispatch bookend, prescribed ──

    #[test]
    fn mission_bookend_record_stamps_mission_source_session_and_mission_id() {
        let rec = mission_bookend_record(
            flow::Level::Info,
            "dispatch start",
            "coder-phase",
            "coder-phase-123-abcdef",
            serde_json::json!({ "runtime": "mission" }),
        );
        assert_eq!(rec.action, "dispatch start");
        assert_eq!(rec.handle, "coder-phase");
        assert_eq!(rec.session_id.as_deref(), Some("coder-phase-123-abcdef"));
        assert_eq!(rec.mission_id.as_deref(), Some("coder-phase-123-abcdef"));
        // (#1877 requirement 2) `source` must be distinct from BOTH the
        // per-model-dispatch FROZEN `"crew_dispatch"` value and review's
        // own `"review"` — otherwise the viewer can't tell a whole-run
        // bookend apart from an individual model call or a review run.
        assert_eq!(rec.source.as_deref(), Some("mission"));
        assert_ne!(rec.source.as_deref(), Some("crew_dispatch"));
        assert_ne!(rec.source.as_deref(), Some("review"));
    }

    // ── #1877 QA must-fix 3 — the coder branch's terminal bookend must ──
    // key on `reached_gate`, not `outcome == Ok(0)`, and the record it
    // produces must be distinguishable from the `BookendGuard` Drop
    // backstop's generic abort record.
    //
    // RED PROVED (mutation per the QA finding): swapping this function's
    // `let reached_gate = !gate_outcome_reached_no_gate(outcome);` for
    // `let reached_gate = matches!(outcome, Ok(0));` (the literal
    // `reached_gate = success` mutation named in the finding) failed
    // `coder_branch_terminal_bookend_ok_2_qa_blockers_still_reaches_the_gate`
    // and `..._ok_3_qa_unavailable_still_reaches_the_gate` below: both
    // asserted `action == "dispatch complete"` and instead got
    // `"dispatch error"`.

    #[test]
    fn coder_branch_terminal_bookend_ok_0_clean_reaches_the_gate() {
        let (reached_gate, rec) = coder_branch_terminal_bookend(&Ok(0), "coder-phase", "m-1");
        assert!(reached_gate);
        assert_eq!(rec.action, "dispatch complete");
        assert!(matches!(rec.level, flow::Level::Info), "{:?}", rec.level);
        assert_eq!(rec.payload.as_ref().unwrap()["result_class"], serde_json::json!("ok"));
        assert_eq!(rec.payload.as_ref().unwrap()["gate"], serde_json::json!("coder-phase"));
    }

    #[test]
    fn coder_branch_terminal_bookend_ok_2_qa_blockers_still_reaches_the_gate() {
        // `coder_phase_gate_outcome`'s own table: QA found blocker(s)
        // leaves the phase Running at the sign-off gate — real dispatch
        // work that started and FINISHED, same as clean. Must close
        // `dispatch complete`, never `dispatch error`.
        let (reached_gate, rec) = coder_branch_terminal_bookend(&Ok(2), "coder-phase", "m-2");
        assert!(reached_gate, "Ok(2) (QA blockers) must still reach the gate");
        assert_eq!(rec.action, "dispatch complete");
        assert!(matches!(rec.level, flow::Level::Info), "{:?}", rec.level);
    }

    #[test]
    fn coder_branch_terminal_bookend_ok_3_qa_unavailable_still_reaches_the_gate() {
        let (reached_gate, rec) = coder_branch_terminal_bookend(&Ok(3), "coder-phase", "m-3");
        assert!(reached_gate, "Ok(3) (QA unavailable) must still reach the gate");
        assert_eq!(rec.action, "dispatch complete");
        assert!(matches!(rec.level, flow::Level::Info), "{:?}", rec.level);
    }

    #[test]
    fn coder_branch_terminal_bookend_ok_1_coder_dispatch_failure_never_reaches_the_gate() {
        let (reached_gate, rec) = coder_branch_terminal_bookend(&Ok(1), "coder-phase", "m-4");
        assert!(!reached_gate);
        assert_eq!(rec.action, "dispatch error");
        assert!(matches!(rec.level, flow::Level::Error), "{:?}", rec.level);
        assert_eq!(rec.payload.as_ref().unwrap()["result_class"], serde_json::json!("error"));
    }

    #[test]
    fn coder_branch_terminal_bookend_err_worktree_failure_never_reaches_the_gate() {
        let (reached_gate, rec) =
            coder_branch_terminal_bookend(&Err(anyhow!("worktree already exists")), "coder-phase", "m-5");
        assert!(!reached_gate);
        assert_eq!(rec.action, "dispatch error");
        assert!(matches!(rec.level, flow::Level::Error), "{:?}", rec.level);
    }

    /// (#1877 QA must-fix 3) The explicit close's payload always carries
    /// `gate: "coder-phase"` and NEVER an `error` key — the opposite shape
    /// from the `BookendGuard` Drop backstop's generic abort record (see
    /// `launch`'s `bookend` construction: `on_abort` builds a payload with
    /// `"error": "mission dispatch terminated before completion..."` and no
    /// `gate` key). Distinguishing the two shapes is what lets
    /// `launch_coder_phase_worktree_failure_still_closes_the_mission_
    /// bookend_as_dispatch_error` (below) prove the explicit close actually
    /// ran, not just that SOME "dispatch error" record landed.
    #[test]
    fn coder_branch_terminal_bookend_payload_never_carries_an_error_key() {
        let (_, rec) = coder_branch_terminal_bookend(&Err(anyhow!("boom")), "coder-phase", "m-6");
        let payload = rec.payload.as_ref().unwrap();
        assert!(
            payload.get("error").is_none(),
            "the explicit close's payload must not carry the Drop backstop's `error` key: {payload:?}"
        );
        assert_eq!(payload["gate"], serde_json::json!("coder-phase"));
    }

    /// `HostTelemetrySampler` itself never stamps `mission_id` (it doesn't
    /// know one — see its own doc). `drained_telemetry` is the one place
    /// that backfill happens for the generic launch path, mirroring
    /// `mission_launch_review.rs`'s `FleetFlowEmitter`, which does the same
    /// for `review`'s own samples. Fast injected cadence (5ms) — same
    /// discipline `crates/darkmux-crew/src/run_obs.rs`'s own tests use — so
    /// this doesn't race the real ~600-900ms `top`/`vm_stat`/`ioreg` shells
    /// against the production 2s cadence `launch` itself uses.
    #[test]
    fn drained_telemetry_backfills_mission_id_onto_samples_that_lack_one() {
        fn fake_sample() -> darkmux_crew::telemetry_sampler::HostSample {
            darkmux_crew::telemetry_sampler::HostSample { cpu: Some(1), mem: Some(2), gpu: Some(3) }
        }
        // (#1877 QA nit) A non-empty `Ok(..)` — NOT `Ok(Vec::new())` — so
        // `lms_diff`'s first (unseeded) call actually reports a load and
        // the sampler's `telemetry.lms` half (routed through the SAME
        // `try_drain` channel `telemetry.process` uses) gets covered too,
        // not just the process-sample half.
        fn fake_lms() -> anyhow::Result<Vec<darkmux_types::LoadedModel>> {
            Ok(vec![darkmux_types::LoadedModel {
                identifier: "darkmux:fake-model".to_string(),
                model: "fake-model".to_string(),
                status: "loaded".to_string(),
                size: "1GB".to_string(),
                context: 4096,
            }])
        }
        let telemetry = run_obs::HostTelemetrySampler::start(
            "case".to_string(),
            "crew".to_string(),
            std::time::Duration::from_millis(5),
            std::time::Duration::from_millis(2),
            fake_sample,
            fake_lms,
        );
        std::thread::sleep(std::time::Duration::from_millis(200));
        let samples = drained_telemetry(&telemetry, "mission-xyz");
        assert!(!samples.is_empty(), "no sample landed within 200ms (40x the 5ms cadence) — sampler did not run");
        assert!(
            samples.iter().any(|s| s.action == "telemetry.process"),
            "expected at least one `telemetry.process` sample: {samples:#?}"
        );
        assert!(
            samples.iter().any(|s| s.action == "telemetry.lms"),
            "expected at least one `telemetry.lms` sample (the fake resident model should have \
             produced a load diff on the first, unseeded call): {samples:#?}"
        );
        assert!(
            samples.iter().all(|s| s.mission_id.as_deref() == Some("mission-xyz")),
            "every drained sample of EVERY kind must carry the backfilled mission_id: {samples:#?}"
        );
        drop(telemetry);
    }

    /// (#1877 "Bookends fire on a panic") Mirrors `darkmux_flow::bookend`'s
    /// own `panic_while_armed_still_fires_the_abort_record` test, but
    /// exercises the EXACT construction `launch` uses (`mission_bookend_
    /// record` + `BookendGuard::new`'s `on_abort` closure) rather than a
    /// synthetic fixture — proving THIS integration keeps the RAII
    /// guarantee, not just the generic mechanism `darkmux-flow`'s own suite
    /// already covers.
    #[test]
    fn mission_bookend_guard_fires_a_dispatch_error_abort_record_on_panic() {
        let mut records: Vec<flow::FlowRecord> = Vec::new();
        let mut sink = |r: flow::FlowRecord| records.push(r);
        let prev_hook = std::panic::take_hook();
        std::panic::set_hook(Box::new(|_| {}));
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let mut guard = flow::BookendGuard::new(&mut sink, move |_id, _kind| {
                mission_bookend_record(
                    flow::Level::Error,
                    "dispatch error",
                    "panic-test-config",
                    "panic-test-mission",
                    serde_json::json!({
                        "runtime": "mission",
                        "result_class": "error",
                        "error": "mission dispatch terminated before completion (early return or panic)",
                    }),
                )
            });
            guard.open(
                "dispatch",
                "dispatch",
                mission_bookend_record(
                    flow::Level::Info,
                    "dispatch start",
                    "panic-test-config",
                    "panic-test-mission",
                    serde_json::json!({ "runtime": "mission" }),
                ),
            );
            panic!("simulated mid-run panic while the guard is armed");
        }));
        std::panic::set_hook(prev_hook);
        assert!(result.is_err(), "the panic must propagate out of catch_unwind");
        assert_eq!(records.len(), 2, "expected [start, abort]: {records:#?}");
        assert_eq!(records[0].action, "dispatch start");
        assert_eq!(records[1].action, "dispatch error");
        assert_eq!(records[1].source.as_deref(), Some("mission"));
    }

    /// (#1877 requirement 2 — "review must stop building its own, or you
    /// get two samplers double-sampling one run") Structural proof of the
    /// reconciliation this arc chose: NEITHER — `review` never receives
    /// this guard, and it isn't made nestable either, because the two
    /// constructions are mutually exclusive by CONTROL FLOW. `launch`
    /// returns into the dedicated `mission_launch_review::launch` (which
    /// mints no `mission_id` and builds its own telemetry sampler +
    /// `with_dispatch_bookends` privately) strictly BEFORE this generic
    /// path ever reaches `run_obs::HostTelemetrySampler::start` — so
    /// double-sampling is not a runtime risk to guard against, it is
    /// unreachable code. This test pins the ORDERING so a future refactor
    /// that moved the guard construction earlier (or the review branch
    /// later) would fail loud here instead of silently double-sampling
    /// review runs in production.
    #[test]
    fn review_branch_returns_before_the_mission_telemetry_bookend_guard_is_ever_constructed() {
        const SRC: &str = include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/src/mission_launch.rs"));
        let review_branch = SRC
            .find("if config_uses_review_kinds(config) {")
            .expect("the review-kind routing branch must exist in mission_launch.rs");
        let guard_construction = SRC
            .find("run_obs::HostTelemetrySampler::start(")
            .expect("the mission telemetry guard must exist in mission_launch.rs");
        assert!(
            review_branch < guard_construction,
            "the review-kind branch (which returns into mission_launch_review::launch before \
             minting a mission_id here) must appear BEFORE the telemetry/bookend guard is \
             constructed — otherwise review's own sampler and this generic one could both run \
             over the same review dispatch"
        );
    }

    /// (#1877 QA should-fix 5) The source-ordering test above pins TEXTUAL
    /// position, not the actual invariant — a refactor that extracts the
    /// guard construction into a helper `fn` defined earlier in the file
    /// (called later) would still fail it while the invariant holds, and
    /// one that moves the review branch's CALL later while its definition
    /// stays early would pass it while double-sampling. This test pins the
    /// invariant BEHAVIORALLY instead: launch a config whose graph uses a
    /// review kind and assert zero `source: "mission"` records land,
    /// regardless of how `launch`'s internals are structured.
    ///
    /// The config fails fast inside `mission_launch_review::launch` on the
    /// missing required `diff_file` input (`launch`'s own doc — no
    /// LMStudio, no worktree, no real work reachable before that `?`) —
    /// which is fine: the claim under test is "review never reaches the
    /// generic guard," not "review succeeds," and a review-kind config
    /// that never even reaches the missing-input bail (a hang, a panic, a
    /// successful launch) would ALSO be caught here, since any of those
    /// would either fail this test's `expect_err` or leave the flow
    /// records inspected below unrepresentative.
    #[test]
    #[serial_test::serial]
    fn review_kind_config_launch_emits_zero_mission_source_records() {
        let guard = LaunchTestGuard::new();
        const REVIEW_KIND_CONFIG: &str = r#"{
            "id": "review-kind-test-mission",
            "name": "Review Kind Test Mission",
            "schema_version": "2.3",
            "phases": [{
                "id": "p1",
                "tasks": [{ "id": "t1", "steps": [{ "id": "s1", "kind": "review.judge" }] }]
            }]
        }"#;
        guard.write_config("review-kind-test-mission", REVIEW_KIND_CONFIG);

        let err = launch("review-kind-test-mission", None, &[], None)
            .expect_err("no `diff_file` input was supplied — the review launcher must bail");
        assert!(
            err.to_string().contains("diff_file"),
            "sanity: this must be review's own missing-input bail, not some other failure: {err}"
        );

        let records = read_all_flow_records();
        assert!(
            records.iter().all(|r| r["source"] != "mission"),
            "a review-kind config must NEVER reach the generic guard's `source: \"mission\"` \
             bookend — got {:#?}",
            records.iter().filter(|r| r["source"] == "mission").collect::<Vec<_>>()
        );
        drop(guard);
    }

    /// (#1877 QA should-fix 6) `read_all_flow_records`-based tests can only
    /// observe telemetry SAMPLES landing at the real 2s production cadence
    /// (`run_obs.rs`'s own "sleep first, then sample" design deliberately
    /// makes that impossible to race in a sub-second test — see this
    /// module's `drained_telemetry_backfills_...` test, which uses an
    /// injected fast cadence instead). So nothing in this file's `launch_*`
    /// tests would notice all four `drained_telemetry(&telemetry, ...)`
    /// call sites being deleted from `launch` — a real, silent way to lose
    /// telemetry interleaving without any test going red. Pin the call
    /// SITE COUNT as a cheap structural backstop: one per exit
    /// (`run_step_graph`'s own emit closure, the scheduler-error return,
    /// the coder-gate return, and the gate-less finish).
    #[test]
    fn launch_drains_telemetry_at_every_emission_point_and_every_exit() {
        const SRC: &str = include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/src/mission_launch.rs"));
        // Built via `concat!` (not a plain string literal) so this test's
        // OWN source line — which necessarily names the exact call shape
        // it's counting — doesn't self-match and inflate the count by one,
        // the same idiom `mission_launch_review_and_review_bench_construct_
        // graphs_through_the_same_launcher`'s `run_needle` uses for the
        // identical reason.
        let needle = concat!("drained_telemetry(&telemetry, ", "&mission_id)");
        let count = SRC.matches(needle).count();
        assert_eq!(
            count, 4,
            "expected exactly 4 call sites of `{needle}` in mission_launch.rs (the run_step_graph \
             emit closure + the 3 known exit points) — got {count}. If this changed on purpose, \
             update this count; if not, telemetry interleaving silently regressed somewhere."
        );
    }

    /// (#1877 requirement 1 — "no opt-in": a Tier-1-only, no-model-dispatch
    /// generic config now gets a mission-level `dispatch *` bookend where it
    /// previously got NONE at all.) Reuses `MIXED_OUTCOME_CONFIG` — one
    /// completed step, one errored step, `MissionOutcomeStatus::Degraded`,
    /// exit 0 — because it's the shape every operator-authored `cmd`
    /// panel config actually produces on a partial run, and because a
    /// Degraded-but-exit-0 run closing as `dispatch complete` (never
    /// `dispatch error`) is itself worth pinning: the bookend carries
    /// "did dispatch work happen and finish," not the mission's own
    /// pass/fail verdict.
    ///
    /// RED PROVED: before this arc's `mission_launch.rs` changes, NO
    /// record with `source == "mission"` was ever emitted on this path —
    /// `read_all_flow_records()` filtered to `source: "mission"` was
    /// empty for every generic (non-review) launch, confirmed by running
    /// this exact assertion against the pre-change tree.
    #[test]
    #[serial_test::serial]
    fn launch_of_a_tier1_generic_config_emits_exactly_one_mission_dispatch_bookend_pair() {
        let guard = LaunchTestGuard::new();
        guard.write_config("mixed-outcome-test-mission", MIXED_OUTCOME_CONFIG);

        let exit = launch("mixed-outcome-test-mission", None, &[], None)
            .expect("a mixed complete/errored generic run must still return an exit code, not an Err");
        assert_eq!(exit, 0);

        let mission_id = single_mission_id();
        let records = read_all_flow_records();
        let mission_records: Vec<&serde_json::Value> =
            records.iter().filter(|r| r["source"] == "mission").collect();
        let starts: Vec<&&serde_json::Value> =
            mission_records.iter().filter(|r| r["action"] == "dispatch start").collect();
        let completes: Vec<&&serde_json::Value> =
            mission_records.iter().filter(|r| r["action"] == "dispatch complete").collect();
        let errors: Vec<&&serde_json::Value> =
            mission_records.iter().filter(|r| r["action"] == "dispatch error").collect();
        assert_eq!(
            starts.len(),
            1,
            "expected exactly one mission-level `dispatch start`, got {mission_records:#?}"
        );
        assert_eq!(
            completes.len() + errors.len(),
            1,
            "expected exactly one mission-level terminal bookend, got {mission_records:#?}"
        );
        assert_eq!(
            completes.len(),
            1,
            "a Degraded-but-exit-0 run must close as `dispatch complete`, not `dispatch error`: \
             {mission_records:#?}"
        );
        for r in &starts {
            assert_eq!(r["mission_id"], serde_json::json!(mission_id));
            assert_eq!(r["session_id"], serde_json::json!(mission_id));
        }
        drop(guard);
    }

    /// (#1877 requirement 4/5 — "bookends fire on the coder branch's early
    /// return") `launch`'s coder-phase gate arm (`if let Some(handles) =
    /// &coder_handles { ... }`) returns EARLY — before `build_envelope`,
    /// before the gate-less generic finish — on a worktree-creation
    /// failure (`coder_phase_gate_outcome`'s documented "worktree step
    /// errored -> hard `Err`" row). An invalid `base` ref makes `git
    /// worktree add` fail immediately (verified: no worktree or branch is
    /// ever registered against this repo — see this test's own assertions),
    /// so this exercises the REAL early-return path, not a mock.
    ///
    /// This runs `git worktree add` against THIS repo (`coder_phase::
    /// repo_root()` resolves from the test process's own cwd), not a
    /// throwaway tempdir repo — deliberately: an invalid `base` ref is
    /// already verified to fail before git creates anything, and building
    /// an isolated fixture repo would mean the test itself mutating
    /// `std::env::set_current_dir`, which is process-wide and would race
    /// every OTHER test running concurrently in this binary — a strictly
    /// worse risk than the one it would avoid.
    ///
    /// RED PROVED: before this arc's change, this exact scenario produced
    /// zero `source: "mission"` records — confirmed by running this
    /// assertion against the pre-change tree, where the coder branch's
    /// early return had no bookend to close at all.
    #[test]
    #[serial_test::serial]
    fn launch_coder_phase_worktree_failure_still_closes_the_mission_bookend_as_dispatch_error() {
        let guard = LaunchTestGuard::new();
        let tmp = TempDir::new().unwrap();
        let workdir = tmp.path().join("wt");

        let err = launch(
            "coder-phase",
            None,
            &[
                format!("workdir={}", workdir.display()),
                "branch=mission-launch-test-branch".to_string(),
                "base=mission-launch-tests-definitely-not-a-real-ref".to_string(),
            ],
            None,
        )
        .expect_err("an invalid `base` ref must fail worktree creation before any dispatch");
        let msg = err.to_string().to_lowercase();
        assert!(
            msg.contains("worktree") || msg.contains("git"),
            "sanity: this must be the worktree-creation failure this test targets, got: {err}"
        );
        assert!(!workdir.exists(), "sanity: an invalid base ref must never actually create the worktree dir");

        let mission_id = single_mission_id();
        let records = read_all_flow_records();
        let mission_records: Vec<&serde_json::Value> =
            records.iter().filter(|r| r["source"] == "mission").collect();
        let starts: Vec<&&serde_json::Value> =
            mission_records.iter().filter(|r| r["action"] == "dispatch start").collect();
        let errors: Vec<&&serde_json::Value> =
            mission_records.iter().filter(|r| r["action"] == "dispatch error").collect();
        assert_eq!(
            starts.len(),
            1,
            "the coder branch's early return must still have opened a mission-level bookend: \
             {mission_records:#?}"
        );
        assert_eq!(
            errors.len(),
            1,
            "the coder branch's early return must close as `dispatch error`, explicitly — not \
             rely on the Drop backstop for a KNOWN outcome: {mission_records:#?}"
        );
        assert_eq!(errors[0]["mission_id"], serde_json::json!(mission_id));
        // (#1877 QA must-fix 3) `errors.len() == 1` alone can't tell the
        // explicit `bookend.close(...)` apart from the `BookendGuard` Drop
        // backstop — both emit exactly one "dispatch error" record. Only
        // the PAYLOAD shape distinguishes them: the explicit close stamps
        // `gate: "coder-phase"` (see `coder_branch_terminal_bookend`) and
        // carries no `error` key; the Drop backstop's `on_abort` closure
        // does the opposite (an `error` key naming "terminated before
        // completion", no `gate` key). Asserting `gate` here is what
        // proves this test exercises the explicit close this arc added —
        // not just "some dispatch-error record landed" — which is exactly
        // what this test's own docstring claims but, before this
        // assertion, never actually checked.
        //
        // RED PROVED: deleting the `bookend.close(...)` call in `launch`'s
        // coder branch (leaving only the Drop backstop to fire on the
        // early `return outcome`) still left `starts.len() == 1` and
        // `errors.len() == 1` passing, but this `payload["gate"]`
        // assertion failed (`gate` absent) and `payload["error"]` was
        // present instead.
        assert_eq!(
            errors[0]["payload"]["gate"],
            serde_json::json!("coder-phase"),
            "must be the explicit coder-branch close, not the Drop backstop's generic abort \
             record: {mission_records:#?}"
        );
        assert!(
            errors[0]["payload"].get("error").is_none(),
            "the Drop backstop's abort record carries an `error` key instead of `gate` — seeing \
             one here means `bookend.close` was never called: {mission_records:#?}"
        );
        drop(guard);
    }

    // ── (#1877, the arc's acceptance test) coder-phase step records reach
    // the flow stream without touching `src/coder_phase.rs` ─────────────

    /// #1877's own issue: the scheduler produces a `StepRecord` per step for
    /// every mission (#1886), telemetry/bookends are prescribed for every
    /// mission (#1899), but nothing reads the records: `src/mission_
    /// launch.rs:848` binds `graph_result` and only ever inspects it for
    /// `Err`. This test is the arc's acceptance test named in that issue:
    /// `coder-phase` must demonstrably receive step records without a
    /// single line changing in `src/coder_phase.rs`.
    ///
    /// Why this can't go through `launch("coder-phase", ...)` the way the
    /// worktree-FAILURE test above does: a SUCCESSFUL worktree step would
    /// let the graph continue straight into the `mission.coder` step, which
    /// dispatches for real (LMStudio/Docker), not appropriate for a fast
    /// unit test, and not what this test needs to prove. What actually
    /// needs proving is narrower and mission-agnostic: does the SHARED
    /// scheduler machinery (`apply_step_terminal`, touched by this arc)
    /// emit the new record for a coder-phase-shaped step. So this drives
    /// `crew::scheduler::run_step_graph` directly against a ONE-step graph
    /// built from `coder_phase::MissionWorktreeStepKind`, the REAL,
    /// UNEDITED Tier-3 kind `coder_phase.rs` defines (#1352: coder-phase's
    /// bespoke kinds stay physically co-located with the mission module
    /// that owns them, never duplicated for a test), exercised against a
    /// real temp git repo. `git worktree add` genuinely runs; there is no
    /// mock and no dispatch, because this kind's `residency()` needs
    /// neither, proven directly by the panicking host factory below. This
    /// bypasses `launch()`'s own `register_coder_phase_kinds` (which needs
    /// a full `coder-phase` config plus a resolvable default profile,
    /// machinery orthogonal to what's under test here) but seeds the SAME
    /// `CoderPhaseContext` artifact shape production seeds, through the
    /// SAME `run_step_graph` caller-seed path production uses.
    ///
    /// Since the destination is the flow stream and the wiring lives
    /// entirely in `darkmux-crew`'s scheduler (never in `mission_launch.rs`
    /// or `coder_phase.rs`), this test proves the actual claim: the
    /// substrate reaches coder-phase by construction, not because this
    /// launcher was special-cased for it.
    ///
    /// **Proved failing first**: before `apply_step_terminal` called
    /// `emit(step_timing_record(...))`, the `timing` filter below found
    /// zero records and `assert_eq!(timing.len(), 1)` failed. Observed
    /// directly by temporarily reverting the `scheduler.rs` emission call
    /// and rerunning this exact test.
    #[test]
    #[serial_test::serial]
    fn coder_phase_step_records_reach_the_flow_stream_without_touching_coder_phase_rs() {
        let guard = LaunchTestGuard::new();

        let repo = TempDir::new().unwrap();
        let git = |args: &[&str]| {
            let status = std::process::Command::new("git")
                .current_dir(repo.path())
                .args(args)
                .status()
                .expect("git must be on PATH for this test");
            assert!(status.success(), "git {args:?} failed");
        };
        git(&["init", "-q"]);
        git(&["config", "user.email", "test@example.com"]);
        git(&["config", "user.name", "Test"]);
        std::fs::write(repo.path().join("README.md"), "hello\n").unwrap();
        git(&["add", "."]);
        git(&["commit", "-q", "-m", "initial"]);

        let wt_path = repo.path().join("wt-under-test");
        let ctx = coder_phase::CoderPhaseContext {
            repo_root: repo.path().to_path_buf(),
            wt_path: wt_path.clone(),
            branch: "coder-phase-step-timing-test".to_string(),
            base: "HEAD".to_string(),
            mission_id: "m-test".to_string(),
            phase_id: "p-test".to_string(),
            session_id: "s-test".to_string(),
            role: "coder".to_string(),
        };
        let seed_artifacts: Vec<(&'static str, Arc<dyn Any + Send + Sync>)> =
            vec![(coder_phase::CODER_CONTEXT_ARTIFACT, Arc::new(ctx) as Arc<dyn Any + Send + Sync>)];

        let registry = crew::step_kinds::StepKindRegistry::new();
        registry.register(Arc::new(coder_phase::MissionWorktreeStepKind)).unwrap();

        let task = crew::types::Task {
            id: "t1".to_string(),
            phase_id: "p-test".to_string(),
            description: "worktree".to_string(),
            display_name: None,
            step_ids: vec!["s1".to_string()],
            depends_on: Vec::new(),
            reads: Vec::new(),
            role_id: None,
            profile_name: None,
            workdir: None,
            image: None,
        };
        let step = Step {
            id: "s1".to_string(),
            task_id: "t1".to_string(),
            gate: None,
            kind: "mission.worktree".to_string(),
            status: NodeStatus::Planned,
            config: serde_json::json!(null),
            started_ts: None,
            completed_ts: None,
            output: None,
        };
        let mut steps: BTreeMap<String, Step> = [(step.id.clone(), step)].into_iter().collect();
        let tasks: BTreeMap<String, crew::types::Task> = [(task.id.clone(), task)].into_iter().collect();

        let facts = crew::step_kinds::Facts::default();
        let est = crew::step_kinds::FixedEstimator::default();
        let report = crew::scheduler::run_step_graph(
            &mut steps,
            &tasks,
            &registry,
            &facts,
            &est,
            1,
            &|| {
                panic!(
                    "mission.worktree needs no model residency: the host factory must never \
                     be called"
                )
            },
            &mut |r| {
                let _ = flow::record(r);
            },
            &mut |_step| {},
            None,
            None,
            &seed_artifacts,
        )
        .expect("the worktree step must complete against a real git repo");

        assert_eq!(steps["s1"].status, NodeStatus::Complete, "the worktree step must reach Complete");
        assert!(wt_path.exists(), "sanity: the worktree must actually have been created on disk");

        // The in-memory summary already worked before this arc's final
        // wiring step (#1886 shipped it).
        assert_eq!(report.step_records.len(), 1);
        assert_eq!(report.step_records[0].kind, "mission.worktree");

        // The actual point of this test: the SAME data reached the
        // durable flow stream, live, under its own vocabulary, with no
        // change to `src/coder_phase.rs`.
        let records = read_all_flow_records();
        let timing: Vec<&serde_json::Value> = records
            .iter()
            .filter(|r| r.get("action").and_then(|v| v.as_str()) == Some("step timing"))
            .collect();
        assert_eq!(
            timing.len(),
            1,
            "expected exactly one \"step timing\" record for the coder-phase worktree step, \
             got: {records:#?}"
        );
        assert_eq!(timing[0]["payload"]["kind"], serde_json::json!("mission.worktree"));
        assert_eq!(timing[0]["payload"]["step_id"], serde_json::json!("s1"));
        assert_eq!(timing[0]["source"], serde_json::json!("scheduler"));
        let wall_ms = timing[0]["payload"]["wall_ms"].as_u64().expect("a real, present duration");
        assert!(
            wall_ms > 0,
            "a real `git worktree add` subprocess must take a measurable, non-zero amount of \
             time, got {wall_ms}ms"
        );

        // The worktree kind's OWN pre-existing `"step result"` companion
        // (coder_phase.rs's `emit_step_result`, untouched by this arc)
        // still fires too, proving the two vocabularies coexist for the
        // SAME step without collision, which is the vocabulary decision
        // this arc had to make explicit rather than merge silently.
        let step_result: Vec<&serde_json::Value> = records
            .iter()
            .filter(|r| r.get("action").and_then(|v| v.as_str()) == Some("step result"))
            .collect();
        assert_eq!(step_result.len(), 1, "the worktree kind's own step-result companion must still fire");
        assert_eq!(step_result[0]["payload"]["kind"], serde_json::json!("mission.worktree"));

        drop(guard);
    }
}
