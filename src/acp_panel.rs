//! Registry-advertised ACP panel commands + ephemeral procedural launches
//! (#1684, Packet 1 of the #1684/#1685 arc).
//!
//! `src/acp.rs`'s spike hardcoded a single `/review` slash command. This
//! module replaces that: it enumerates the SAME merged mission-config
//! registry `darkmux mission launch`/`darkmux mission status` already use
//! (`crew::mission_config::list_ids` + `crew::mission_config::load` — the
//! user tier `~/.darkmux/mission-configs/` over the built-ins), filters to
//! configs that declare a `panel` block ([`crew::mission_config::PanelConfig`]),
//! and decides — per the operator-ratified design — how an invoked command
//! should run:
//!
//! - A config whose graph structurally uses a review-pipeline step kind
//!   (`crate::mission_launch::config_uses_review_kinds` — the SAME test
//!   `src/mission_launch.rs::launch` uses, never an `id == "review"`
//!   string literal) keeps `acp.rs`'s EXISTING bespoke path (`run_review`)
//!   unchanged — this module never touches it beyond routing to it.
//! - A config whose graph contains ZERO model-dispatching steps (every
//!   step kind is `procedural.*`) runs EPHEMERAL: [`run_ephemeral`]
//!   interprets the config's graph and drives it through
//!   `darkmux_crew::scheduler::run_step_graph` directly, in-process, with
//!   NO mission instance minted and NO lifecycle records — "instances for
//!   work you'd revisit, flow records for acts you'd audit." Steps still
//!   emit their own flow records through the ordinary flow sink
//!   (`crate::flow::record`), each stamped with a per-invocation
//!   correlation id (see `run_ephemeral`'s doc) so concurrent runs of the
//!   same config don't collide in the viewer.
//! - Anything else (at least one model-seated step) launches as a normal
//!   `darkmux mission launch <id>` subprocess — a full instance, same
//!   pattern `acp.rs` already uses for `review`'s own subprocess spawn.
//!
//! **Advertised id vs. document id.** A [`PanelCommand`]'s `id` is always
//! the REGISTRY-RESOLVABLE key (what `list_ids()`/`load()` key on — an
//! on-disk filename stem, effectively), never the JSON body's own
//! `MissionConfig.id` field — those two strings can differ on an
//! operator's hand-edited config, and advertising the wrong one would show
//! Zed a command that can never actually resolve. See [`PanelCommand`]'s
//! own doc.
//!
//! This module owns the REGISTRY ENUMERATION, the ROUTING DECISION, and
//! the EPHEMERAL RUNNER. `acp.rs` owns the ACP wire-protocol plumbing
//! (session/new advertising, session/prompt dispatch, subprocess spawning
//! for the `Launch`/`Review` routes) — split so each stays independently
//! readable.

use crate::crew::mission_config::{
    self, LaunchParams, MissionConfig, PhaseConfig, StepConfig, TaskConfig, PANEL_ARGS_TASK_ID,
};
use crate::crew::scheduler::SchedulerReport;
use crate::crew::step_kinds::{Facts, FixedEstimator, StepKindRegistry};
use crate::crew::types::{NodeStatus, Step, Task};
use anyhow::{Context, Result};
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

/// One entry `darkmux acp` advertises as a slash command in the editor's
/// agent panel.
///
/// `id` is the REGISTRY-RESOLVABLE key — the same string
/// `crew::mission_config::list_ids()` returned it under, and therefore the
/// same string `crew::mission_config::load(id)` resolves — NEVER the
/// document body's own `MissionConfig.id` field (#1684 QA finding: those
/// two can differ when an operator's on-disk filename and the JSON body's
/// `id` field drift, e.g. a copy-pasted config nobody renamed internally;
/// advertising the body id would show a command in Zed that
/// [`route_command`]/`mission_config::load` can never actually resolve).
///
/// `description` is [`PanelConfig::description`] (a short UI label),
/// falling back to `MissionConfig.name` — NEVER `MissionConfig.description`
/// (#1684 QA finding: that field is deliberately long-form developer
/// provenance prose, unsuitable for a command-palette entry; the built-in
/// `review` config's is ~2KB).
#[derive(Debug, Clone)]
pub struct PanelCommand {
    pub id: String,
    pub description: String,
    pub hint: Option<String>,
}

/// Enumerate every mission config in the merged registry (built-ins +
/// `~/.darkmux/mission-configs/`) that declares a `panel` block — the
/// advertising filter. Reuses `crew::mission_config::list_ids` +
/// `crew::mission_config::load` (the SAME resolution `darkmux mission
/// launch`/`darkmux mission status` already use) rather than
/// re-implementing discovery. A config that fails to load (malformed JSON,
/// e.g. from a hand-edited operator override) is skipped with a stderr
/// note — one broken config must never take down the whole command list.
pub fn list_panel_commands() -> Vec<PanelCommand> {
    let mut out = Vec::new();
    for id in mission_config::list_ids() {
        let loaded = match mission_config::load(&id) {
            Ok(l) => l,
            Err(e) => {
                eprintln!(
                    "[darkmux-acp] skipping mission config \"{id}\" while listing panel \
                     commands: {e:#}"
                );
                continue;
            }
        };
        let Some(panel) = &loaded.config.panel else { continue };
        out.push(PanelCommand {
            // The RESOLVABLE key (`id`, from `list_ids()`), never
            // `loaded.config.id` — see `PanelCommand`'s own doc.
            id: id.clone(),
            description: panel
                .description
                .clone()
                .unwrap_or_else(|| loaded.config.name.clone()),
            hint: panel.hint.clone(),
        });
    }
    out.sort_by(|a, b| a.id.cmp(&b.id));
    out
}

/// A human-readable fallback for a prompt that didn't match any currently
/// advertised command — lists the live command set instead of hardcoding
/// `/review` the way the pre-#1684 spike did.
pub fn not_a_command_message(commands: &[PanelCommand]) -> String {
    if commands.is_empty() {
        return "darkmux acp has no commands to advertise right now — no mission config in \
                the merged registry (built-ins + ~/.darkmux/mission-configs/) declares a \
                `panel` block."
            .to_string();
    }
    let list = commands.iter().map(|c| format!("`/{}`", c.id)).collect::<Vec<_>>().join(", ");
    format!("darkmux acp doesn't recognize that as a command. Available commands: {list}.")
}

/// Split a raw prompt into `(command name, raw args)`. Strips a leading
/// `/` when present but tolerates the no-slash form too (matching the
/// pre-#1684 spike's own `/review`-or-`review` leniency); the first
/// whitespace-delimited word, LOWERCASED (mission-config ids are
/// conventionally lowercase-kebab, and the pre-#1684 spike itself
/// lowercased — `/Review` must still resolve), is the command name;
/// everything after it (trimmed, case PRESERVED) is the raw args string
/// forwarded verbatim to whichever route the command resolves to. `None`
/// for empty/whitespace-only text.
pub fn parse_command(text: &str) -> Option<(String, String)> {
    let trimmed = text.trim();
    let without_slash = trimmed.strip_prefix('/').unwrap_or(trimmed);
    if without_slash.is_empty() {
        return None;
    }
    let mut parts = without_slash.splitn(2, char::is_whitespace);
    let name = parts.next()?.to_ascii_lowercase();
    let args = parts.next().unwrap_or("").trim().to_string();
    Some((name, args))
}

/// What `session/prompt`'s command dispatch decided to do with an invoked
/// command name — see [`route_command`].
pub enum RoutePlan {
    /// The config's graph uses a review-pipeline step kind
    /// (`config_uses_review_kinds`) — the EXISTING bespoke path in
    /// `acp.rs` (`run_review`), unchanged by this module. Carries the
    /// REGISTRY-RESOLVABLE id (#1695 merge-gate MUST FIX) so `run_review`
    /// spawns `mission launch <this-id>`, never a hardcoded `"review"` —
    /// a panel-advertised review VARIANT (an operator config carrying
    /// `review.*` kinds under a different id, e.g. `review-lean`) must
    /// launch ITSELF, not silently launch the built-in `review` config in
    /// its place.
    Review(String),
    /// The config's graph contains ZERO model-dispatching steps (every
    /// step kind is `procedural.*`) — run in-process via [`run_ephemeral`],
    /// no mission instance minted.
    Ephemeral(Box<MissionConfig>),
    /// The config's graph has at least one model-seated step — launch it
    /// as a normal `darkmux mission launch <id>` subprocess (a full
    /// instance), same pattern as `review`'s own subprocess.
    Launch(String),
}

/// Decide how an invoked command name should run. `None` when `cmd` is not
/// one of the CURRENTLY advertised commands — `advertised` is recomputed
/// per prompt by the caller (the registry can change between `session/new`
/// and a later `session/prompt`), never cached across the session's
/// lifetime. A command that WAS advertised but no longer resolves (e.g.
/// its file was deleted between `session/new` and this prompt) also
/// returns `None` here rather than panicking — `advertised` already
/// reflects the current registry, so this is defense in depth, not the
/// primary check.
///
/// Matching is CASE-INSENSITIVE against the advertised registry keys
/// (#1695 merge-gate finding 2) — `cmd` arrives already lowercased from
/// [`parse_command`], but a registry key (an on-disk filename stem) keeps
/// whatever case the operator gave the file, so a naive `==` would make a
/// mixed-case filename permanently unreachable even though it was
/// advertised. `advertised` is caller-sorted ([`list_panel_commands`]),
/// so `.find()`'s first case-insensitive match is deterministic — two
/// configs that collide only after lowercasing resolve to whichever sorts
/// first, never a panic. The MATCHED entry's own (correctly-cased) `id` is
/// what actually gets loaded/launched — never the lowercased `cmd` the
/// user typed.
///
/// The review route is decided STRUCTURALLY — `config_uses_review_kinds`,
/// the SAME test `src/mission_launch.rs::launch` uses to route a config to
/// the dedicated review launcher (#1530) — never by an `id == "review"`
/// string literal (#1684 QA finding). A renamed variant (`review-lean`)
/// carrying real `review.*` step kinds still reaches the bespoke stage-plan
/// path; an id that merely happens to be named `"review"` but carries no
/// review kinds does not.
pub fn route_command(advertised: &[PanelCommand], cmd: &str) -> Option<RoutePlan> {
    let matched = advertised.iter().find(|c| c.id.eq_ignore_ascii_case(cmd))?;
    let resolved_id = matched.id.clone();
    let loaded = mission_config::load(&resolved_id).ok()?;
    if crate::mission_launch::config_uses_review_kinds(&loaded.config) {
        return Some(RoutePlan::Review(resolved_id));
    }
    if is_procedural_only(&loaded.config) {
        Some(RoutePlan::Ephemeral(Box::new(loaded.config)))
    } else {
        Some(RoutePlan::Launch(resolved_id))
    }
}

/// `true` iff the config's graph declares at least one step AND every
/// declared step kind's REGISTRY ID is prefixed `procedural.` — the
/// ephemeral-vs-mission-launch routing test (rule D). This is a
/// declared-KIND test, not a runtime guarantee that zero model work can
/// possibly happen: a `procedural.shell` step could itself invoke
/// `darkmux dispatch` (or any other model-touching command) from inside
/// its shell command. The routing rule governs what this LAUNCHER
/// declares/dispatches directly, matching every Tier 1 builtin's naming
/// convention (`procedural.*` vs `dispatch.*`) and failing safe for any
/// unrecognized Tier 2/3 kind (routes to `Launch`, a full instance, never
/// silently ephemeral). A config with zero steps anywhere (a freeform
/// document — every phase manual, nothing to dispatch) is NOT ephemeral:
/// `mission launch` already handles the freeform mint-and-work-by-hand
/// path correctly, and there is nothing for an in-process runner to
/// execute.
pub fn is_procedural_only(config: &MissionConfig) -> bool {
    let mut saw_step = false;
    for phase in &config.phases {
        for task in &phase.tasks {
            for step in &task.steps {
                saw_step = true;
                if !step.kind.starts_with("procedural.") {
                    return false;
                }
            }
        }
    }
    saw_step
}

/// Run a procedural-only config's graph in-process — no mission instance
/// minted, no lifecycle records (rule D: "instances for work you'd
/// revisit, flow records for acts you'd audit"). Steps still emit their
/// own flow records through the ordinary sink (`crate::flow::record`,
/// the SAME sink `mission launch`/`dispatch` use); only the
/// mission/phase/task/step INSTANCE persistence is skipped — the `persist`
/// callback below is a deliberate no-op, a documented-valid `run_step_graph`
/// caller shape (see that function's own doc on `persist`).
///
/// Returns the TERMINAL step's `output` (rule E) — the sink task (the one
/// no other task's `depends_on`/`reads` names) in DOCUMENT order, its last
/// step. `cwd` (rule C: "the subprocess/execution cwd is the session's cwd
/// — always") is filled into every `procedural.shell` step's config that
/// doesn't already declare its own `cwd`, never overriding a step-authored
/// one.
///
/// Blocking — every call in here (`interpret`, `run_step_graph`,
/// `procedural.shell`'s own `std::process::Command::output()`) is
/// synchronous. `acp.rs`'s caller runs this on a `tokio::task::
/// spawn_blocking` thread rather than the connection's own async task, so
/// it never stalls the ACP event loop.
///
/// `gate` (#1684 Packet 2) is the operator sign-off gate handler for any
/// step in `config`'s graph that declares `"gate": "operator"` (e.g. the
/// `pr-merge`/`pr-approve` example verbs — see `mission_config::StepConfig::
/// gate`). `acp.rs`'s `session/prompt` handler wires the ACP `session/
/// request_permission` handler here (a channel round-trip back to the
/// connection's async task — see `acp.rs`'s own doc on why that shape is
/// required from a `spawn_blocking` thread); `None` (no handler at all)
/// still fails CLOSED rather than silently ungated — a gated step with no
/// handler wired refuses itself via `crew::gate::resolve_gate`'s own
/// `None` fallback.
pub fn run_ephemeral(
    config: &MissionConfig,
    args: &str,
    cwd: &Path,
    gate: Option<&mut crate::crew::gate::GateHandler<'_>>,
) -> Result<String> {
    let mut config = config.clone();
    inject_panel_args_task_if_referenced(&mut config, args);

    // (#1684 QA finding — CONSIDER 7) `mission launch` runs
    // `MissionConfig::validate` at its consumption point before ever
    // calling `interpret` (contract 7: semantic validation is a separate,
    // explicit pass); the ephemeral path is a consumption point too and
    // gets the SAME gate — a zero-step task, for instance, would otherwise
    // either wedge (never reaches `Complete`, `task_status` never
    // resolves) or surface as an unhelpful "terminal task has no steps"
    // deep inside `render_ephemeral_result`, instead of a clear, named
    // validate()-time error. `known_kinds` is Tier 1 builtins only —
    // sufficient because ephemeral is procedural-only by construction
    // (the routing decision in `is_procedural_only` already excludes
    // anything else), and the reserved `PANEL_ARGS_TASK_ID` reads/depends_on
    // carve-out in `validate` means the just-injected args task never
    // trips a false dangling-reference finding here.
    let known_ids = StepKindRegistry::with_builtins().ids();
    let known_kinds: Vec<&str> = known_ids.iter().map(String::as_str).collect();
    let errors: Vec<_> = config
        .validate(&known_kinds)
        .into_iter()
        .filter(|f| f.severity == mission_config::FindingSeverity::Error)
        .collect();
    if !errors.is_empty() {
        let msg = errors.iter().map(|f| f.to_string()).collect::<Vec<_>>().join("\n");
        anyhow::bail!("panel command config \"{}\" failed validation:\n{msg}", config.id);
    }

    let params = LaunchParams::default();
    let (ordered_tasks, mut steps, interpret_warnings) =
        mission_config::interpret(&config, &params).context("interpreting panel command graph")?;

    apply_default_cwd(&mut steps, cwd);

    let tasks: BTreeMap<String, Task> = ordered_tasks.iter().map(|t| (t.id.clone(), t.clone())).collect();
    let registry = StepKindRegistry::with_builtins();
    let facts = Facts::default();
    let est = FixedEstimator::default();

    // (#1684 QA finding — MUST-FIX 4) A per-INVOCATION correlation id,
    // backfilled onto every emitted flow record exactly the way
    // `mission_launch.rs`'s `run_step_graph` call sites backfill their own
    // minted `mission_id` (`record.mission_id.get_or_insert_with(...)`,
    // never overwriting a record that already carries one — see
    // `step_lifecycle_record`'s own doc in `darkmux-crew`'s scheduler for
    // why the bare records carry NO mission_id and rely on the caller to
    // backfill it). Without this, every ephemeral run of the SAME config
    // shares one CONFIG-scoped `session_id` (`session_id::task` hashes
    // only `step.task_id`) and no `mission_id` at all — two concurrent
    // invocations collide in the viewer with nothing to tell them apart.
    // This id is a FLOW-RECORD correlation label only — no mission
    // instance is minted for it (rule D still holds: nothing under
    // `<mission_id>/` is ever written).
    let correlation_id = mint_ephemeral_correlation_id(&config.id);

    let report = crate::crew::scheduler::run_step_graph(
        &mut steps,
        &tasks,
        &registry,
        &facts,
        &est,
        1,
        &crate::crew::concurrent_dispatch::lms_host_factory,
        &mut |mut record| {
            record.mission_id.get_or_insert_with(|| correlation_id.clone());
            let _ = crate::flow::record(record);
        },
        &mut |_step: &Step| {
            // Deliberately a no-op — ephemeral runs mint no mission
            // instance, so there is no `<mission>/<phase>/steps/<id>.json`
            // to persist a Step transition into (rule D).
        },
        gate,
        None,
        &[],
    )
    .context("running panel command graph")?;

    render_ephemeral_result(&ordered_tasks, &steps, &report, &interpret_warnings)
}

/// Mint a per-invocation flow-record correlation id for an ephemeral
/// panel-command run — see `run_ephemeral`'s own doc for why this exists.
/// Shape mirrors `mission_launch::mint_run_id`'s spirit
/// (`<config-id>-<unix-nanos>-<counter>`) without pulling in `blake3` here
/// (`acp_panel.rs` has no other cryptographic need); a nanosecond
/// timestamp plus an in-process atomic counter is already collision-free
/// for concurrent ACP prompts within one process.
fn mint_ephemeral_correlation_id(config_id: &str) -> String {
    static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    format!("acp-ephemeral-{config_id}-{nanos}-{n}")
}

/// If any task in `config` names [`PANEL_ARGS_TASK_ID`] (`"__panel_args__"`)
/// in its own `reads` or `depends_on`, prepend a new phase carrying exactly
/// one `procedural.noop` task under that id, whose step's `config.output`
/// is `args` verbatim (empty string when the command was invoked with no
/// arguments — the launch path's `--param` OMITS the flag entirely for
/// that case, but an ephemeral graph that declares `reads:
/// ["__panel_args__"]` must still resolve cleanly through `interpret`, so
/// this seeds an empty value rather than skipping injection). A config
/// that never references the reserved id is untouched.
///
/// **Reservation collision (#1695 merge-gate finding 1).** If the
/// operator's OWN document already declares a task literally named
/// `__panel_args__`, injection is SKIPPED entirely rather than prepending
/// a second task under the same id — `interpret`'s own duplicate-id check
/// (`push_task`) would otherwise bail with a bare "duplicate task id"
/// error that never explains WHY that particular id is special. Skipping
/// is also the semantically correct choice, not just the safe one:
/// `interpret` already resolves `reads`/`depends_on` entries against the
/// document's OWN declared tasks first, so a task reading
/// `__panel_args__` in a document that declares one under that name
/// already receives THAT task's real output — operator sovereignty: an
/// explicit declaration under a reserved name wins over the synthetic
/// default for that name, silently and correctly, with nothing to inject.
fn inject_panel_args_task_if_referenced(config: &mut MissionConfig, args: &str) {
    let referenced = config
        .phases
        .iter()
        .flat_map(|p| p.tasks.iter())
        .any(|t| t.reads.iter().chain(t.depends_on.iter()).any(|r| r == PANEL_ARGS_TASK_ID));
    if !referenced {
        return;
    }
    let already_declared =
        config.phases.iter().flat_map(|p| p.tasks.iter()).any(|t| t.id == PANEL_ARGS_TASK_ID);
    if already_declared {
        return;
    }
    let args_task = TaskConfig {
        id: PANEL_ARGS_TASK_ID.to_string(),
        description: Some("synthetic: the raw text typed after the panel command name".to_string()),
        display_name: None,
        depends_on: Vec::new(),
        reads: Vec::new(),
        role_id: None,
        steps: vec![StepConfig {
            id: format!("{PANEL_ARGS_TASK_ID}-step"),
            kind: "procedural.noop".to_string(),
            config: serde_json::json!({"output": args}),
            gate: None,
            extras: BTreeMap::new(),
        }],
        extras: BTreeMap::new(),
    };
    config.phases.insert(
        0,
        PhaseConfig {
            id: "__panel_args_phase__".to_string(),
            description: None,
            display_name: None,
            tasks: vec![args_task],
            extras: BTreeMap::new(),
        },
    );
}

/// Fill `cwd` into every `procedural.shell` step's config that doesn't
/// already declare one — never overrides a step-authored `cwd`.
fn apply_default_cwd(steps: &mut BTreeMap<String, Step>, cwd: &Path) {
    let cwd_str = cwd.to_string_lossy().to_string();
    for step in steps.values_mut() {
        if step.kind != "procedural.shell" {
            continue;
        }
        if step.config.get("cwd").and_then(|v| v.as_str()).is_some() {
            continue;
        }
        match &mut step.config {
            serde_json::Value::Object(map) => {
                map.insert("cwd".to_string(), serde_json::Value::String(cwd_str.clone()));
            }
            serde_json::Value::Null => {
                let mut map = serde_json::Map::new();
                map.insert("cwd".to_string(), serde_json::Value::String(cwd_str.clone()));
                step.config = serde_json::Value::Object(map);
            }
            _ => {}
        }
    }
}

/// The two literal prefixes [`render_ephemeral_result`] returns its
/// business-logic FAILURE messages under — as `Ok(String)`, since
/// `run_ephemeral`'s own contract has no separate structured pass/fail
/// signal and the ACP panel surface just displays whichever string comes
/// back. `pub` (#1698 Packet A) so `src/radio_cli.rs`'s CLI verb, which DOES
/// have a real exit code to decide, can recognize the same two shapes
/// without re-deriving or copy-pasting the literals — one place names what
/// "the ephemeral run failed" looks like as text.
pub const EPHEMERAL_FAILURE_PREFIX: &str = "darkmux: command failed";
pub const EPHEMERAL_INCOMPLETE_PREFIX: &str = "darkmux: command did not reach";

/// `true` iff `output` (an `Ok(String)` from [`run_ephemeral`]) is one of
/// [`render_ephemeral_result`]'s two business-logic FAILURE renderings,
/// rather than a genuine success. See those constants' own doc for why this
/// exists — the small, documented coupling a caller with a real exit code
/// (`src/radio_cli.rs`) needs, without a second copy of the literals.
pub fn ephemeral_output_is_failure(output: &str) -> bool {
    output.starts_with(EPHEMERAL_FAILURE_PREFIX) || output.starts_with(EPHEMERAL_INCOMPLETE_PREFIX)
}

/// The sink task in DOCUMENT order — a task no other task's `depends_on`
/// OR `reads` names — and its last step's output, rendered as the final
/// panel message. `ordered_tasks` MUST be the original `Vec<Task>`
/// `interpret` returned (document order); a `BTreeMap`'s key order is
/// lexicographic by id, not document order, so it cannot substitute here.
///
/// `interpret_warnings` (#1695 merge-gate finding 3) are `interpret`'s own
/// non-fatal findings (e.g. an absent `expand.over` collection under a
/// pre-2.0 document — see `InterpretedGraph`'s doc) — `run_ephemeral`
/// previously bound and dropped them; every other production caller
/// (`mission_launch.rs`) prints them, so silently discarding them here was
/// the one place in the codebase where they went nowhere.
///
/// **A SIDE branch can fail while the terminal step still reports a
/// success-shaped string** (see the CONSIDER-6 comment further down): a
/// caller with a real exit code to decide (`src/radio_cli.rs`) sees the
/// warning appended to the OUTPUT text, not a distinct prefix
/// [`ephemeral_output_is_failure`] can key on — named here since that
/// caller can't tell partial-failure from clean success by string alone.
fn render_ephemeral_result(
    ordered_tasks: &[Task],
    steps: &BTreeMap<String, Step>,
    report: &SchedulerReport,
    interpret_warnings: &[String],
) -> Result<String> {
    let referenced: BTreeSet<&str> = ordered_tasks
        .iter()
        .flat_map(|t| t.depends_on.iter().chain(t.reads.iter()))
        .map(|s| s.as_str())
        .collect();
    // (#1684 QA finding — CONSIDER 6) Every SINK task (no other task's
    // `depends_on`/`reads` names it) in document order, not just the last
    // one — a fan-out graph with more than one sink loses every branch but
    // the last silently otherwise. The LAST sink in document order is
    // still what becomes the "final message" (rule E is singular by
    // design), but the others are named in a warning rather than dropped
    // with no trace.
    let sinks: Vec<&Task> = ordered_tasks.iter().filter(|t| !referenced.contains(t.id.as_str())).collect();
    let sink = sinks
        .last()
        .ok_or_else(|| anyhow::anyhow!("panel command graph has no terminal (sink) task"))?;
    let last_step_id = sink
        .step_ids
        .last()
        .ok_or_else(|| anyhow::anyhow!("panel command graph's terminal task `{}` has no steps", sink.id))?;
    let terminal = steps
        .get(last_step_id)
        .ok_or_else(|| anyhow::anyhow!("panel command graph's terminal step `{last_step_id}` vanished"))?;

    let output = terminal.output.clone().unwrap_or_default();
    if terminal.status == NodeStatus::Error {
        return Ok(format!("{EPHEMERAL_FAILURE_PREFIX}:\n\n{output}"));
    }
    if terminal.status != NodeStatus::Complete {
        // The terminal task never completed — its dependency chain
        // stranded on an earlier error (`step_is_ready` never schedules a
        // task whose dependency didn't reach Complete). Name what errored
        // rather than returning an empty/misleading message.
        return Ok(format!(
            "{EPHEMERAL_INCOMPLETE_PREFIX} its final step (status: {:?}) — step(s) errored: {}",
            terminal.status,
            if report.errored.is_empty() { "(none recorded)".to_string() } else { report.errored.join(", ") }
        ));
    }

    // (#1684 QA finding — CONSIDER 6) The terminal step can `Complete`
    // cleanly while a SIDE branch (a non-terminal sink, or any other step
    // off the terminal's own dependency chain) errored — `report.errored`
    // is scheduler-wide, not scoped to the chain that fed the terminal
    // step. A clean-looking final message over a partially-failed run is
    // exactly the "silence reads as success" failure this project's own
    // doctrine (CLAUDE.md's "no blind runs") warns against; name it.
    let mut warnings: Vec<String> = interpret_warnings.to_vec();
    if !report.errored.is_empty() {
        warnings.push(format!("step(s) errored elsewhere in the graph: {}", report.errored.join(", ")));
    }
    if sinks.len() > 1 {
        let other_ids: Vec<&str> = sinks[..sinks.len() - 1].iter().map(|t| t.id.as_str()).collect();
        warnings.push(format!(
            "graph has {} terminal branches; only `{}`'s output is shown (also ran: {})",
            sinks.len(),
            sink.id,
            other_ids.join(", ")
        ));
    }
    if warnings.is_empty() {
        return Ok(output);
    }
    Ok(format!("{output}\n\n---\n⚠ {}", warnings.join("\n⚠ ")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crew::mission_config::{PanelConfig, PhaseConfig, StepConfig, TaskConfig};
    use std::collections::BTreeMap as Map;

    fn step(id: &str, kind: &str, config: serde_json::Value) -> StepConfig {
        StepConfig { id: id.to_string(), kind: kind.to_string(), config, gate: None, extras: Map::new() }
    }

    fn gated_step(id: &str, kind: &str, config: serde_json::Value, gate: &str) -> StepConfig {
        StepConfig { gate: Some(gate.to_string()), ..step(id, kind, config) }
    }

    fn task(id: &str, depends_on: &[&str], reads: &[&str], steps: Vec<StepConfig>) -> TaskConfig {
        TaskConfig {
            id: id.to_string(),
            description: None,
            display_name: None,
            depends_on: depends_on.iter().map(|s| s.to_string()).collect(),
            reads: reads.iter().map(|s| s.to_string()).collect(),
            role_id: None,
            steps,
            extras: Map::new(),
        }
    }

    fn phase(id: &str, tasks: Vec<TaskConfig>) -> PhaseConfig {
        PhaseConfig { id: id.to_string(), description: None, display_name: None, tasks, extras: Map::new() }
    }

    fn config(id: &str, panel: Option<PanelConfig>, phases: Vec<PhaseConfig>) -> MissionConfig {
        MissionConfig {
            id: id.to_string(),
            name: id.to_string(),
            description: None,
            schema_version: None,
            inputs: Vec::new(),
            phases,
            panel,
            extras: Map::new(),
        }
    }

    // ── parse_command ────────────────────────────────────────────────

    #[test]
    fn parse_command_strips_slash_and_splits_name_from_args() {
        assert_eq!(parse_command("/pr-view 42"), Some(("pr-view".to_string(), "42".to_string())));
        assert_eq!(parse_command("pr-view 42"), Some(("pr-view".to_string(), "42".to_string())));
        assert_eq!(parse_command("/review"), Some(("review".to_string(), String::new())));
        assert_eq!(parse_command("   "), None);
        assert_eq!(parse_command(""), None);
    }

    // ── is_procedural_only (rule D routing test) ────────────────────────

    #[test]
    fn a_config_with_only_procedural_steps_is_ephemeral() {
        let cfg = config(
            "echo-test",
            None,
            vec![phase(
                "p1",
                vec![task("t1", &[], &[], vec![step("s1", "procedural.shell", serde_json::json!({"command": "echo hi"}))])],
            )],
        );
        assert!(is_procedural_only(&cfg));
    }

    #[test]
    fn a_config_with_a_dispatch_step_is_not_ephemeral() {
        // (Required test) A `dispatch.*` step must route AWAY from the
        // ephemeral path — this only asserts the ROUTING decision, never
        // actually dispatches a model.
        let cfg = config(
            "coder-verb",
            None,
            vec![phase(
                "p1",
                vec![
                    task("t1", &[], &[], vec![step("s1", "procedural.shell", serde_json::json!({"command": "echo hi"}))]),
                    task("t2", &["t1"], &[], vec![step("s2", "dispatch.internal", serde_json::Value::Null)]),
                ],
            )],
        );
        assert!(!is_procedural_only(&cfg), "a graph with any dispatch.* step must not be ephemeral");
    }

    #[test]
    fn a_config_with_zero_steps_is_not_ephemeral() {
        // A freeform config (every phase manual) — nothing for an
        // in-process runner to execute; `mission launch` handles this path.
        let cfg = config("freeform", None, vec![phase("p1", vec![])]);
        assert!(!is_procedural_only(&cfg));
    }

    // ── route_command ────────────────────────────────────────────────

    #[test]
    fn route_command_returns_none_for_an_unadvertised_command() {
        let advertised = vec![PanelCommand { id: "review".to_string(), description: "d".to_string(), hint: None }];
        assert!(route_command(&advertised, "not-advertised").is_none());
    }

    #[test]
    fn review_always_routes_to_the_review_variant() {
        let advertised = vec![PanelCommand { id: "review".to_string(), description: "d".to_string(), hint: None }];
        let plan = route_command(&advertised, "review").expect("review must route");
        let RoutePlan::Review(id) = plan else { panic!("expected RoutePlan::Review") };
        assert_eq!(id, "review");
    }

    /// (#1695 merge-gate MUST FIX) A panel-advertised review VARIANT — an
    /// operator config under a DIFFERENT id, carrying real `review.*` step
    /// kinds — must route to `RoutePlan::Review` carrying ITS OWN id, not
    /// the built-in `"review"`. Pre-fix, `run_review` hardcoded `mission
    /// launch review` regardless of which id actually routed here, so a
    /// variant would advertise and invoke fine while silently launching
    /// the wrong config underneath.
    #[test]
    #[serial_test::serial]
    fn review_variant_routes_to_review_carrying_its_own_id_not_the_builtin() {
        let tmp = tempfile::TempDir::new().unwrap();
        let prev = std::env::var("DARKMUX_CREW_DIR").ok();
        // SAFETY: this test is #[serial_test::serial].
        unsafe { std::env::set_var("DARKMUX_CREW_DIR", tmp.path()) };

        let dir = tmp.path().join("mission-configs");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("review-lean.json"),
            serde_json::to_string(&serde_json::json!({
                "id": "review-lean",
                "name": "Review Lean",
                "panel": {"description": "A leaner review"},
                "phases": [{
                    "id": "adjudicate",
                    "tasks": [{"id": "t1", "steps": [{"id": "s1", "kind": "review.judge"}]}]
                }]
            }))
            .unwrap(),
        )
        .unwrap();

        let advertised =
            vec![PanelCommand { id: "review-lean".to_string(), description: "A leaner review".to_string(), hint: None }];
        let plan = route_command(&advertised, "review-lean").expect("review-lean must route");
        let RoutePlan::Review(launch_id) = plan else { panic!("expected RoutePlan::Review for a config carrying review.* kinds") };
        assert_eq!(
            launch_id, "review-lean",
            "the routed id must be the VARIANT's own registry key — this is what \
             `run_review` spawns as `mission launch <launch_id>`, so a wrong id here \
             means the wrong config launches"
        );

        // SAFETY: this test is #[serial_test::serial].
        unsafe {
            match prev {
                Some(v) => std::env::set_var("DARKMUX_CREW_DIR", v),
                None => std::env::remove_var("DARKMUX_CREW_DIR"),
            }
        }
    }

    /// (#1695 merge-gate finding 2) A mixed-case on-disk config filename
    /// advertises under its own (correctly-cased) id, but a user typing
    /// the command lowercases it (`parse_command`'s own normalization) —
    /// `route_command` must still resolve it, and must resolve/launch
    /// using the ORIGINAL-CASED registry key, never the lowercased text
    /// the user typed (a case-sensitive filesystem would 404 on that).
    #[test]
    #[serial_test::serial]
    fn route_command_matches_case_insensitively_and_launches_the_correctly_cased_id() {
        let tmp = tempfile::TempDir::new().unwrap();
        let prev = std::env::var("DARKMUX_CREW_DIR").ok();
        // SAFETY: this test is #[serial_test::serial].
        unsafe { std::env::set_var("DARKMUX_CREW_DIR", tmp.path()) };

        let dir = tmp.path().join("mission-configs");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("Pr-View.json"),
            serde_json::to_string(&serde_json::json!({
                "id": "Pr-View",
                "name": "PR View",
                "panel": {"description": "View a PR"},
                "phases": [{
                    "id": "p1",
                    "tasks": [{"id": "t1", "steps": [{"id": "s1", "kind": "procedural.shell", "config": {"command": "echo hi"}}]}]
                }]
            }))
            .unwrap(),
        )
        .unwrap();

        // The advertised entry keeps the file's own case; the user typed
        // "/Pr-View", which `parse_command` lowercases to "pr-view" before
        // it ever reaches `route_command`.
        let advertised =
            vec![PanelCommand { id: "Pr-View".to_string(), description: "View a PR".to_string(), hint: None }];
        let plan = route_command(&advertised, "pr-view").expect("a mixed-case filename must still be invocable");
        match plan {
            RoutePlan::Ephemeral(config) => {
                assert_eq!(config.id, "Pr-View", "the loaded config is the correctly-cased file's own document");
            }
            _ => panic!("expected an Ephemeral route for a procedural-only fixture"),
        }

        // SAFETY: this test is #[serial_test::serial].
        unsafe {
            match prev {
                Some(v) => std::env::set_var("DARKMUX_CREW_DIR", v),
                None => std::env::remove_var("DARKMUX_CREW_DIR"),
            }
        }
    }

    // ── (#1684 QA finding — MUST-FIX 2/3) advertised id + description ──

    #[test]
    #[serial_test::serial]
    fn list_panel_commands_advertises_the_resolvable_filename_not_the_document_body_id() {
        let tmp = tempfile::TempDir::new().unwrap();
        let prev = std::env::var("DARKMUX_CREW_DIR").ok();
        // SAFETY: this test is #[serial_test::serial].
        unsafe { std::env::set_var("DARKMUX_CREW_DIR", tmp.path()) };

        let dir = tmp.path().join("mission-configs");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("pr-view.json"),
            serde_json::to_string(&serde_json::json!({
                // Deliberately DIFFERENT from the filename stem — the
                // exact drift #1684 QA finding 2 caught: a config file
                // whose on-disk name and JSON body `id` don't match.
                "id": "pr_view",
                "name": "PR View — a very long developer-facing name nobody wants in a menu",
                "description": "a 2000-character developer provenance essay stands in here in the real bug",
                "panel": {"description": "View a PR"},
                "phases": []
            }))
            .unwrap(),
        )
        .unwrap();

        let commands = list_panel_commands();
        let found = commands
            .iter()
            .find(|c| c.description == "View a PR")
            .expect("the injected config must be advertised (short panel.description, not the long one)");
        assert_eq!(
            found.id, "pr-view",
            "must advertise the RESOLVABLE filename stem, not the document body's mismatched `id` field"
        );
        // And what got advertised must actually be loadable under that id
        // — the whole point of fixing MUST-FIX 2.
        assert!(mission_config::load(&found.id).is_ok(), "the advertised id must resolve via mission_config::load");

        // SAFETY: this test is #[serial_test::serial].
        unsafe {
            match prev {
                Some(v) => std::env::set_var("DARKMUX_CREW_DIR", v),
                None => std::env::remove_var("DARKMUX_CREW_DIR"),
            }
        }
    }

    // ── not_a_command_message ────────────────────────────────────────

    #[test]
    fn not_a_command_message_lists_the_advertised_commands() {
        let advertised = vec![
            PanelCommand { id: "review".to_string(), description: "d".to_string(), hint: None },
            PanelCommand { id: "pr-list".to_string(), description: "d2".to_string(), hint: None },
        ];
        let msg = not_a_command_message(&advertised);
        assert!(msg.contains("/review"), "{msg}");
        assert!(msg.contains("/pr-list"), "{msg}");
    }

    #[test]
    fn not_a_command_message_handles_an_empty_advertised_list() {
        let msg = not_a_command_message(&[]);
        assert!(!msg.is_empty());
        assert!(!msg.contains("/review"));
    }

    // ── run_ephemeral (required test: two-step procedural.shell chain) ──

    #[test]
    fn ephemeral_run_chains_two_procedural_shell_steps_via_step_input_env_var() {
        let cfg = config(
            "echo-chain",
            None,
            vec![phase(
                "p1",
                vec![
                    task(
                        "producer",
                        &[],
                        &[],
                        vec![step("producer-step", "procedural.shell", serde_json::json!({"command": "echo hello-from-producer"}))],
                    ),
                    task(
                        "consumer",
                        &["producer"],
                        &[],
                        vec![step(
                            "consumer-step",
                            "procedural.shell",
                            serde_json::json!({"command": "echo got: $DARKMUX_STEP_INPUT_PRODUCER"}),
                        )],
                    ),
                ],
            )],
        );
        let tmp = std::env::temp_dir();
        let out = run_ephemeral(&cfg, "", &tmp, None).expect("ephemeral run succeeds");
        assert_eq!(out.trim(), "got: hello-from-producer");
    }

    #[test]
    #[serial_test::serial]
    fn ephemeral_run_never_mints_a_mission_instance_directory() {
        // (Required test) Assert NO mission instance directory was
        // created — isolate DARKMUX_CREW_DIR so this test can inspect the
        // (empty) missions dir without racing any other test's real state.
        let tmp = tempfile::TempDir::new().unwrap();
        let prev = std::env::var("DARKMUX_CREW_DIR").ok();
        // SAFETY: this test is #[serial_test::serial].
        unsafe { std::env::set_var("DARKMUX_CREW_DIR", tmp.path()) };

        let cfg = config(
            "noop-test",
            None,
            vec![phase("p1", vec![task("t1", &[], &[], vec![step("s1", "procedural.noop", serde_json::Value::Null)])])],
        );
        let cwd = std::env::temp_dir();
        let out = run_ephemeral(&cfg, "", &cwd, None).expect("ephemeral run succeeds");
        // `procedural.noop` with no `output` override defaults to its own
        // step id (see `ProceduralNoopStepKind::run`) — proves the run
        // actually executed, not just that no directory appeared.
        assert_eq!(out, "s1");

        let missions_dir = crate::crew::loader::missions_dir();
        let entries: Vec<_> = std::fs::read_dir(&missions_dir).map(|d| d.collect()).unwrap_or_default();
        assert!(entries.is_empty(), "ephemeral run must mint zero mission instance directories, found {entries:?}");

        // SAFETY: this test is #[serial_test::serial].
        unsafe {
            match prev {
                Some(v) => std::env::set_var("DARKMUX_CREW_DIR", v),
                None => std::env::remove_var("DARKMUX_CREW_DIR"),
            }
        }
    }

    #[test]
    fn ephemeral_run_seeds_args_when_a_task_reads_the_synthetic_args_task() {
        // `procedural.shell`'s `sanitize_env_key` uppercases alnum bytes
        // and maps everything else (including `_`) to `_`, so the reserved
        // id `__panel_args__` becomes the env var name below verbatim
        // (case-folded).
        let cfg = config(
            "args-echo",
            None,
            vec![phase(
                "p1",
                vec![task(
                    "t1",
                    &[],
                    &[PANEL_ARGS_TASK_ID],
                    vec![step(
                        "s1",
                        "procedural.shell",
                        serde_json::json!({"command": "echo arg: $DARKMUX_STEP_INPUT___PANEL_ARGS__"}),
                    )],
                )],
            )],
        );
        let tmp = std::env::temp_dir();
        let out = run_ephemeral(&cfg, "hello world", &tmp, None).expect("ephemeral run succeeds");
        assert_eq!(out.trim(), "arg: hello world");
    }

    #[test]
    fn ephemeral_run_with_empty_args_still_resolves_a_task_that_reads_the_reserved_id() {
        // (#1684 QA context) The reserved id is injected with an EMPTY
        // string when the command was invoked with no arguments — the
        // config must still interpret/run cleanly, not dangle.
        let cfg = config(
            "args-echo-empty",
            None,
            vec![phase(
                "p1",
                vec![task(
                    "t1",
                    &[],
                    &[PANEL_ARGS_TASK_ID],
                    vec![step(
                        "s1",
                        "procedural.shell",
                        serde_json::json!({"command": "echo arg:[$DARKMUX_STEP_INPUT___PANEL_ARGS__]"}),
                    )],
                )],
            )],
        );
        let tmp = std::env::temp_dir();
        let out = run_ephemeral(&cfg, "", &tmp, None).expect("ephemeral run succeeds");
        assert_eq!(out.trim(), "arg:[]");
    }

    // ── (#1695 merge-gate finding 1) reserved-id collision ──────────────

    #[test]
    fn ephemeral_run_skips_injection_when_the_document_already_declares_the_reserved_task_id() {
        // The document itself owns a task literally named "__panel_args__"
        // — injection must be skipped (never double-inject, which would
        // collide at interpret()'s own duplicate-id check), and the
        // reading task must receive the DOCUMENT's own task's real
        // output, never the synthetic args string.
        let cfg = config(
            "reserved-collision",
            None,
            vec![phase(
                "p1",
                vec![
                    task(
                        PANEL_ARGS_TASK_ID,
                        &[],
                        &[],
                        vec![step(
                            "producer-step",
                            "procedural.noop",
                            serde_json::json!({"output": "operator-owned-value"}),
                        )],
                    ),
                    task(
                        "consumer",
                        &[],
                        &[PANEL_ARGS_TASK_ID],
                        vec![step(
                            "consumer-step",
                            "procedural.shell",
                            serde_json::json!({"command": "echo got: $DARKMUX_STEP_INPUT___PANEL_ARGS__"}),
                        )],
                    ),
                ],
            )],
        );
        let tmp = std::env::temp_dir();
        // A NON-EMPTY args string — if injection had run anyway (ignoring
        // the collision), the reading task would see THIS value instead
        // of the document's own task output.
        let out = run_ephemeral(&cfg, "this-should-be-ignored", &tmp, None).expect("ephemeral run succeeds, no duplicate-id bail");
        assert_eq!(out.trim(), "got: operator-owned-value");
    }

    // ── (#1684 QA finding — CONSIDER 7) validate() runs before interpret ──

    #[test]
    fn ephemeral_run_rejects_a_zero_step_task_at_validate_time_not_a_confusing_runtime_error() {
        let cfg = config(
            "hollow",
            None,
            vec![phase(
                "p1",
                vec![
                    task("t1", &[], &[], vec![]), // zero steps — a real MissionConfig::validate() Error
                ],
            )],
        );
        let tmp = std::env::temp_dir();
        let err = run_ephemeral(&cfg, "", &tmp, None).expect_err("a zero-step task must fail validate(), not run");
        assert!(err.to_string().contains("failed validation"), "{err:#}");
    }

    // ── (#1695 merge-gate finding 3) interpret() warnings surfaced ──────

    #[test]
    fn render_ephemeral_result_appends_interpret_warnings_to_the_output() {
        // `run_ephemeral` used to bind and drop `interpret`'s own non-fatal
        // warnings; `render_ephemeral_result` must surface them in the
        // final message the same way it already surfaces scheduler-level
        // findings (errored steps, multi-sink branches) — a direct unit
        // test since `interpret()` itself has no live producer of a
        // non-empty warnings Vec today (see `InterpretedGraph`'s doc).
        let t = Task {
            id: "t1".to_string(),
            phase_id: "p1".to_string(),
            description: String::new(),
            display_name: None,
            step_ids: vec!["s1".to_string()],
            depends_on: Vec::new(),
            reads: Vec::new(),
            role_id: None,
            profile_name: None,
            workdir: None,
            image: None,
        };
        let mut steps = BTreeMap::new();
        steps.insert(
            "s1".to_string(),
            Step {
                id: "s1".to_string(),
                task_id: "t1".to_string(),
                gate: None,
                kind: "procedural.noop".to_string(),
                status: NodeStatus::Complete,
                config: serde_json::Value::Null,
                started_ts: None,
                completed_ts: None,
                output: Some("done".to_string()),
            },
        );
        let report = SchedulerReport::default();
        let interpret_warnings = vec!["absent expand.over collection for task foo".to_string()];

        let out = render_ephemeral_result(&[t], &steps, &report, &interpret_warnings).expect("renders");
        assert!(out.contains("done"), "{out}");
        assert!(out.contains("absent expand.over collection for task foo"), "{out}");
    }

    // ── (#1684 Packet 2) run_ephemeral + operator sign-off gate ────────

    /// A two-step procedural config — a `gather` task feeding a gated
    /// `executor` task — is the shape every documented gated panel verb
    /// (`pr-merge`, `pr-approve`) actually has: a gather step assembles the
    /// facts, the gated step is the consequential action. This test
    /// exercises BOTH decisions through the real `run_ephemeral` path (not
    /// just `gate::resolve_gate`'s own unit tests) and asserts the
    /// handler's facts map is literally the gather task's output — the
    /// dialog-body contract the #1685 spec depends on.
    fn gather_then_gated_config() -> MissionConfig {
        config(
            "gather-then-gated",
            None,
            vec![phase(
                "p1",
                vec![
                    task(
                        "gather",
                        &[],
                        &[],
                        vec![step(
                            "gather-step",
                            "procedural.shell",
                            serde_json::json!({"command": "echo 42 open PRs"}),
                        )],
                    ),
                    task(
                        "executor",
                        &["gather"],
                        &[],
                        vec![gated_step(
                            "executor-step",
                            "procedural.noop",
                            serde_json::json!({"output": "merged"}),
                            "operator",
                        )],
                    ),
                ],
            )],
        )
    }

    #[test]
    fn ephemeral_run_gate_handler_receives_the_gather_tasks_output_and_approving_runs_the_executor() {
        let cfg = gather_then_gated_config();
        let tmp = std::env::temp_dir();

        let mut received: Option<Map<String, String>> = None;
        let mut approve = |_s: &Step, f: &Map<String, String>| {
            received = Some(f.clone());
            crate::crew::gate::GateDecision::Approved
        };
        let out = run_ephemeral(&cfg, "", &tmp, Some(&mut approve)).expect("ephemeral run succeeds");
        assert_eq!(out.trim(), "merged", "an approved gate must let the executor step actually run");
        assert_eq!(
            received.as_ref().and_then(|f| f.get("gather")).map(|s| s.trim()),
            Some("42 open PRs"),
            "the gate handler must receive the gather task's output as its facts map — the \
             dialog-body contract: {received:?}"
        );
    }

    #[test]
    fn ephemeral_run_gate_handler_declining_fails_the_command_without_running_the_executor() {
        let cfg = gather_then_gated_config();
        let tmp = std::env::temp_dir();

        let mut decline = |_s: &Step, _f: &Map<String, String>| crate::crew::gate::GateDecision::Declined {
            reason: "operator declined".to_string(),
        };
        // `run_ephemeral` still returns `Ok` — a declined gate is a command
        // FAILURE (rendered as such, mirroring `render_ephemeral_result`'s
        // existing Error-terminal handling), never a hard `Err` propagated
        // across the ACP boundary.
        let out = run_ephemeral(&cfg, "", &tmp, Some(&mut decline))
            .expect("a declined gate still renders a command-failed message, not an Err");
        assert!(out.contains("darkmux: command failed"), "{out}");
        assert!(out.contains("operator declined"), "{out}");
    }
}
