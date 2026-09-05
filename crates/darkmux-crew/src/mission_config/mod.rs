//! Mission configs — missions as DATA (#1284 Packet 1).
//!
//! A mission config is a named JSON document declaring a mission's graph
//! SHAPE: phases (ordered), each phase's tasks, each task's steps, each
//! step naming a REGISTERED step-kind id (`step_kinds::StepKind::id()`)
//! plus a kind-specific `config` object. [`interpret`] (#1284 Packet 3)
//! turns a resolved config into the executable `Vec<Task>`/`BTreeMap<String,
//! Step>` shape `darkmux-crew`'s `scheduler::run_step_graph` consumes — the
//! SAME shape `build_review_graph` (`darkmux-lab`'s `lab::review`) and
//! `default_phase_graph` (the `darkmux` binary's `coder_phase.rs`) used to
//! build BY HAND, one Rust function per mission type, before Packet 3 cut
//! both over to load their config through this module and call
//! [`interpret`] instead. Packet 1 built the schema, the loader, the
//! built-in transcriptions of those two graphs, and the `darkmux doctor`
//! surface. Packet 3 added [`interpret`] itself (schema 1.0 → 1.1, additive
//! per contract 5) plus, at the time, a typed expansion primitive that
//! replaced review.json's original `expands_per_staffed_seat` placeholder
//! bool — retired in schema 2.0 (#1550 cluster item 2; a MAJOR bump — see
//! [`MISSION_CONFIG_SCHEMA`]'s doc) once #1512's review dissolution moved
//! the one real consumer to static per-role tasks.
//!
//! **Lenient-on-read (contract 7, `CLAUDE.md` "Cross-system contracts"):**
//! every struct here carries `#[serde(flatten)] extras` overflow, optional
//! fields stay `Option`, and an unrecognized field or a newer
//! `schema_version` never fails PARSING. Semantic validation is the
//! SEPARATE [`MissionConfig::validate`] pass — never invoked on the hot
//! load path ([`load`] never calls it).
//!
//! **Naming note.** This `MissionConfig` is a MISSION GRAPH document
//! (phases/tasks/steps) — a completely different concept from the OPERATOR
//! `config.json` `mission{}` block (drift-detection knobs like
//! `stale_active_days`), which was `darkmux_types::config::MissionConfig`
//! until the #1284 review round renamed it `MissionBoardConfig` precisely
//! so THIS type could own the bare name as the arc's headline concept
//! (Rust-only rename; the serde field stays `mission`, so operator
//! config.json files are untouched).

pub mod grow;
pub mod inputs;
pub mod interpret;
pub mod prune;
pub mod load;

pub use grow::{grow_task, items_from_artifact, GrownFrom};
pub use inputs::{
    check_embedded_inputs_collected, check_placeholders_declared, check_supplied_inert_inputs,
    find_unsubstituted_braces, substitute_step_config, undeclared_placeholders,
    unreferenced_inputs,
};
pub use interpret::{interpret, LaunchParams, TaskOverride};
pub use load::{has_non_user_fallback, list_ids, load, LoadedMissionConfig, MissionConfigSource};

use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

/// Current schema version for mission config documents. Plain semver
/// applied to the DATA SHAPE — mirrors `RULES_SCHEMA_VERSION`
/// (`darkmux-eureka`), `FLOW_SCHEMA_VERSION` (`darkmux-flow`),
/// `CONFIG_SCHEMA_VERSION` (`darkmux-types::config`). Started at "1.0"
/// (#1284 Packet 1); bumped to "1.1" in Packet 3 — additive: [`TaskConfig`]
/// gained the optional `expand` field (`ExpansionSpec`, since removed — see
/// below), replacing review.json's original `expands_per_staffed_seat`
/// prose-`notes` bool placeholder with a typed, interpretable primitive.
/// Bumped to "1.2" (#1398) — additive: [`PhaseConfig`] and [`TaskConfig`]
/// gained the optional `display_name` field (an operator-facing short
/// label, split from `description` which is deliberately long — see each
/// field's own doc), and `ExpansionSpec` gained the optional
/// `display_name_pattern` twin of its existing `description_pattern`.
/// Bumped to "1.3" (#1475 packet 2) — additive: `ExpansionSpec` gained the
/// optional `role_pattern` (a per-expanded-copy `role_id`, so the review
/// probe stage binds one distinct role per expanded task). Bumped to "1.4"
/// (#1619) — additive: [`TaskConfig`] gained the optional `reads` field (the
/// run-scoped output ledger made nameable).
///
/// Bumped to **"2.1"** (#1684) — additive: [`MissionConfig`] gained the
/// optional `panel` field ([`PanelConfig`]). Presence of the block is what
/// makes `darkmux acp` advertise this config as a slash command in the
/// editor's agent panel — absence means the config stays launch-only
/// (`darkmux mission launch <id>`/`darkmux mission propose`), never
/// panel-visible. `PanelConfig` itself carries `#[serde(flatten)] extras`
/// overflow (contract 7), so a future sub-field is safe to add without
/// another schema bump.
///
/// Bumped to **"2.0"** (#1550 cluster item 2) — a MAJOR bump, not minor:
/// `TaskConfig::expand`/`ExpansionSpec`/`interpret::LaunchParams::expansions`
/// were REMOVED (all three retired from this crate — no longer valid
/// doc-links). Per this constant's own bump discipline (below) — and
/// `CLAUDE.md`'s "Versioning" section, the same rule applied to a different
/// data shape — a field REMOVAL is breaking, full stop, regardless of
/// whether the removed field was ever load-bearing in production. (It
/// wasn't, as it happens: both production launchers always fed an empty
/// `expansions` map, so a document declaring `expand` interpreted to ZERO
/// real copies on every real run — retired per #1512's dissolution, once the
/// one real consumer, the review probe stage, moved to static per-role
/// tasks and stopped needing runtime expansion. That history explains WHY
/// the field was safe to remove; it does not downgrade the bump — a
/// removed field is major by the rule itself, not by how much a specific
/// removal happened to matter in practice.)
///
/// Lenient-on-read (contract 7) still holds at the wire level: a document
/// that still declares `expand` parses cleanly — the key overflows into
/// `extras`, inert — but [`MissionConfig::validate`] now flags it as a loud
/// `Error` (a REMOVED field silently losing its meaning is never safe to
/// stay quiet about, unlike an ADDITIVE field a future consumer can safely
/// ignore per the minor-bump contract).
///
/// Bumped to **"2.2"** (#1684 Packet 2) — additive: [`StepConfig`] gained
/// the optional `gate` field (recognized value today: `"operator"`) — the
/// operator sign-off gate. Presence blocks the step at run time until the
/// caller-supplied gate handler approves it (`darkmux_crew::gate`); absence
/// (every pre-2.2 document) is a pure no-op. A future consumer that doesn't
/// understand the field can safely ignore it per the minor-bump contract —
/// but a document that DOES declare an unrecognized gate VALUE is never
/// silently treated as ungated (see `StepConfig::gate`'s own doc on the
/// fail-closed contract).
///
/// Bumped to **"3.0"** (#2004) — a MAJOR bump: `gh_verb` was RENAMED to
/// [`MissionConfig::cmd`], and the config block it is checked against
/// (`darkmux_types::config::GhConfig`) to `CmdConfig` (`cmd.enabled` /
/// `cmd.allowed`). A rename is a removal plus an addition, and per this
/// constant's own discipline a removed field is major regardless of whether
/// anything depended on it — as it happens nothing did: no built-in and no
/// user document declared `gh_verb`, so this rename migrates zero data.
///
/// The old name was a lie about the mechanism. `GhConfig`'s own doc already
/// stated the design — "GitHub never enters darkmux core ... just a list of
/// operator-chosen VERB NAMES" — and the mechanism honors it: the gate does
/// nothing but compare a string a config declares against a list the
/// operator allowlisted. But naming it `gh_verb` meant a GitLab user
/// declared `"gh_verb": "mr-merge"`, and a config gating `terraform apply`
/// or `kubectl delete` — which want this gate just as much — had to declare
/// a GitHub-shaped field to get it. `cmd` is neutral across forges AND
/// across domains, which is what the mechanism always was.
///
/// A document still declaring `gh_verb` is a loud `Error` at validate time,
/// NOT a silent overflow into `extras`. That is not merely tidiness: the
/// gate fails OPEN by design (a config declaring no verb is never blocked,
/// so an ungated config stays ungated), so a stale `gh_verb` key would make
/// a config that used to be gated run UNGATED with no signal at all. Same
/// reasoning as the `expand` removal in 2.0, with a sharper edge.
///
/// Bumped to **"2.3"** (#1685) — additive: [`MissionConfig`] gained the
/// optional `cmd` field. Presence names the `gh`-verb allowlist entry
/// (`darkmux_types::config::CmdConfig`) this config requires before it may
/// run at ALL, on either entry point (`darkmux acp`'s ephemeral panel route
/// via `check_cmd`, or a direct `darkmux mission launch <id>`) — see
/// [`MissionConfig::cmd`]'s own doc. Absence (every pre-2.3 document,
/// and every config that isn't an operator-authored GitHub-CLI verb) is a
/// pure no-op.
///
/// Bumped to **"3.1"** (#2299) — additive: [`PhaseConfig`], [`TaskConfig`]
/// and [`StepConfig`] gained the optional `enabled` field (default `true`).
/// `false` prunes the item when a run is minted (`mission_config::prune`):
/// it never exists in the run, so the graph shows exactly what will execute
/// and nothing gray. The resolved-config snapshot every run keeps carries
/// the flags, and the run's `graph-report.json` names what was pruned and
/// why. There is deliberately NO CLI override — edit the JSON and run; the
/// snapshot is the record. A pre-3.1 reader ignores the field and mints
/// everything, which is the additive contract.
///
/// Bumped to **"3.2"** (#2300) — additive: [`TaskConfig`] gained the optional
/// `grow` field ([`GrowSpec`]). A task declaring it is a TEMPLATE, never
/// minted itself: at the boundary of the phase that owns it, the launcher
/// reads the `from` task's last step `output` as a PATH to a JSON file,
/// takes the array at `items`, and mints one copy of the template (all its
/// steps) per item. `{{item.<field>}}` in `id`/`config` renders from the
/// item's own top-level scalar fields.
///
/// This is NOT the schema-1.1 `expand`/`ExpansionSpec` primitive coming
/// back (removed in 2.0 above, and deliberately not resurrected). That one
/// was fed by a LAUNCH PARAM — a collection the launcher already held
/// before the run started — which is exactly why both production launchers
/// always handed it an empty map and nothing ever grew. `grow` is fed by a
/// STEP'S OUTPUT, produced by work the run itself did: the fan-out cannot
/// be known at launch time, because the plan it fans out over does not
/// exist yet. Different input, different lifetime, different mechanism.
///
/// A pre-3.2 reader ignores the field and mints nothing for that task,
/// which is the additive contract (the template is not executable on its
/// own in any reader).
///
/// Bumped to **"3.3"** (#2310 P3) — additive: [`MissionConfig`] gained the
/// optional `outcome_from` field — the document task id whose last step's
/// body the launcher promotes as the `mission close` record's payload,
/// overriding the positional "the last phase's last task" default (see
/// `src/mission_launch.rs::run_summary_payload`'s own doc for the full
/// promotion rule). Absence (every pre-3.3 document) keeps the positional
/// rule unchanged — a pre-3.3 reader ignores the field and gets exactly the
/// pre-existing behavior, the additive contract.
///
/// Bumped to **"3.4"** (#2310 P4/P4a) — additive: [`TaskConfig`] gained
/// the optional `run_on` field — which TERMINAL statuses of this task's
/// dependencies satisfy readiness (see `crate::types::Task::run_on`'s doc
/// and `scheduler::dependency_satisfies_run_on`). Absence (every pre-3.4
/// document) resolves to `crate::types::default_run_on()`
/// (`["complete"]`), the exact readiness rule every pre-3.4 document
/// already had — a pre-3.4 reader ignores the field and gets identical
/// behavior, the additive contract. (P4a) A task declaring `"error"` also
/// accepts a dependency that reached `Abandoned` — the scheduler's
/// `scheduler::cascade_abandon` rolls a task's TRANSITIVE dependents to
/// `Abandoned`, eagerly, the moment an ancestor errors, so a task several
/// hops downstream of the failure still sees a resolved terminal status
/// this same pass rather than staying wedged `Planned`. See DESIGN.md's
/// "Mission configs: a task's `run_on` decides which of its dependencies'
/// failures it survives" for the full cascade design.
///
/// Bumped to **"3.5"** (#2310 P4f) — additive: [`TaskConfig`] gained the
/// optional `excludes` field — document-wide task ids that must not be
/// `enabled` alongside this one, so a phase can ship TWO templates for one
/// slot with exactly one live. `review.json`'s `create-mods` phase is
/// the first user: the attended `create-mod` (wait for a frontier-written
/// mod) and the unattended `create-mod-dispatch` (a coder on a
/// hosted-endpoint profile, for the self-hosted runner where no
/// orchestrator session exists). Enforced only in
/// [`MissionConfig::validate`], never at run time. A pre-3.5 reader
/// overflows the field into `extras` and mints whatever `enabled` says —
/// and since every SHIPPED document keeps one of each excluded pair
/// disabled, an older binary's behavior on one is unchanged, the additive
/// contract.
///
/// Bump discipline (see `CLAUDE.md`'s "Versioning" — same rule, different
/// data shape): additive field/section → minor; rename/retype/removed
/// field/new-required-field → major.
pub const MISSION_CONFIG_SCHEMA: &str = "3.5";

/// One mission config document — the whole graph SHAPE, as data.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MissionConfig {
    pub id: String,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Schema version this document was authored against. `#[serde(default)]`
    /// so a document that omits it still parses — treated as compatible
    /// (no schema-version finding) by [`MissionConfig::validate`], since
    /// absence isn't drift, it's just an unlabeled document.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub schema_version: Option<String>,
    /// Runtime-only values the LAUNCHER (Packet 3+) must supply before this
    /// config can become an executable graph — a diff, a worktree path, a
    /// case id, resolved crew staffing. Declared here so the document is
    /// self-describing about what it needs from its caller, WITHOUT those
    /// genuinely per-launch values living inside the static document
    /// itself. See [`MissionInput`]'s doc.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub inputs: Vec<MissionInput>,
    /// Ordered — a phase's position in this list IS its ordering (mirrors
    /// `crew::types::Mission::phase_ids`'s #1341 strictly-linear-phase
    /// doctrine: no `depends_on` at the phase level, "the phase before this
    /// one" is purely positional).
    #[serde(default)]
    pub phases: Vec<PhaseConfig>,
    /// (#1684, schema 2.1) Presence of this block is what makes `darkmux
    /// acp` advertise this config as a slash command in the editor's agent
    /// panel — `None` (the default; every pre-2.1 document) keeps the
    /// config launch-only. See [`PanelConfig`]'s own doc for the field(s)
    /// it carries.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub panel: Option<PanelConfig>,
    /// (#1685, schema 2.3) The `gh`-verb allowlist entry this config
    /// requires — `None` (the default; every config that isn't an
    /// operator-authored GitHub-CLI verb) means no allowlist check applies
    /// at all, the ordinary case. `Some(verb)` means darkmux refuses to run
    /// ANY step in this config's graph unless
    /// `darkmux_types::config_access::cmd_allowed(verb)` returns true
    /// (`config.cmd.enabled == true` AND `verb` is named in
    /// `config.cmd.allowed`) — checked ONCE, before `validate`/`interpret`
    /// ever runs, by [`check_cmd`]. All three call sites that can
    /// execute a config's graph call it: `darkmux acp`'s ephemeral panel
    /// route (`run_ephemeral` in the `darkmux` binary crate), a direct
    /// `darkmux mission launch <id>` (same crate, `mission_launch::launch`),
    /// and `darkmux-lab`'s `review_bench::resolve_funnel_ctx` (the
    /// `--funnel` path, which resolves the SAME user-tier-overridable
    /// `review.json` and runs it through a `StepKindRegistry::
    /// with_builtins()`-backed graph) — so the gate holds regardless of
    /// which surface invoked the config. Named independently of the
    /// document's own `id` (conventionally the same string, e.g. a
    /// `pr-merge.json` config declaring `"cmd": "pr-merge"`, but not
    /// required to match) so the registry-key concern and the allowlist-name
    /// concern don't silently couple.
    ///
    /// darkmux core holds no opinion about what a "verb" IS — this field
    /// and `CmdConfig.allowed` are both bare strings the OPERATOR chooses;
    /// GitHub itself never enters this crate.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cmd: Option<String>,
    /// (#2310 P3, schema 3.3) The document task id whose last step's body
    /// becomes the `mission close` record's payload — an explicit override
    /// of the positional "the last phase's last task" default
    /// (`src/mission_launch.rs::run_summary_payload`). `Some(id)` names a
    /// task that MUST exist somewhere in [`Self::phases`]; a config
    /// declaring an id that resolves to no real task (a typo, or a
    /// `grow`-templated task, which is never itself minted) is a loud error
    /// at launch time, not a silent fall-through to the positional rule —
    /// the whole point of naming a task explicitly is that a config author
    /// gets a config-authoring MISTAKE surfaced immediately, not a close
    /// payload silently drawn from the wrong step. `None` (every pre-3.3
    /// document, and any config with no reason to override the positional
    /// default) keeps that default unchanged — `review.json` is the first
    /// consumer, naming `review-synthesis-task` so the review mission's
    /// close payload is the final envelope, not the report step's rendered
    /// GitHub comment.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub outcome_from: Option<String>,
    /// Forward-compat overflow — unknown top-level keys land here and
    /// re-serialize flat (a newer document read by an older binary).
    #[serde(flatten)]
    pub extras: BTreeMap<String, serde_json::Value>,
}

/// (#1685) Check `config`'s [`MissionConfig::cmd`] (if any) against the
/// operator's `gh`-verb allowlist. `None` = no verb declared, always
/// allowed (the ordinary case — most configs never touch this). `Some(reason)`
/// = blocked; the caller MUST refuse to run any step in the graph rather
/// than attempting it — never a partial run. All three call sites that can
/// execute a config's graph call this ONCE, up front, before `validate`/
/// `interpret` ever runs: `darkmux acp`'s ephemeral panel route, a direct
/// `darkmux mission launch <id>`, and `darkmux-lab`'s `review_bench`
/// `--funnel` path (#1685 QA CONSIDER 3 — the config it loads is
/// user-tier-overridable and its graph is `procedural.shell`-capable, same
/// as the other two).
pub fn check_cmd(config: &MissionConfig) -> Option<String> {
    let verb = config.cmd.as_deref()?;
    if darkmux_types::config_access::cmd_allowed(verb) {
        None
    } else {
        Some(format!(
            "config \"{}\" requires the allowlist entry \"{verb}\" — darkmux holds no \
             credentials of its own and refuses to run this config's shell-out on the \
             operator's behalf until it is explicitly allowed. Run `darkmux config set \
             cmd.enabled true` and `darkmux config set cmd.allowed <comma-separated-list-including-{verb}>` \
             to allow it (the second command REPLACES the whole list — include every command you \
             want allowed).",
            config.id
        ))
    }
}

/// (#1684, schema 2.1) The panel-advertising block on a [`MissionConfig`].
/// Presence — not any particular field value — is the signal `darkmux acp`
/// reads at `session/new` to decide whether to advertise this config's `id`
/// as a slash command (see `src/acp_panel.rs` in the `darkmux` binary
/// crate, which enumerates the merged mission-config registry and filters
/// on `panel.is_some()`).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct PanelConfig {
    /// A short, UI-facing label for the command palette. `MissionConfig.
    /// description` is deliberately long-form dev prose (provenance,
    /// design rationale — see that field's own callers), unsuitable to
    /// render verbatim in an editor's slash-command list. `None` falls
    /// back to `MissionConfig.name`, NEVER to `MissionConfig.description`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Optional input hint shown in the editor before the user has typed
    /// anything after the command name — the ACP `UnstructuredCommandInput`
    /// hint text. `None` advertises the command with no input hint.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hint: Option<String>,
    /// Lenient-on-read overflow (contract 7) — a future sub-field on this
    /// block is safe to add without another schema bump.
    #[serde(flatten)]
    pub extras: BTreeMap<String, serde_json::Value>,
}

/// (#1684) The reserved task id a panel-invoked config's task can name in
/// its own `reads`/`depends_on` to receive the raw text typed after the
/// command name — never a real document-declared task, always resolved at
/// RUN time by the panel-command caller (`src/acp_panel.rs`'s ephemeral
/// runner injects a real `procedural.noop` task under this id into a
/// CLONE of the document before `interpret` runs; the launch/subprocess
/// path forwards the same raw text as `--param args=<raw>`). Double-
/// underscore-wrapped (matching the injected phase id
/// `__panel_args_phase__`) to keep collisions with an operator's own task
/// ids vanishingly unlikely.
///
/// [`MissionConfig::validate`] treats a `reads`/`depends_on` entry naming
/// this EXACT id as always-resolvable (never "dangling unknown task id"),
/// even though it never appears as a real [`TaskConfig`] in the document
/// itself — without this carve-out, a config declaring `reads:
/// ["__panel_args__"]` would fail `validate()` (and therefore `darkmux
/// doctor`'s mission-config check, and `darkmux mission launch`, which
/// runs `validate()` before minting) purely because the reference looks
/// dangling to a check that only knows about the STATIC document, never
/// this runtime-injected convention.
pub const PANEL_ARGS_TASK_ID: &str = "__panel_args__";

/// If any task in `config` names [`PANEL_ARGS_TASK_ID`] (`"__panel_args__"`)
/// in its own `reads` or `depends_on`, prepend a new phase carrying exactly
/// one `procedural.noop` task under that id, whose step's `config.output`
/// is `args` verbatim (empty string when no argument text was supplied — a
/// graph that declares `reads: ["__panel_args__"]` must still resolve
/// cleanly through `interpret`, so this seeds an empty value rather than
/// skipping injection). A config that never references the reserved id is
/// untouched.
///
/// **Two callers, one mechanism (#1685).** Originally `acp_panel.rs`'s own
/// private helper (the ACP ephemeral route's `args` — the raw text typed
/// after a panel slash command); moved here so
/// `mission_launch::launch`'s DIRECT `darkmux mission launch <id> --param
/// args=<value>` path can call the SAME injection before `interpret` runs.
/// Before this move, a config declaring `reads: ["__panel_args__"]`
/// (needed by any panel verb that takes an argument, e.g. a PR number)
/// resolved fine through the ACP route but HARD-FAILED `interpret` on a
/// direct CLI launch — "reads unknown task id `__panel_args__`" — because
/// nothing on that path ever injected the synthetic task. Both callers now
/// share this one function so the two entry points behave identically.
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
pub fn inject_panel_args_task_if_referenced(config: &mut MissionConfig, args: &str) {
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
        excludes: Vec::new(),
        enabled: None,
        id: PANEL_ARGS_TASK_ID.to_string(),
        description: Some("synthetic: the raw text typed after the panel command name (ACP) or the \
                            `--param args=<value>` flag (direct CLI launch)".to_string()),
        display_name: None,
        depends_on: Vec::new(),
        reads: Vec::new(),
        role_id: None,
        run_on: None,
        steps: vec![StepConfig {
            enabled: None,
            id: format!("{PANEL_ARGS_TASK_ID}-step"),
            kind: "procedural.noop".to_string(),
            config: serde_json::json!({"output": args}),
            gate: None,
            extras: BTreeMap::new(),
        }],
        grow: None,
        extras: BTreeMap::new(),
    };
    config.phases.insert(
        0,
        PhaseConfig {
            enabled: None,
            id: "__panel_args_phase__".to_string(),
            description: None,
            display_name: None,
            tasks: vec![args_task],
            extras: BTreeMap::new(),
        },
    );
}

/// A declared runtime-only input a mission config's LAUNCHER must supply
/// (Packet 3+) — a value that is genuinely per-launch (a diff, a worktree
/// path, a case id) and therefore doesn't belong IN the static document.
/// Purely a documentation contract at this packet — nothing consumes it
/// yet; Packet 3 decides how a named input maps onto the composed
/// `Task`/`Step` fields it feeds (workdir, role override, step config
/// substitution, …).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MissionInput {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Whether the launcher must supply this input to proceed. Absent
    /// (`None`) is treated as `true` (required) by convention — an
    /// undeclared optionality on a runtime-only input is the more
    /// conservative default (better to demand a value than silently run
    /// with one missing). Not enforced by [`MissionConfig::validate`] at
    /// this packet (no launcher exists yet to check against) — recorded
    /// here as the intended semantic for Packet 3.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub required: Option<bool>,
    /// (#2310 P4e) The value a launch uses when the operator supplies
    /// none — filled into the launch's collected inputs by
    /// `mission_launch::apply_input_defaults` BEFORE any placeholder
    /// resolution, so a document may reference the input from an EMBEDDED
    /// position (`"…{{mod_wait_seconds}}…"`) without the launch being
    /// refused for an uncollected input.
    ///
    /// **Why a defaulted input is not the same as an absent one.** The
    /// whole-value placeholder shape (`"draws": "{{draws}}"`) already had
    /// a way to express "unset": the key is OMITTED from the step config
    /// and the step kind's own Rust default applies. That mechanism cannot
    /// reach INSIDE a string, and a shell command is one string — so an
    /// input a command interpolates needs its default at the DOCUMENT
    /// layer, where the operator can also read it off `mission config
    /// show`. Only inputs that are not [`Self::ignored`] are defaulted (an
    /// ignored input's warning keys on the operator having supplied it, so
    /// a default there would make every launch warn).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default: Option<serde_json::Value>,
    /// (#2310 P4c-2 item 4) `true` when this input is accepted for
    /// CLI-surface parity with another config but has NO EFFECT here —
    /// e.g. `review.json`'s `bundler`, accepted so an operator carrying
    /// a param line over from the frozen `review` config gets a signal
    /// instead of a silent no-op. The generic launcher warns (naming
    /// [`Self::ignored_reason`]) when the operator supplies an ignored
    /// input, on the `--dry-run` path too, before anything mints. Absent
    /// or `false` means the input is live. STRUCTURAL: any config may
    /// declare this on any input; the launcher's check never matches on a
    /// config id (replaces the old `config.id == "review"` special
    /// case).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ignored: Option<bool>,
    /// Why this input is ignored — rendered into the warning. Unenforced
    /// at parse time (a missing reason on an `ignored: true` input is a
    /// doc smell, not a load-bearing break); `darkmux doctor`/`validate`
    /// is where that could be surfaced if it ever needs to be loud.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ignored_reason: Option<String>,
    #[serde(flatten)]
    pub extras: BTreeMap<String, serde_json::Value>,
}

/// One phase, as data. `id` is a SUFFIX — the launcher composes the real
/// `Phase.id` (e.g. `<mission-id>-<suffix>`), matching
/// `build_review_graph`'s caller-supplied `investigate_phase_id`-style
/// convention. **A phase with zero tasks is valid by design** — it
/// expresses a manual/freeform phase (operator-driven transitions, no
/// automated Task/Step graph underneath): a duration-container phase
/// ("wait for the trip") or a blog-post phase ("draft by hand, no
/// dispatch"). `validate()` does not require `tasks` to be non-empty.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PhaseConfig {
    pub id: String,
    /// (#2299) `false` prunes this item at mint: it never exists in the run —
    /// not in the task graph, not in the viewer, no record. Absent means
    /// enabled; the FIELD is the gate, never its presence. Provenance is the
    /// config snapshot the run keeps, which carries the flag verbatim.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// (#1398) Operator-facing short label — `description` is deliberately
    /// LONG (for `coder-phase` it's the coder's dispatch brief verbatim;
    /// for `review` it's multi-sentence transcription prose), so a single
    /// overloaded field can't serve both jobs. `None` on a config that
    /// doesn't set one — every renderer falls back to `id` (never
    /// `description`; see the graph lens / `interpret::TaskOverride`'s twin
    /// doc). Threaded through [`interpret::interpret`]'s mint path onto the
    /// persisted `crew::types::Phase::display_name`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    #[serde(default)]
    pub tasks: Vec<TaskConfig>,
    #[serde(flatten)]
    pub extras: BTreeMap<String, serde_json::Value>,
}

/// One task, as data — mirrors `crew::types::Task`'s shape (the ASSIGNABLE
/// unit: role/profile/workdir/image fixed for the task's whole duration,
/// `depends_on` the only cross-task dependency/concurrency declaration).
/// `id` and `depends_on` entries are DOCUMENT-WIDE (not phase-scoped) —
/// `depends_on` may name a task in an EARLIER phase, exactly as
/// `build_review_graph`'s `report` phase's `synthesis` task depends on the
/// `investigate` phase's `dedup` task. `profile_name`/`workdir`/`image` are
/// deliberately NOT fields here (unlike the real `Task`) — those are
/// genuinely per-launch values (a worktree path, an image override) that
/// belong in [`MissionConfig::inputs`], not the static document; see the
/// packet report for how the two built-in configs use `role_id` as an
/// overridable default and push workdir/image to `inputs` entirely.
///
/// **Placeholder-prefix rule (task AND step ids).** When the Rust builder a
/// config transcribes composes its `Task`/`Step` ids from a caller-supplied
/// phase id (`default_phase_graph`'s `format!("{phase_id}-worktree")` /
/// `-coder` / `-verify`, steps appending `-step`), the config writes those
/// ids with the owning [`PhaseConfig::id`] as a LITERAL prefix — that
/// literal prefix stands in for the real phase id: at launch (#1284
/// Packet 3), the launcher substitutes its composed phase id for the
/// phase-config id wherever it prefixes a task/step id, so the persisted
/// ids match what the Rust builder produces today byte for byte (task ids
/// surface in `mission status`, the viewer, and lifecycle records — a
/// silent id-scheme change at cutover is exactly what this rule prevents).
/// A config whose builder uses FIXED ids (`build_review_graph`'s
/// `review-bundle-task` etc.) writes them verbatim — no substitution. Each
/// built-in config names which convention it uses in its own top-level
/// `description`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TaskConfig {
    pub id: String,
    /// (#2299) `false` prunes this item at mint: it never exists in the run —
    /// not in the task graph, not in the viewer, no record. Absent means
    /// enabled; the FIELD is the gate, never its presence. Provenance is the
    /// config snapshot the run keeps, which carries the flag verbatim.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
    /// (#2310 P4f, schema 3.5) Task ids, document-wide, that must not be
    /// ENABLED alongside this one — two ways to staff the SAME slot, of
    /// which exactly one may be live. `review.json`'s `create-mods`
    /// phase is the first user: `create-mod` waits for a frontier-written
    /// mod (the attended path) and `create-mod-dispatch` sends a coder to
    /// a hosted-endpoint profile (the unattended runner path); both
    /// enabled would mint two mod-writing tasks per finding, which is not
    /// a degraded run but a duplicated seat.
    ///
    /// Enforced in [`MissionConfig::validate`] as an `Error`, never at run
    /// time: the operator flips one `enabled` field in their own copy, and
    /// the mistake is worth catching before a mint rather than after a
    /// phase's worth of dispatches. The relation is symmetric in EFFECT —
    /// declaring it on one side is enough, and declaring it on both
    /// reports once, not twice.
    ///
    /// **What the disabled state does and does not silence.** The CONFLICT
    /// is only reported when both sides would actually be minted — a pair
    /// with one side `enabled: false`, or living under a disabled phase,
    /// is the intended shipping shape and says nothing. A MALFORMED
    /// exclusion is not covered by that: an id naming no task in the
    /// document, or a task naming itself, is an `Error` regardless of
    /// enabled state. That asymmetry is deliberate. An exclusion that
    /// resolves to nothing fails OPEN — it protects nothing while looking
    /// exactly like protection — and the copy most likely to carry that
    /// typo is a template shipping DISABLED, where it costs nothing right
    /// up until the operator enables it and both seats run.
    ///
    /// A pre-3.5 reader overflows the field into `extras` and mints
    /// whatever `enabled` says, which is the additive contract: every
    /// SHIPPED document keeps one of each excluded pair disabled, so an
    /// older binary reading one behaves exactly as it does today.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub excludes: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// (#1398) Operator-facing short label — same overload split as
    /// [`PhaseConfig::display_name`], one level down. `None` falls back to
    /// `id` everywhere a Task renders.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub depends_on: Vec<String>,
    /// (#1619, schema 1.4) Task ids whose LAST STEP OUTPUT every step of
    /// this task receives as input (#2310 P2a — previously only the first
    /// step) — the run-scoped OUTPUT LEDGER made nameable.
    /// Every completed task's output is available to any later task by
    /// naming it here; no `depends_on` edge is needed to receive data.
    ///
    /// `reads` ORDERS execution exactly like `depends_on` (you cannot read
    /// an output that hasn't been written — the scheduler waits for every
    /// `reads` target to complete) but it is NOT a rendered graph edge: the
    /// mission-graph lens draws only `depends_on`. That split is the whole
    /// point. Before this field, `depends_on` was the ONLY way to receive an
    /// upstream task's output, so the built-in review config had to declare
    /// cross-phase task edges (synthesis → dedup two phases back) that
    /// rendered as phase bypasses — the operator read them as design
    /// short-circuits. Data flow now rides the ledger invisibly; `depends_on`
    /// is left for ordering the graph should SHOW (typically intra-phase).
    ///
    /// Like `depends_on`, entries are DOCUMENT-WIDE task ids and join the
    /// same cycle detection — a `reads` loop is as unschedulable as a
    /// `depends_on` loop.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub reads: Vec<String>,
    /// Default crew role this task dispatches, e.g. `"coder"`. Not
    /// necessarily final — Packet 3's launcher may let a `MissionInput`
    /// override it per-launch (the built-in `coder-phase` config declares
    /// exactly that intent — see its `inputs`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub role_id: Option<String>,
    /// (#2310 P4, schema 3.4) Which terminal statuses of this task's
    /// `depends_on`/`reads` dependencies satisfy readiness — mirrors
    /// [`crate::types::Task::run_on`]'s doc exactly (this field is copied
    /// verbatim into that one at interpret time; `None` here resolves to
    /// [`crate::types::default_run_on`], the same `["complete"]` every
    /// pre-3.4 document already behaved as). `Some(vec![])` is legal JSON
    /// but validated as an Error (see `validate`) — an empty `run_on`
    /// would make the task permanently unready, the same "wedges forever"
    /// failure mode a zero-step task already gets caught for.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_on: Option<Vec<String>>,
    /// Ordered — step at index `i` depends on the step at index `i - 1`
    /// (mirrors `crew::types::Step`'s #1341 "no depends_on field" —
    /// purely positional intra-task ordering).
    #[serde(default)]
    pub steps: Vec<StepConfig>,
    /// (#2300, schema 3.2) Run-time fan-out: this task is a TEMPLATE grown
    /// into N real copies from an upstream step's OUTPUT. See [`GrowSpec`].
    /// `None` (every pre-3.2 document) means the task mints exactly once,
    /// as it always did.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub grow: Option<GrowSpec>,
    #[serde(flatten)]
    pub extras: BTreeMap<String, serde_json::Value>,
}

/// (#2300) The map-shaped fan-out a [`TaskConfig`] declares: one copy of
/// this task per item in an artifact an EARLIER phase produced.
///
/// ```json
/// "grow": {
///   "from": "crawl-plan-task",
///   "items": "units",
///   "id": "{{item.id}}",
///   "config": { "unit": "{{item.id}}", "rule": "{{item.rule}}" }
/// }
/// ```
///
/// **Where the data comes from.** `from` names a task in an EARLIER phase
/// (a same-phase or later-phase `from` is a validation `Error` — the
/// producer must have run before the consumer's phase is minted, and
/// growth happens at the phase boundary). That task's LAST step `output`
/// is read as a PATH to a JSON file — the contract every producing step
/// honors (`crawl.plan` writes `plan/<rule>.json` and returns that path).
/// `items` names a TOP-LEVEL key of that file whose value is an array.
///
/// **What gets minted.** One copy of the whole task — every step, in
/// order — per item. The template itself is never minted. Zero items
/// mints zero copies and the phase completes with a recorded reason
/// (`grew_nothing`), which is a real outcome, not a failure.
///
/// **Templating.** `id` renders into the copy's task-id SUFFIX
/// (`<template id>-<rendered>`; each step id gets the same suffix), and
/// every key of `config` is merged into EVERY step's `config` in the copy.
/// `{{item.<field>}}` substitutes the item's own top-level SCALAR fields
/// (string/number/bool); naming an object or array field is an error, not
/// a stringified blob.
///
/// **Edges.** `depends_on`/`reads` declared on the template apply to every
/// copy; copies never depend on each other, so a track fails alone.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GrowSpec {
    /// Task id, in an EARLIER phase, whose last step output is the path to
    /// the JSON artifact to grow from.
    pub from: String,
    /// Top-level key of that artifact holding the array of items.
    pub items: String,
    /// Per-copy task-id suffix template, e.g. `"{{item.id}}"`.
    pub id: String,
    /// Templates merged into every step's `config` in each copy. An object;
    /// anything else is a validation `Error`.
    #[serde(default)]
    pub config: serde_json::Value,
    #[serde(flatten)]
    pub extras: BTreeMap<String, serde_json::Value>,
}

// (#1550 cluster item 2) `ExpansionSpec` — the "one `TaskConfig` template
// expands into N real Task/Step copies" primitive — and its
// `default_kind_pattern` helper were removed here. See
// `MISSION_CONFIG_SCHEMA`'s doc (schema 2.0) for why: fully specified, fully
// interpreted, but never actually fed by either production launcher.

/// One step, as data. `kind` names a REGISTERED `step_kinds::StepKind` id —
/// Tier 1 generic (e.g. `"dispatch.internal"`), Tier 2 pattern, or a Tier 3
/// mission-bespoke id (e.g. `"review.bundle"`, `"mission.worktree"`, #1352).
/// `config` is kind-specific and opaque to this schema — mirrors
/// `crew::types::Step.config`'s own flat `serde_json::Value` bag.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StepConfig {
    pub id: String,
    pub kind: String,
    /// (#2299) `false` prunes this item at mint: it never exists in the run —
    /// not in the task graph, not in the viewer, no record. Absent means
    /// enabled; the FIELD is the gate, never its presence. Provenance is the
    /// config snapshot the run keeps, which carries the flag verbatim.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
    #[serde(default)]
    pub config: serde_json::Value,
    /// (#1684 Packet 2, schema 2.2) The operator sign-off gate. `None` (the
    /// default; every pre-2.2 document) means the step runs exactly as
    /// before — `darkmux-crew`'s `scheduler::run_step_graph` never invokes
    /// a gate handler for an ungated step. The only value RECOGNIZED today
    /// is the literal string `"operator"` (`darkmux_crew::gate::
    /// crate::gate::GATE_KIND_OPERATOR`) — [`interpret::interpret`] threads this field
    /// onto the executable `crew::types::Step::gate` verbatim (no
    /// resolution/validation happens here; that's [`MissionConfig::validate`]'s
    /// job — see the `gate` finding it emits for an unrecognized value).
    ///
    /// **Fail-closed contract.** A value other than `"operator"` is NOT
    /// silently treated as ungated — [`MissionConfig::validate`] surfaces it
    /// as a `Warning` (lenient-on-read; a future minor bump may recognize
    /// more gate kinds), and at RUN time `gate::resolve_gate` refuses the
    /// step outright (never invokes the caller's handler for a kind it
    /// doesn't understand) rather than running it unattended. An operator
    /// typo in this field therefore blocks the step, never silently skips
    /// the sign-off it was meant to require.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gate: Option<String>,
    #[serde(flatten)]
    pub extras: BTreeMap<String, serde_json::Value>,
}

impl PhaseConfig {
    /// (#2299) Absent means enabled.
    pub fn is_enabled(&self) -> bool {
        self.enabled.unwrap_or(true)
    }
}

impl TaskConfig {
    /// (#2299) Absent means enabled.
    pub fn is_enabled(&self) -> bool {
        self.enabled.unwrap_or(true)
    }
}

impl StepConfig {
    /// (#2299) Absent means enabled.
    pub fn is_enabled(&self) -> bool {
        self.enabled.unwrap_or(true)
    }
}

/// Severity of a [`ValidationFinding`]. `Error` blocks a config from being
/// USABLE (Packet 3's launcher would refuse it); `Warning` is
/// operator-actionable but non-blocking — notably, an unrecognized step
/// kind is ALWAYS `Warning`, never `Error` (see `validate`'s doc: a Tier 3
/// kind that registers at composition time is invisible to a
/// document-level check, so "unknown" doesn't mean "wrong").
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FindingSeverity {
    Error,
    Warning,
}

/// One actionable validation finding — a JSON-pointer-ish `path` into the
/// document plus a human-readable `message`. `Display` renders the
/// `[level] path: message` line both `darkmux doctor` and a future CLI can
/// print verbatim.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ValidationFinding {
    pub severity: FindingSeverity,
    pub path: String,
    pub message: String,
}

impl std::fmt::Display for ValidationFinding {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let level = match self.severity {
            FindingSeverity::Error => "error",
            FindingSeverity::Warning => "warning",
        };
        write!(f, "[{level}] {}: {}", self.path, self.message)
    }
}

impl MissionConfig {
    /// Semantic validation — SEPARATE from parsing (contract 7): a
    /// lenient-on-read document always PARSES regardless of content; this
    /// is what a caller (`darkmux doctor`, Packet 3's launcher) runs to
    /// decide whether the document is actually USABLE. [`load`] never
    /// calls this — semantic validation never runs on the hot load path.
    ///
    /// `known_step_kinds` is the caller's registered-kind universe (e.g.
    /// `StepKindRegistry::with_builtins().ids()`). A step whose `kind`
    /// isn't in it produces a `Warning`, NEVER an `Error`: Tier 3 kinds
    /// (#1352) register into their OWN per-mission registry at
    /// COMPOSITION time (`build_review_graph`, `default_phase_graph`),
    /// which no document-level check can see — an "unknown" kind here may
    /// simply be a Tier 3 id this call site's registry doesn't carry, not
    /// a real mistake. Pass `&[]` to skip the kind-reference check
    /// entirely (an empty universe is treated as "unverifiable", not
    /// "everything is wrong" — no kind warnings are emitted).
    pub fn validate(&self, known_step_kinds: &[&str]) -> Vec<ValidationFinding> {
        let mut findings = Vec::new();

        if self.id.trim().is_empty() {
            findings.push(ValidationFinding {
                severity: FindingSeverity::Error,
                path: "id".to_string(),
                message: "mission config id is empty".to_string(),
            });
        }

        // (#2004) `gh_verb` was RENAMED to `cmd` in schema 3.0. Lenient-on-read
        // means the old key parses cleanly into `extras` and is inert — which
        // is exactly the danger here, because this gate FAILS OPEN: a config
        // declaring no verb is never blocked. So a document left on the old
        // name would silently lose its gate and run UNGATED, with the shell-out
        // it was protecting proceeding as if the operator had allowlisted it.
        // Loud at validate/doctor time, never a runtime surprise.
        if self.extras.contains_key("gh_verb") {
            findings.push(ValidationFinding {
                severity: FindingSeverity::Error,
                path: "gh_verb".to_string(),
                message: format!(
                    "config \"{}\" declares `gh_verb`, which was RENAMED to `cmd` in schema 3.0 \
                     (see MISSION_CONFIG_SCHEMA's doc) — this key now overflows into extras and \
                     is silently ignored, so this config would run WITHOUT the allowlist gate it \
                     asked for. Rename the field to `cmd` and set `schema_version` to \"3.0\"; \
                     the allowlist it is checked against is now `cmd.allowed` (was `gh.allowed`)",
                    self.id
                ),
            });
        }
        if self.name.trim().is_empty() {
            findings.push(ValidationFinding {
                severity: FindingSeverity::Error,
                path: "name".to_string(),
                message: "mission config name is empty".to_string(),
            });
        }

        if let Some(v) = &self.schema_version {
            match parse_major(v) {
                Some(major) if major != current_major() => {
                    findings.push(ValidationFinding {
                        severity: FindingSeverity::Warning,
                        path: "schema_version".to_string(),
                        message: format!(
                            "document schema_version \"{v}\" (major {major}) differs from this \
                             binary's MISSION_CONFIG_SCHEMA \"{MISSION_CONFIG_SCHEMA}\" (major \
                             {}) — a major-version mismatch may mean this binary can't fully \
                             interpret the document, or vice versa",
                            current_major()
                        ),
                    });
                }
                None => {
                    findings.push(ValidationFinding {
                        severity: FindingSeverity::Warning,
                        path: "schema_version".to_string(),
                        message: format!("schema_version \"{v}\" is not a MAJOR.MINOR string"),
                    });
                }
                _ => {}
            }
        }

        // Ids are DOCUMENT-WIDE, not phase/task-scoped — `Task.depends_on`
        // may cross phases, and every `Step`/`Task` lands in one flat map
        // once composed into a real graph, so a same-named collision
        // anywhere in the document is a real problem.
        let mut seen_phase_ids: BTreeSet<&str> = BTreeSet::new();
        let mut seen_task_ids: BTreeSet<&str> = BTreeSet::new();
        let mut seen_step_ids: BTreeSet<&str> = BTreeSet::new();
        let mut all_task_ids: BTreeSet<&str> = BTreeSet::new();

        // (#2300) Task id -> the index of the phase that declares it, so a
        // `grow.from` can be checked for the ONE relation that makes growth
        // possible: the producer's phase must have already run.
        let mut phase_index_of_task: BTreeMap<&str, usize> = BTreeMap::new();
        let mut grow_templates: BTreeSet<&str> = BTreeSet::new();
        // (#2345 CONSIDER-2, round 2) A task this document itself prunes at
        // mint time (`enabled: false` on the task, or on its owning phase —
        // `prune.rs`'s own two checks) is never minted either, same as a
        // `grow` template — it just has a different REASON for never
        // producing a `Step.output`. Tracked alongside `grow_templates` so
        // `outcome_from` can refuse naming either kind below.
        let mut disabled_tasks: BTreeSet<&str> = BTreeSet::new();
        for (pi, phase) in self.phases.iter().enumerate() {
            for task in &phase.tasks {
                all_task_ids.insert(task.id.as_str());
                phase_index_of_task.entry(task.id.as_str()).or_insert(pi);
                if task.grow.is_some() {
                    grow_templates.insert(task.id.as_str());
                }
                if !task.is_enabled() || !phase.is_enabled() {
                    disabled_tasks.insert(task.id.as_str());
                }
            }
        }

        for (pi, phase) in self.phases.iter().enumerate() {
            let phase_path = format!("phases[{pi}]");
            if phase.id.trim().is_empty() {
                findings.push(ValidationFinding {
                    severity: FindingSeverity::Error,
                    path: format!("{phase_path}.id"),
                    message: "phase id is empty".to_string(),
                });
            } else if !seen_phase_ids.insert(phase.id.as_str()) {
                findings.push(ValidationFinding {
                    severity: FindingSeverity::Error,
                    path: format!("{phase_path}.id"),
                    message: format!("duplicate phase id \"{}\"", phase.id),
                });
            }

            for (ti, task) in phase.tasks.iter().enumerate() {
                let task_path = format!("{phase_path}.tasks[{ti}]");
                if task.id.trim().is_empty() {
                    findings.push(ValidationFinding {
                        severity: FindingSeverity::Error,
                        path: format!("{task_path}.id"),
                        message: "task id is empty".to_string(),
                    });
                } else if !seen_task_ids.insert(task.id.as_str()) {
                    findings.push(ValidationFinding {
                        severity: FindingSeverity::Error,
                        path: format!("{task_path}.id"),
                        message: format!("duplicate task id \"{}\"", task.id),
                    });
                }

                // (#2310 P4) `run_on` names which TERMINAL statuses of
                // this task's dependencies satisfy readiness — only
                // "complete" and "error" are recognized today (mirrors
                // `scheduler::dependency_satisfies_run_on`'s literal
                // match). An unknown value is refused loudly rather than
                // silently never matching (which would wedge the task
                // Planned forever, indistinguishable from a typo'd
                // dependency). An explicitly empty `run_on: []` is the
                // same failure shape as a zero-step task (below) — never
                // satisfiable — so it's an Error too, not a silent
                // fallback to the default.
                if let Some(run_on) = &task.run_on {
                    if run_on.is_empty() {
                        findings.push(ValidationFinding {
                            severity: FindingSeverity::Error,
                            path: format!("{task_path}.run_on"),
                            message: format!(
                                "task \"{}\" declares an empty run_on — no dependency status could \
                                 ever satisfy it, so the task would wedge Planned forever",
                                task.id
                            ),
                        });
                    }
                    for value in run_on {
                        if value != "complete" && value != "error" {
                            findings.push(ValidationFinding {
                                severity: FindingSeverity::Error,
                                path: format!("{task_path}.run_on"),
                                message: format!(
                                    "task \"{}\" run_on names unknown value \"{value}\" — only \
                                     \"complete\" and \"error\" are recognized",
                                    task.id
                                ),
                            });
                        }
                    }
                    // (C1, #2310 P4a review) `run_on: ["error"]` alone
                    // validates structurally clean but is a likely
                    // authoring mistake: on an ORDINARY clean run, every
                    // dependency reaches `Complete`, never `Error` — a
                    // task whose `run_on` doesn't accept `"complete"`
                    // then NEVER becomes ready on that (the overwhelming
                    // majority) path, and sits `Planned` until the
                    // mission-level close reconcile rolls it to
                    // `Abandoned` as "not started". Warning, not Error —
                    // "runs only after a failure" is occasionally exactly
                    // what an operator wants (a cleanup/notify task), so
                    // this is surfaced, never refused.
                    if !run_on.is_empty() && !run_on.iter().any(|v| v == "complete") {
                        findings.push(ValidationFinding {
                            severity: FindingSeverity::Warning,
                            path: format!("{task_path}.run_on"),
                            message: format!(
                                "task \"{}\" run_on omits \"complete\"; this task runs only \
                                 after an upstream failure",
                                task.id
                            ),
                        });
                    }
                }

                // (#1619) `reads` gets the SAME structural checks as
                // `depends_on` — it orders execution identically, so a
                // dangling or self-referential entry is exactly as fatal.
                for (relation, entries) in
                    [("depends_on", &task.depends_on), ("reads", &task.reads)]
                {
                    for dep in entries {
                        if dep == &task.id {
                            findings.push(ValidationFinding {
                                severity: FindingSeverity::Error,
                                path: format!("{task_path}.{relation}"),
                                message: format!("task \"{}\" {relation} itself", task.id),
                            });
                        } else if dep.as_str() == PANEL_ARGS_TASK_ID {
                            // (#1684) The reserved panel-args id is never a
                            // real document task — it resolves at RUN time
                            // (see PANEL_ARGS_TASK_ID's own doc). Not
                            // dangling; deliberately exempted here.
                        } else if !all_task_ids.contains(dep.as_str()) {
                            findings.push(ValidationFinding {
                                severity: FindingSeverity::Error,
                                path: format!("{task_path}.{relation}"),
                                message: format!(
                                    "task \"{}\" {relation} unknown task id \"{dep}\"",
                                    task.id
                                ),
                            });
                        } else if phase_index_of_task.get(dep.as_str()).is_some_and(|d| *d > pi) {
                            // (#2310 fix-loop C2 / C2-1) A FORWARD edge —
                            // depending on a task declared in a LATER phase.
                            // LEGAL, and it works: ids are document-wide and
                            // every scheduler pass runs over the CUMULATIVE
                            // maps, so the later phase's own pass runs this
                            // task. But the declaring phase cannot finish
                            // within its own pass, which surprises anyone
                            // reading the document top-to-bottom — and a
                            // sweep that mistook the shape for dead work
                            // silently dropped it once already. Warning, not
                            // Error: pre-1.0 documents may rely on it, and
                            // refusing a working shape at load time is a
                            // bigger break than naming it.
                            findings.push(ValidationFinding {
                                severity: FindingSeverity::Warning,
                                path: format!("{task_path}.{relation}"),
                                message: format!(
 "task \"{}\" {relation} \"{dep}\", which is declared in a later phase — this task cannot run \
  in its own phase's pass; it runs once that later phase's pass reaches it",
                                    task.id
                                ),
                            });
                        }
                    }
                }

                // (#swarm-5) A task with zero steps is a graph the
                // scheduler can never finish: a task's status derives from
                // its steps, so with none it can never reach Complete — it
                // wedges Planned forever, every downstream `depends_on`
                // never unblocks, and the mission sits permanently stuck
                // with nothing erroring. That is a composition mistake, and
                // this is exactly the load-time/doctor surface composition
                // mistakes are supposed to fail loudly at (contract 7:
                // lenient on READ, loud at validate) — not a runtime hang
                // the operator diagnoses from a frozen graph lens. (#1550
                // cluster item 2: the EXPANDING-task exemption this check
                // used to carry — a template task's steps materialize
                // per-item at interpret time — retired with the expansion
                // primitive; every task's steps are now real from the
                // document, so the exemption's premise is gone too.)
                // (#2300) `grow` — the run-time fan-out. Every check here
                // is an Error, not a Warning: a growth spec that can't
                // resolve doesn't degrade, it mints ZERO tasks, and a phase
                // that silently mints nothing is exactly the failure mode
                // the retired `expand` primitive shipped for two schema
                // versions (see MISSION_CONFIG_SCHEMA's 2.0 note).
                if let Some(grow) = &task.grow {
                    let grow_path = format!("{task_path}.grow");
                    if grow.from.trim().is_empty() {
                        findings.push(ValidationFinding {
                            severity: FindingSeverity::Error,
                            path: format!("{grow_path}.from"),
                            message: format!("task \"{}\" declares `grow` with an empty `from`", task.id),
                        });
                    } else if grow.from == task.id {
                        findings.push(ValidationFinding {
                            severity: FindingSeverity::Error,
                            path: format!("{grow_path}.from"),
                            message: format!("task \"{}\" grows from itself", task.id),
                        });
                    } else {
                        match phase_index_of_task.get(grow.from.as_str()) {
                            None => findings.push(ValidationFinding {
                                severity: FindingSeverity::Error,
                                path: format!("{grow_path}.from"),
                                message: format!(
                                    "task \"{}\" grows from unknown task id \"{}\"",
                                    task.id, grow.from
                                ),
                            }),
                            Some(&from_pi) if from_pi >= pi => findings.push(ValidationFinding {
                                severity: FindingSeverity::Error,
                                path: format!("{grow_path}.from"),
                                message: format!(
 "task \"{}\" (phase \"{}\") grows from task \"{}\", which is declared in the {} phase \"{}\" — growth \
  happens at a PHASE BOUNDARY, so the producing task must live in an EARLIER phase or its output does \
  not exist yet when this phase is minted",
                                    task.id,
                                    phase.id,
                                    grow.from,
                                    if from_pi == pi { "same" } else { "later" },
                                    self.phases[from_pi].id
                                ),
                            }),
                            Some(_) => {}
                        }
                    }
                    if grow.items.trim().is_empty() {
                        findings.push(ValidationFinding {
                            severity: FindingSeverity::Error,
                            path: format!("{grow_path}.items"),
                            message: format!(
 "task \"{}\" declares `grow` with an empty `items` — name the top-level key of the produced JSON \
  holding the array to map over",
                                task.id
                            ),
                        });
                    }
                    if grow.id.trim().is_empty() {
                        findings.push(ValidationFinding {
                            severity: FindingSeverity::Error,
                            path: format!("{grow_path}.id"),
                            message: format!(
 "task \"{}\" declares `grow` with an empty `id` — every copy needs a distinct id suffix (e.g. \"{{{{item.id}}}}\")",
                                task.id
                            ),
                        });
                    }
                    if !grow.config.is_null() && !grow.config.is_object() {
                        findings.push(ValidationFinding {
                            severity: FindingSeverity::Error,
                            path: format!("{grow_path}.config"),
                            message: format!(
 "task \"{}\" declares `grow.config` as {}, but it must be an object — its keys are merged into every \
  grown step's config",
                                task.id,
                                if grow.config.is_array() { "an array" } else { "a scalar" }
                            ),
                        });
                    }
                }

                // (#2300) A grow template is not a real task at run time —
                // its copies do not exist until the phase boundary — so
                // nothing can name it as a dependency and get an edge.
                for (relation, entries) in [("depends_on", &task.depends_on), ("reads", &task.reads)] {
                    for dep in entries {
                        if grow_templates.contains(dep.as_str()) {
                            findings.push(ValidationFinding {
                                severity: FindingSeverity::Error,
                                path: format!("{task_path}.{relation}"),
                                message: format!(
 "task \"{}\" {relation} \"{dep}\", which is a `grow` TEMPLATE — a template is never minted, so this \
  edge would resolve to nothing. Name the template's own `grow.from` producer instead",
                                    task.id
                                ),
                            });
                        }
                    }
                }

                if task.steps.is_empty() {
                    findings.push(ValidationFinding {
                        severity: FindingSeverity::Error,
                        path: format!("{task_path}.steps"),
                        message: format!(
                            "task \"{}\" has no steps — the scheduler can never complete it, \
                             so it would wedge the mission graph forever (every task derives \
                             completion from its steps)",
                            task.id
                        ),
                    });
                }

                // (#1550 cluster item 2, operator correction 2) `expand` was
                // a REAL field through schema 1.4; `TaskConfig`'s
                // `#[serde(flatten)] extras` overflow means a document still
                // declaring it parses cleanly (contract 7, lenient-on-read)
                // and SILENTLY loses its fan-out — worse than the
                // additive-newer-minor hazard #1648 guards (there ignoring
                // an unknown field is at least arguably safe for a FUTURE
                // schema); there is no future schema where `expand` becomes
                // meaningful again, so staying silent here is never
                // appropriate. Loud at validate/doctor time, not a runtime
                // surprise where the fan-out just never happens.
                if task.extras.contains_key("expand") {
                    findings.push(ValidationFinding {
                        severity: FindingSeverity::Error,
                        path: format!("{task_path}.expand"),
                        message: format!(
                            "task \"{}\" declares `expand`, which was REMOVED in schema 2.0 \
                             (see MISSION_CONFIG_SCHEMA's doc) — this key now overflows into \
                             extras and is silently ignored, dropping the fan-out this document \
                             expected. Declare the expanded tasks explicitly instead, one \
                             TaskConfig per item (the built-in \"review\" config's probe stage \
                             is the reference shape, #1512)",
                            task.id
                        ),
                    });
                }

                for (si, step) in task.steps.iter().enumerate() {
                    let step_path = format!("{task_path}.steps[{si}]");
                    if step.id.trim().is_empty() {
                        findings.push(ValidationFinding {
                            severity: FindingSeverity::Error,
                            path: format!("{step_path}.id"),
                            message: "step id is empty".to_string(),
                        });
                    } else if !seen_step_ids.insert(step.id.as_str()) {
                        findings.push(ValidationFinding {
                            severity: FindingSeverity::Error,
                            path: format!("{step_path}.id"),
                            message: format!("duplicate step id \"{}\"", step.id),
                        });
                    }
                    if step.kind.trim().is_empty() {
                        findings.push(ValidationFinding {
                            severity: FindingSeverity::Error,
                            path: format!("{step_path}.kind"),
                            message: "step kind is empty".to_string(),
                        });
                    } else if !known_step_kinds.is_empty()
                        && !known_step_kinds.contains(&step.kind.as_str())
                    {
                        findings.push(ValidationFinding {
                            severity: FindingSeverity::Warning,
                            path: format!("{step_path}.kind"),
                            message: format!(
                                "step \"{}\" references unknown step kind \"{}\"",
                                step.id, step.kind
                            ),
                        });
                    }

                    // (#1684 Packet 2) `gate` is lenient-on-read (contract 7:
                    // an unrecognized value still PARSES) but the only value
                    // this binary actually understands today is
                    // `crate::gate::GATE_KIND_OPERATOR`. A `Warning`, not an `Error` — a
                    // future minor bump may recognize more gate kinds, and
                    // the RUN-time behavior for an unrecognized value is
                    // never "silently ungated" regardless of this finding
                    // (see `gate::resolve_gate`'s fail-closed contract on
                    // `crew::types::Step::gate`) — this is an authoring
                    // hint, not the enforcement mechanism.
                    if let Some(g) = &step.gate {
                        if g != crate::gate::GATE_KIND_OPERATOR {
                            findings.push(ValidationFinding {
                                severity: FindingSeverity::Warning,
                                path: format!("{step_path}.gate"),
                                message: format!(
                                    "step \"{}\" declares gate \"{g}\", which this binary does \
                                     not recognize (only \"{}\" is understood today) — at run \
                                     time an unrecognized gate FAILS CLOSED (the step is refused, \
                                     never silently run ungated), so this is not a config that \
                                     quietly does nothing; fix the value or drop the field",
                                    step.id,
                                    crate::gate::GATE_KIND_OPERATOR
                                ),
                            });
                        }
                    }
                }
            }
        }

        // (#2345 C2) `outcome_from` names the task whose last step's output
        // the launcher promotes as the `mission close` record's payload
        // (see its own field doc). A typo here used to be refused only
        // AFTER the whole run (`src/mission_launch.rs::run_summary_
        // payload`'s close-time check) — for a long-running mission (a
        // crawl, a review) that means every step dispatches, every token
        // spends, hours pass, before the config-authoring mistake ever
        // surfaces, landing as an abandoned phase and a null payload.
        // Refused HERE instead, at validate time, before a single step
        // ever runs — the close-time check stays in place as the backstop
        // for whatever reaches it without going through `validate` first
        // (a hand-edited document swapped in after launch, say).
        if let Some(outcome_from) = &self.outcome_from {
            if !all_task_ids.contains(outcome_from.as_str()) {
                findings.push(ValidationFinding {
                    severity: FindingSeverity::Error,
                    path: "outcome_from".to_string(),
                    message: format!(
                        "outcome_from names unknown task id \"{outcome_from}\" — must name a real \
                         task declared somewhere in `phases`"
                    ),
                });
            } else if grow_templates.contains(outcome_from.as_str()) {
                findings.push(ValidationFinding {
                    severity: FindingSeverity::Error,
                    path: "outcome_from".to_string(),
                    message: format!(
                        "outcome_from names \"{outcome_from}\", which declares `grow` — a grow \
                         TEMPLATE is never itself minted (it stamps zero-or-more real copies at its \
                         phase boundary instead), so it has no `Step.output` of its own to promote \
                         as the close payload"
                    ),
                });
            } else if disabled_tasks.contains(outcome_from.as_str()) {
                // (#2345 CONSIDER-2, round 2) `all_task_ids` (checked above)
                // includes `enabled: false` tasks — they are real DECLARED
                // tasks, just pruned at mint (`prune.rs`), so the "unknown
                // task id" branch above never catches this. Without this
                // check, `outcome_from` naming a disabled task passed
                // `validate` cleanly and only failed at CLOSE time, silently
                // (`run_summary_payload` returning `Ok(None)` reads
                // identically to "the task hasn't produced output yet" —
                // see that function's own doc, widened in the same packet).
                findings.push(ValidationFinding {
                    severity: FindingSeverity::Error,
                    path: "outcome_from".to_string(),
                    message: format!(
                        "outcome_from names \"{outcome_from}\", which is disabled (`enabled: false` \
                         on the task itself, or on its owning phase) — a disabled task is pruned at \
                         mint and never produces a `Step.output`, so it has nothing to promote as \
                         the close payload. Enable the task, or name a different one"
                    ),
                });
            }
        }

        // (#2310 P4f, schema 3.5) `excludes` — two templates for ONE seat,
        // of which exactly one may be live (see `TaskConfig::excludes`).
        // Every branch here is an Error, and each for the same reason the
        // `grow` checks above are: the failure is not a degraded run, it is
        // a document that mints work nobody asked for. Both seats enabled
        // means two mod-writing tasks per finding — one waiting on a
        // frontier that may never answer, one spending endpoint tokens —
        // and the run only reveals that after a phase's worth of dispatches.
        // An exclusion that names NOTHING (a typo, a renamed task) is
        // called out separately because it fails OPEN: it protects nothing
        // and looks, in the document, exactly like protection.
        {
            let mut reported: BTreeSet<(&str, &str)> = BTreeSet::new();
            for (pi, phase) in self.phases.iter().enumerate() {
                for (ti, task) in phase.tasks.iter().enumerate() {
                    if task.excludes.is_empty() {
                        continue;
                    }
                    let path = format!("phases[{pi}].tasks[{ti}].excludes");
                    for peer in &task.excludes {
                        if peer == &task.id {
                            findings.push(ValidationFinding {
                                severity: FindingSeverity::Error,
                                path: path.clone(),
                                message: format!(
                                    "task \"{}\" excludes ITSELF — `excludes` names the OTHER way \
                                     of staffing this slot, never this task; use `enabled: false` \
                                     to turn a task off",
                                    task.id
                                ),
                            });
                        } else if !all_task_ids.contains(peer.as_str()) {
                            findings.push(ValidationFinding {
                                severity: FindingSeverity::Error,
                                path: path.clone(),
                                message: format!(
                                    "task \"{}\" excludes unknown task id \"{peer}\" — an exclusion \
                                     that resolves to nothing fails OPEN (both seats would run), so \
                                     a typo here is silent protection. Name a real task declared \
                                     somewhere in `phases`",
                                    task.id
                                ),
                            });
                        } else if !disabled_tasks.contains(task.id.as_str())
                            && !disabled_tasks.contains(peer.as_str())
                        {
                            // One PAIR is one finding, whichever side (or
                            // both) declared the relation — a duplicated
                            // error reads as two separate defects.
                            let pair = if task.id.as_str() < peer.as_str() {
                                (task.id.as_str(), peer.as_str())
                            } else {
                                (peer.as_str(), task.id.as_str())
                            };
                            if reported.insert(pair) {
                                findings.push(ValidationFinding {
                                    severity: FindingSeverity::Error,
                                    path: path.clone(),
                                    message: format!(
 "tasks \"{}\" and \"{peer}\" are both ENABLED, but they exclude each other — they are two ways to \
  staff the SAME slot, so enabling both mints two tasks per item where one was meant. Set \
  `\"enabled\": false` on whichever one this run should not use",
                                        task.id
                                    ),
                                });
                            }
                        }
                    }
                }
            }
        }

        // (#2310 P4c-2 review MUST FIX) Every undeclared placeholder in the
        // static graph — the SAME check `mission_launch.rs::launch` runs
        // before minting, surfaced here too so `mission config show` and
        // `darkmux doctor` catch a typo without a launch attempt.
        for (where_, name) in inputs::undeclared_placeholders(self) {
            findings.push(ValidationFinding {
                severity: FindingSeverity::Error,
                path: "inputs".to_string(),
                message: format!(
                    "{where_} names placeholder `{{{{{name}}}}}`, which is not a declared input of \
                     this mission config{}",
                    inputs::grow_namespace_hint(&name)
                ),
            });
        }

        // (#2310 P4c-2 review round 2, item a) An EMBEDDED placeholder
        // (part of a larger string, e.g. `"label": "run-{{tag}}"`) naming
        // a DECLARED but OPTIONAL input is a design smell independent of
        // any particular launch: whenever the operator leaves that input
        // unset, `check_embedded_inputs_collected` refuses the launch
        // outright (an embedded placeholder has no key to omit the way a
        // whole-value one does) — so an optional input embedded this way
        // is, in practice, required. Warning, not Error: a document is
        // still USABLE (the launcher enforces the real constraint at
        // launch time), this just flags that the document's own
        // `required: false` is misleading.
        //
        // (#2310 P4e review, MUST FIX) A DEFAULTED input is exempt,
        // because the premise above is false for it: `mission_launch::
        // apply_input_defaults` collects a declared `default` on EVERY
        // launch before either placeholder pass runs, so the input is
        // never unset and `check_embedded_inputs_collected` never has
        // anything to refuse. A document default is in fact the RIGHT
        // way to embed an optional input — it is the only mechanism that
        // reaches inside a string — so warning on it told the operator to
        // make a genuinely-optional input required, on every `mission
        // config show review` and every doctor pass, about a document
        // that is correct.
        for (where_, name) in inputs::embedded_placeholders(self) {
            if let Some(input) = self.inputs.iter().find(|i| i.name == name) {
                if input.required != Some(true) && input.default.is_none() {
                    findings.push(ValidationFinding {
                        severity: FindingSeverity::Warning,
                        path: "inputs".to_string(),
                        message: format!(
                            "{where_} names embedded placeholder `{{{{{name}}}}}`, which is an \
                             optional input; use a whole-value placeholder or make it required"
                        ),
                    });
                }
            }
        }

        // (#2310 P4e review, item 5) `required: true` combined with a
        // `default` is the same shape of contradiction as `ignored` +
        // `required` below, and it fails SILENTLY rather than loudly:
        // `apply_input_defaults` runs BEFORE `missing_required_inputs`, so
        // the default always satisfies the requirement and the launch that
        // `required: true` exists to block can never happen. The operator
        // reading the document believes they must supply the input; the
        // launcher never asks. Error, not Warning — unlike the embedded
        // smell above, there is no reading of this document under which the
        // `required` flag does anything at all.
        for input in &self.inputs {
            if input.required == Some(true) && input.default.is_some() {
                findings.push(ValidationFinding {
                    severity: FindingSeverity::Error,
                    path: format!("inputs[{}]", input.name),
                    message: format!(
                        "input \"{}\" declares BOTH `required: true` and a `default` — the default \
                         is collected before the required check runs, so `required` can never \
                         block a launch; drop one of the two",
                        input.name
                    ),
                });
            }
        }

        // (#2310 P4c-2 review item 4) `ignored: true` combined with
        // `required: true` on the SAME input is a contradiction the
        // launcher would enforce oppositely at the same time: `required`
        // blocks the launch until the operator supplies it, `ignored`
        // exists specifically to tell the operator supplying it does
        // NOTHING. An operator cannot win. `ignored: true` with no
        // `ignored_reason` is a lesser smell (the warning still fires, just
        // with a generic "it has no effect" fallback) — Warning, not Error.
        for input in &self.inputs {
            if input.ignored == Some(true) && input.required == Some(true) {
                findings.push(ValidationFinding {
                    severity: FindingSeverity::Error,
                    path: format!("inputs[{}]", input.name),
                    message: format!(
                        "input \"{}\" declares BOTH `ignored: true` and `required: true` — \
                         `required` blocks the launch until the operator supplies it, `ignored` \
                         says supplying it does nothing; a config cannot ask for both",
                        input.name
                    ),
                });
            }
            if input.ignored == Some(true) && input.ignored_reason.is_none() {
                findings.push(ValidationFinding {
                    severity: FindingSeverity::Warning,
                    path: format!("inputs[{}]", input.name),
                    message: format!(
                        "input \"{}\" declares `ignored: true` with no `ignored_reason` — the \
                         operator-facing warning falls back to a generic \"it has no effect on \
                         this config\" instead of naming why",
                        input.name
                    ),
                });
            }
        }

        findings
    }

    /// True when [`validate`](Self::validate) reports zero `Error`
    /// findings (`Warning`s don't block usability — e.g. an unrecognized
    /// Tier 3 step kind is expected, not a failure).
    pub fn is_valid(&self, known_step_kinds: &[&str]) -> bool {
        !self
            .validate(known_step_kinds)
            .iter()
            .any(|f| f.severity == FindingSeverity::Error)
    }
}

fn current_major() -> u32 {
    parse_major(MISSION_CONFIG_SCHEMA).expect("MISSION_CONFIG_SCHEMA is a valid MAJOR.MINOR constant")
}

fn parse_major(v: &str) -> Option<u32> {
    v.split('.').next()?.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn minimal_json() -> &'static str {
        r#"{"id":"x","name":"X"}"#
    }

    #[test]
    fn minimal_document_parses_and_validates_clean() {
        let cfg: MissionConfig = serde_json::from_str(minimal_json()).unwrap();
        assert_eq!(cfg.id, "x");
        assert_eq!(cfg.name, "X");
        assert!(cfg.phases.is_empty());
        assert!(cfg.inputs.is_empty());
        assert!(cfg.validate(&[]).is_empty());
    }

    #[test]
    fn missing_id_or_name_fails_to_parse() {
        // `id`/`name` are required (non-Option) fields, matching
        // `WorkloadSpec.id`'s precedent (`workloads::types::
        // workload_manifest_rejects_missing_id`) — a document that omits
        // the core identity fields fails to PARSE, not just to validate.
        let missing_id = r#"{"name":"X"}"#;
        assert!(serde_json::from_str::<MissionConfig>(missing_id).is_err());
        let missing_name = r#"{"id":"x"}"#;
        assert!(serde_json::from_str::<MissionConfig>(missing_name).is_err());
    }

    #[test]
    fn empty_id_is_a_validate_time_error_not_a_parse_error() {
        let json = r#"{"id":"","name":"X"}"#;
        let cfg: MissionConfig = serde_json::from_str(json).unwrap();
        let findings = cfg.validate(&[]);
        assert!(
            findings
                .iter()
                .any(|f| f.severity == FindingSeverity::Error && f.path == "id"),
            "expected an id-empty error, got {findings:?}"
        );
    }

    #[test]
    fn unknown_top_level_field_is_tolerated_lenient_on_read() {
        let json = r#"{"id":"x","name":"X","totallyNewField":{"nested":true}}"#;
        let cfg: MissionConfig = serde_json::from_str(json).unwrap();
        assert_eq!(
            cfg.extras.get("totallyNewField"),
            Some(&serde_json::json!({"nested": true}))
        );
    }

    #[test]
    fn newer_schema_version_parses_and_warns_not_errors() {
        let json = r#"{"id":"x","name":"X","schema_version":"99.0"}"#;
        let cfg: MissionConfig = serde_json::from_str(json).unwrap();
        let findings = cfg.validate(&[]);
        assert!(findings.iter().all(|f| f.severity == FindingSeverity::Warning));
        assert!(findings.iter().any(|f| f.path == "schema_version"));
    }

    #[test]
    fn unparseable_schema_version_warns() {
        let json = r#"{"id":"x","name":"X","schema_version":"not-a-version"}"#;
        let cfg: MissionConfig = serde_json::from_str(json).unwrap();
        let findings = cfg.validate(&[]);
        assert!(
            findings
                .iter()
                .any(|f| f.severity == FindingSeverity::Warning && f.path == "schema_version")
        );
    }

    #[test]
    fn absent_schema_version_is_not_drift() {
        let cfg: MissionConfig = serde_json::from_str(minimal_json()).unwrap();
        assert!(cfg.schema_version.is_none());
        assert!(cfg.validate(&[]).is_empty());
    }

    // ─── (#2310 P4f, schema 3.5) `excludes`: two ways to staff one seat ───
    //
    // A phase can ship TWO templates for the same slot — review's
    // create-mods phase carries the attended `create-mod` (wait for a
    // frontier-written mod) and the unattended `create-mod-dispatch` (a
    // coder on a hosted endpoint). Exactly one is meant to be live. Both
    // enabled is not a degraded run, it is two seats writing a mod for the
    // same finding, so it is refused at validate time, before a mint.

    /// Two enabled tasks naming each other's slot is an Error naming BOTH.
    #[test]
    fn two_enabled_tasks_that_exclude_each_other_are_a_validate_error() {
        let json = r#"{
          "id":"x","name":"X",
          "phases":[{"id":"p","tasks":[
            {"id":"seat-a","steps":[{"id":"a-step","kind":"procedural.noop","config":{}}]},
            {"id":"seat-b","excludes":["seat-a"],"steps":[{"id":"b-step","kind":"procedural.noop","config":{}}]}
          ]}]
        }"#;
        let cfg: MissionConfig = serde_json::from_str(json).unwrap();
        let findings = cfg.validate(&[]);
        let hit = findings
            .iter()
            .find(|f| f.severity == FindingSeverity::Error && f.path.ends_with("excludes"))
            .unwrap_or_else(|| panic!("expected an excludes error, got {findings:?}"));
        assert!(
            hit.message.contains("seat-a") && hit.message.contains("seat-b"),
            "the finding must name BOTH tasks so the operator knows which field to flip: {}",
            hit.message
        );
    }

    /// The shipped shape: one of the pair is `enabled: false`, which is the
    /// whole point of declaring the exclusion — no finding at all.
    #[test]
    fn an_exclusion_is_silent_when_only_one_of_the_pair_is_enabled() {
        let json = r#"{
          "id":"x","name":"X",
          "phases":[{"id":"p","tasks":[
            {"id":"seat-a","steps":[{"id":"a-step","kind":"procedural.noop","config":{}}]},
            {"id":"seat-b","enabled":false,"excludes":["seat-a"],"steps":[{"id":"b-step","kind":"procedural.noop","config":{}}]}
          ]}]
        }"#;
        let cfg: MissionConfig = serde_json::from_str(json).unwrap();
        assert!(cfg.validate(&[]).is_empty(), "{:?}", cfg.validate(&[]));
    }

    /// One pair, one finding — even when both sides declare the exclusion.
    /// A duplicated error would read as two separate defects.
    #[test]
    fn a_mutual_exclusion_declared_from_both_sides_reports_once() {
        let json = r#"{
          "id":"x","name":"X",
          "phases":[{"id":"p","tasks":[
            {"id":"seat-a","excludes":["seat-b"],"steps":[{"id":"a-step","kind":"procedural.noop","config":{}}]},
            {"id":"seat-b","excludes":["seat-a"],"steps":[{"id":"b-step","kind":"procedural.noop","config":{}}]}
          ]}]
        }"#;
        let cfg: MissionConfig = serde_json::from_str(json).unwrap();
        let errors: Vec<_> = cfg
            .validate(&[])
            .into_iter()
            .filter(|f| f.path.ends_with("excludes"))
            .collect();
        assert_eq!(errors.len(), 1, "one pair is one finding: {errors:?}");
    }

    /// A typo'd peer id is its own Error — an exclusion that names nothing
    /// protects nothing, and fails OPEN (both seats run) if left silent.
    #[test]
    fn excluding_an_unknown_task_id_is_an_error() {
        let json = r#"{
          "id":"x","name":"X",
          "phases":[{"id":"p","tasks":[
            {"id":"seat-a","excludes":["seat-typo"],"steps":[{"id":"a-step","kind":"procedural.noop","config":{}}]}
          ]}]
        }"#;
        let cfg: MissionConfig = serde_json::from_str(json).unwrap();
        let findings = cfg.validate(&[]);
        let hit = findings
            .iter()
            .find(|f| f.severity == FindingSeverity::Error && f.path.ends_with("excludes"))
            .unwrap_or_else(|| panic!("expected an unknown-id error, got {findings:?}"));
        assert!(hit.message.contains("seat-typo"), "names the id that resolves to nothing: {}", hit.message);
        // (#2310 P4f review, CONSIDER 2) Pin the MESSAGE, not just the
        // severity and path. Deleting this branch outright left this test
        // green: the both-enabled branch below emits an Error on the same
        // path whose text also happens to contain the typo'd id, so the
        // operator would be told "these two tasks are both enabled" about
        // a task that does not exist. The fail-open explanation is the
        // whole reason this is a separate finding.
        assert!(
            hit.message.contains("unknown task id") && hit.message.contains("fails OPEN"),
            "the finding must explain that an exclusion resolving to nothing protects nothing: {}",
            hit.message
        );
    }

    /// Self-exclusion is a nonsense document, not a task that disables
    /// itself.
    #[test]
    fn excluding_yourself_is_an_error() {
        let json = r#"{
          "id":"x","name":"X",
          "phases":[{"id":"p","tasks":[
            {"id":"seat-a","excludes":["seat-a"],"steps":[{"id":"a-step","kind":"procedural.noop","config":{}}]}
          ]}]
        }"#;
        let cfg: MissionConfig = serde_json::from_str(json).unwrap();
        let findings = cfg.validate(&[]);
        let hit = findings
            .iter()
            .find(|f| f.severity == FindingSeverity::Error && f.path.ends_with("excludes"))
            .unwrap_or_else(|| panic!("expected a self-exclusion error, got {findings:?}"));
        // (#2310 P4f review, CONSIDER 2) Same gap as the unknown-id test
        // above: severity + path alone were satisfied by the both-enabled
        // branch, so this branch was deletable while the test stayed
        // green. A self-exclusion must be named as one, and must point at
        // `enabled: false` as the way to actually turn a task off.
        assert!(
            hit.message.contains("excludes ITSELF") && hit.message.contains("enabled"),
            "the finding must name self-exclusion and point at the field that turns a task off: {}",
            hit.message
        );
    }

    /// A DISABLED PHASE prunes both seats, so there is no conflict to
    /// report — the check keys on what will actually be minted.
    #[test]
    fn an_exclusion_inside_a_disabled_phase_is_silent() {
        let json = r#"{
          "id":"x","name":"X",
          "phases":[{"id":"p","enabled":false,"tasks":[
            {"id":"seat-a","steps":[{"id":"a-step","kind":"procedural.noop","config":{}}]},
            {"id":"seat-b","excludes":["seat-a"],"steps":[{"id":"b-step","kind":"procedural.noop","config":{}}]}
          ]}]
        }"#;
        let cfg: MissionConfig = serde_json::from_str(json).unwrap();
        assert!(cfg.validate(&[]).is_empty(), "{:?}", cfg.validate(&[]));
    }

    /// (#2310 P4f review, CONSIDER 4) The disabled guard covers the
    /// CONFLICT only. A malformed exclusion — one naming no task at all —
    /// is an Error whether or not the declaring task is enabled, and that
    /// is deliberate rather than an oversight in the branch ordering: a
    /// typo inside a template that ships DISABLED is precisely the one
    /// nobody notices, because it costs nothing until the day an operator
    /// flips `enabled` and the exclusion silently protects nothing. Catch
    /// it while the document is being written, not on the launch that
    /// depends on it.
    #[test]
    fn a_disabled_task_excluding_an_unknown_id_is_still_an_error() {
        let json = r#"{
          "id":"x","name":"X",
          "phases":[{"id":"p","tasks":[
            {"id":"seat-a","steps":[{"id":"a-step","kind":"procedural.noop","config":{}}]},
            {"id":"seat-b","enabled":false,"excludes":["seat-typo"],"steps":[{"id":"b-step","kind":"procedural.noop","config":{}}]}
          ]}]
        }"#;
        let cfg: MissionConfig = serde_json::from_str(json).unwrap();
        let findings = cfg.validate(&[]);
        let hit = findings
            .iter()
            .find(|f| f.severity == FindingSeverity::Error && f.path.ends_with("excludes"))
            .unwrap_or_else(|| {
                panic!("a disabled task's typo'd exclusion is still a document defect: {findings:?}")
            });
        assert!(hit.message.contains("fails OPEN"), "{}", hit.message);
    }

    fn step(id: &str, kind: &str) -> StepConfig {
        StepConfig {
            enabled: None,
            id: id.to_string(),
            kind: kind.to_string(),
            config: serde_json::Value::Null,
            gate: None,
            extras: BTreeMap::new(),
        }
    }

    fn gated_step(id: &str, kind: &str, gate: &str) -> StepConfig {
        StepConfig { gate: Some(gate.to_string()), ..step(id, kind) }
    }

    fn task(id: &str, depends_on: &[&str], steps: Vec<StepConfig>) -> TaskConfig {
        TaskConfig {
            excludes: Vec::new(),
            enabled: None,
            id: id.to_string(),
            description: None,
            display_name: None,
            depends_on: depends_on.iter().map(|s| s.to_string()).collect(),
            reads: Vec::new(),
            role_id: None,
            run_on: None,
            steps,
            grow: None,
            extras: BTreeMap::new(),
        }
    }

    fn phase(id: &str, tasks: Vec<TaskConfig>) -> PhaseConfig {
        PhaseConfig {
            enabled: None,
            id: id.to_string(),
            description: None,
            display_name: None,
            tasks,
            extras: BTreeMap::new(),
        }
    }

    fn doc(phases: Vec<PhaseConfig>) -> MissionConfig {
        MissionConfig {
            id: "m".to_string(),
            name: "M".to_string(),
            description: None,
            schema_version: Some(MISSION_CONFIG_SCHEMA.to_string()),
            inputs: Vec::new(),
            phases,
            panel: None,
            cmd: None,
            outcome_from: None,
            extras: BTreeMap::new(),
        }
    }

    // ── (#1685) check_cmd ────────────────────────────────────────

    #[test]
    fn check_cmd_is_a_no_op_when_the_config_declares_none() {
        let cfg = doc(vec![]);
        assert!(cfg.cmd.is_none());
        assert!(check_cmd(&cfg).is_none(), "a config with no cmd never gets blocked");
    }

    #[test]
    fn a_stale_gh_verb_key_is_a_loud_error_not_a_silent_ungating() {
        // (#2004) The rename's whole safety property. `gh_verb` overflows into
        // `extras` and is inert — and this gate FAILS OPEN, so a config left on
        // the old name loses its allowlist check entirely and the shell-out it
        // was protecting runs as if the operator had approved it. Validation is
        // the only thing standing between a rename and a silently ungated
        // `pr-merge`, so it is asserted here rather than trusted.
        let mut cfg = doc(vec![]);
        cfg.id = "pr-merge".into();
        cfg.extras.insert("gh_verb".into(), serde_json::json!("pr-merge"));

        let findings = cfg.validate(&[]);
        let hit = findings
            .iter()
            .find(|f| f.path == "gh_verb")
            .expect("a document still declaring gh_verb must be flagged");
        assert_eq!(
            hit.severity,
            FindingSeverity::Error,
            "a silently-ungated config is an Error, never a Warning"
        );
        assert!(hit.message.contains("cmd"), "the finding must name the new field: {}", hit.message);
        assert!(
            hit.message.contains("WITHOUT the allowlist gate"),
            "the finding must state the CONSEQUENCE, not just the rename: {}",
            hit.message
        );

        // And the gate itself still reads as ungated — which is exactly why
        // the validation above has to exist.
        assert!(
            check_cmd(&cfg).is_none(),
            "a stale key must NOT accidentally gate; if this ever passes, the \
             validation finding is no longer load-bearing and this test is lying"
        );
    }

    #[test]
    #[serial_test::serial]
    fn check_cmd_blocks_when_the_gate_is_off() {
        let mut cfg = doc(vec![]);
        cfg.cmd = Some("pr-merge".to_string());
        let prev_enabled = std::env::var("DARKMUX_CMD_ENABLED").ok();
        let prev_allowed = std::env::var("DARKMUX_CMD_ALLOWED").ok();
        // Neither env override set, and the test-support config tier is
        // empty by construction — config_access falls to its built-in
        // `false`/empty defaults.
        unsafe {
            std::env::remove_var("DARKMUX_CMD_ENABLED");
            std::env::remove_var("DARKMUX_CMD_ALLOWED");
        }
        let reason = check_cmd(&cfg).expect("must be blocked with the gate off");
        assert!(reason.contains("pr-merge"), "{reason}");
        assert!(reason.contains("cmd.enabled"), "{reason}");
        unsafe {
            match prev_enabled {
                Some(v) => std::env::set_var("DARKMUX_CMD_ENABLED", v),
                None => std::env::remove_var("DARKMUX_CMD_ENABLED"),
            }
            match prev_allowed {
                Some(v) => std::env::set_var("DARKMUX_CMD_ALLOWED", v),
                None => std::env::remove_var("DARKMUX_CMD_ALLOWED"),
            }
        }
    }

    #[test]
    #[serial_test::serial]
    fn check_cmd_allows_when_enabled_and_named() {
        let mut cfg = doc(vec![]);
        cfg.cmd = Some("pr-merge".to_string());
        let prev_enabled = std::env::var("DARKMUX_CMD_ENABLED").ok();
        let prev_allowed = std::env::var("DARKMUX_CMD_ALLOWED").ok();
        unsafe {
            std::env::set_var("DARKMUX_CMD_ENABLED", "true");
            std::env::set_var("DARKMUX_CMD_ALLOWED", "pr-list,pr-merge");
        }
        assert!(check_cmd(&cfg).is_none(), "enabled + named must pass");
        unsafe {
            match prev_enabled {
                Some(v) => std::env::set_var("DARKMUX_CMD_ENABLED", v),
                None => std::env::remove_var("DARKMUX_CMD_ENABLED"),
            }
            match prev_allowed {
                Some(v) => std::env::set_var("DARKMUX_CMD_ALLOWED", v),
                None => std::env::remove_var("DARKMUX_CMD_ALLOWED"),
            }
        }
    }

    #[test]
    #[serial_test::serial]
    fn check_cmd_blocks_a_verb_not_named_even_when_enabled() {
        let mut cfg = doc(vec![]);
        cfg.cmd = Some("pr-merge".to_string());
        let prev_enabled = std::env::var("DARKMUX_CMD_ENABLED").ok();
        let prev_allowed = std::env::var("DARKMUX_CMD_ALLOWED").ok();
        unsafe {
            std::env::set_var("DARKMUX_CMD_ENABLED", "true");
            std::env::set_var("DARKMUX_CMD_ALLOWED", "pr-list,pr-info");
        }
        let reason = check_cmd(&cfg).expect("pr-merge is not in the allowlist");
        assert!(reason.contains("pr-merge"), "{reason}");
        unsafe {
            match prev_enabled {
                Some(v) => std::env::set_var("DARKMUX_CMD_ENABLED", v),
                None => std::env::remove_var("DARKMUX_CMD_ENABLED"),
            }
            match prev_allowed {
                Some(v) => std::env::set_var("DARKMUX_CMD_ALLOWED", v),
                None => std::env::remove_var("DARKMUX_CMD_ALLOWED"),
            }
        }
    }

    #[test]
    fn phase_with_zero_tasks_is_valid() {
        let cfg = doc(vec![phase("wait", vec![])]);
        assert!(cfg.is_valid(&[]));
    }

    #[test]
    fn dangling_depends_on_is_caught() {
        let cfg = doc(vec![phase(
            "p1",
            vec![task("t1", &["nonexistent"], vec![step("s1", "dispatch.internal")])],
        )]);
        let findings = cfg.validate(&["dispatch.internal"]);
        assert!(
            findings.iter().any(|f| f.severity == FindingSeverity::Error
                && f.message.contains("nonexistent")),
            "expected a dangling depends_on error, got {findings:?}"
        );
    }

    #[test]
    fn cross_phase_depends_on_resolves_cleanly() {
        // A later phase's task depending on an earlier phase's task is
        // exactly `build_review_graph`'s synthesis→dedup shape — must NOT
        // be flagged as dangling.
        let cfg = doc(vec![
            phase("p1", vec![task("t1", &[], vec![step("s1", "dispatch.internal")])]),
            phase("p2", vec![task("t2", &["t1"], vec![step("s2", "dispatch.internal")])]),
        ]);
        assert!(cfg.is_valid(&["dispatch.internal"]));
    }

    /// (#2310 fix-loop C2 / C2-1) A FORWARD edge — a task depending on one
    /// declared in a LATER phase — is legal (ids are document-wide, and
    /// every scheduler pass runs over the cumulative maps, so the later
    /// phase's pass runs it), but it is surprising enough to name:
    /// the declaring phase cannot finish within its own pass. Warning,
    /// never Error — pre-1.0 documents may rely on the shape.
    #[test]
    fn forward_depends_on_is_warned_not_refused() {
        let cfg = doc(vec![
            phase("p1", vec![task("t-forward", &["t-late"], vec![step("s1", "dispatch.internal")])]),
            phase("p2", vec![task("t-late", &[], vec![step("s2", "dispatch.internal")])]),
        ]);
        let findings = cfg.validate(&["dispatch.internal"]);
        assert!(
            findings.iter().any(|f| f.severity == FindingSeverity::Warning
                && f.message.contains("t-late")
                && f.message.contains("later phase")),
            "expected a forward-dependency warning, got {findings:?}"
        );
        assert!(cfg.is_valid(&["dispatch.internal"]), "a forward edge must stay LEGAL");
    }

    /// The same shape through `reads`, which orders execution identically.
    #[test]
    fn forward_reads_is_warned_too() {
        let mut forward = task("t-forward", &[], vec![step("s1", "dispatch.internal")]);
        forward.reads = vec!["t-late".to_string()];
        let cfg = doc(vec![
            phase("p1", vec![forward]),
            phase("p2", vec![task("t-late", &[], vec![step("s2", "dispatch.internal")])]),
        ]);
        let findings = cfg.validate(&["dispatch.internal"]);
        assert!(
            findings.iter().any(|f| f.severity == FindingSeverity::Warning
                && f.path.ends_with(".reads")
                && f.message.contains("later phase")),
            "expected a forward-reads warning, got {findings:?}"
        );
    }

    /// A BACKWARD edge (the ordinary shape) stays silent — no warning.
    #[test]
    fn a_backward_depends_on_warns_about_nothing() {
        let cfg = doc(vec![
            phase("p1", vec![task("t1", &[], vec![step("s1", "dispatch.internal")])]),
            phase("p2", vec![task("t2", &["t1"], vec![step("s2", "dispatch.internal")])]),
        ]);
        let findings = cfg.validate(&["dispatch.internal"]);
        assert!(
            !findings.iter().any(|f| f.message.contains("later phase")),
            "got {findings:?}"
        );
    }

    #[test]
    fn self_referential_depends_on_is_caught() {
        let cfg = doc(vec![phase(
            "p1",
            vec![task("t1", &["t1"], vec![step("s1", "dispatch.internal")])],
        )]);
        let findings = cfg.validate(&["dispatch.internal"]);
        assert!(findings
            .iter()
            .any(|f| f.severity == FindingSeverity::Error && f.message.contains("depends_on itself")));
    }

    #[test]
    fn dangling_and_self_referential_reads_are_caught() {
        // (#1619) `reads` orders execution exactly like `depends_on`, so a
        // dangling or self-referential entry gets the SAME error tier —
        // failing at validate, never hanging at run.
        let mut t = task("t1", &[], vec![step("s1", "dispatch.internal")]);
        t.reads = vec!["nonexistent".to_string()];
        let cfg = doc(vec![phase("p1", vec![t])]);
        let findings = cfg.validate(&["dispatch.internal"]);
        assert!(
            findings.iter().any(|f| f.severity == FindingSeverity::Error
                && f.message.contains("reads unknown task id \"nonexistent\"")),
            "expected a dangling reads error, got {findings:?}"
        );

        let mut t = task("t1", &[], vec![step("s1", "dispatch.internal")]);
        t.reads = vec!["t1".to_string()];
        let cfg = doc(vec![phase("p1", vec![t])]);
        let findings = cfg.validate(&["dispatch.internal"]);
        assert!(
            findings
                .iter()
                .any(|f| f.severity == FindingSeverity::Error && f.message.contains("reads itself")),
            "expected a self-read error, got {findings:?}"
        );
    }

    #[test]
    fn a_config_without_reads_still_parses_and_a_reads_config_round_trips() {
        // (#1619, contract 5) Additive minor: every pre-1.4 document omits
        // `reads` and must parse identically; a document that declares it
        // must survive a serialize→parse round trip without losing it.
        let mut t = task("t2", &[], vec![step("s2", "dispatch.internal")]);
        t.reads = vec!["t1".to_string()];
        let cfg = doc(vec![
            phase("p1", vec![task("t1", &[], vec![step("s1", "dispatch.internal")])]),
            phase("p2", vec![t]),
        ]);
        let json = serde_json::to_string(&cfg).unwrap();
        let back: MissionConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(back.phases[1].tasks[0].reads, vec!["t1"]);
        assert!(back.is_valid(&["dispatch.internal"]));

        // The omitting shape: strip the field wholesale, still parses, reads
        // defaults empty.
        let stripped = json.replace("\"reads\":[\"t1\"],", "").replace(",\"reads\":[\"t1\"]", "");
        assert!(!stripped.contains("reads"), "precondition: field removed");
        let old: MissionConfig = serde_json::from_str(&stripped).unwrap();
        assert!(old.phases[1].tasks[0].reads.is_empty());
    }

    #[test]
    fn duplicate_task_id_is_caught() {
        let cfg = doc(vec![phase(
            "p1",
            vec![
                task("dup", &[], vec![step("s1", "dispatch.internal")]),
                task("dup", &[], vec![step("s2", "dispatch.internal")]),
            ],
        )]);
        let findings = cfg.validate(&["dispatch.internal"]);
        assert!(findings
            .iter()
            .any(|f| f.severity == FindingSeverity::Error && f.message.contains("duplicate task id")));
    }

    #[test]
    fn duplicate_phase_id_is_caught() {
        let cfg = doc(vec![phase("dup", vec![]), phase("dup", vec![])]);
        let findings = cfg.validate(&[]);
        assert!(findings
            .iter()
            .any(|f| f.severity == FindingSeverity::Error && f.message.contains("duplicate phase id")));
    }

    #[test]
    fn unknown_step_kind_is_a_warning_not_an_error() {
        let cfg = doc(vec![phase(
            "p1",
            vec![task("t1", &[], vec![step("s1", "review.bundle")])],
        )]);
        // Only Tier 1 kinds known to this call site — "review.bundle" is a
        // real Tier 3 kind, but this check can't see it.
        let findings = cfg.validate(&["dispatch.internal", "procedural.shell"]);
        assert!(findings
            .iter()
            .any(|f| f.severity == FindingSeverity::Warning && f.message.contains("review.bundle")));
        assert!(!findings.iter().any(|f| f.severity == FindingSeverity::Error));
        assert!(cfg.is_valid(&["dispatch.internal", "procedural.shell"]));
    }

    #[test]
    fn empty_known_kinds_skips_the_kind_check_entirely() {
        let cfg = doc(vec![phase(
            "p1",
            vec![task("t1", &[], vec![step("s1", "anything.at.all")])],
        )]);
        assert!(cfg.validate(&[]).is_empty());
    }

    #[test]
    fn empty_step_kind_is_an_error() {
        let cfg = doc(vec![phase("p1", vec![task("t1", &[], vec![step("s1", "")])])]);
        let findings = cfg.validate(&[]);
        assert!(findings
            .iter()
            .any(|f| f.severity == FindingSeverity::Error && f.path.ends_with(".kind")));
    }

    #[test]
    fn finding_display_renders_level_path_message() {
        let f = ValidationFinding {
            severity: FindingSeverity::Warning,
            path: "phases[0].id".to_string(),
            message: "test message".to_string(),
        };
        assert_eq!(f.to_string(), "[warning] phases[0].id: test message");
    }

    /// (#swarm-5) A task with zero steps can never reach Complete (a task
    /// derives completion from its steps), so it wedges the mission graph
    /// forever with nothing erroring. That is a composition mistake and must
    /// fail HERE — at validate/doctor time — not surface as a frozen graph
    /// lens mid-run. (#1550 cluster item 2: the EXPANDING-task exemption
    /// this test used to also pin was retired along with the expansion
    /// primitive — every task's steps are real from the document now, so
    /// there's no "materializes per-item at interpret time" case left to
    /// exempt.)
    #[test]
    fn zero_step_task_is_an_error() {
        let cfg = doc(vec![phase("p1", vec![task("hollow", &[], vec![])])]);
        let findings = cfg.validate(&[]);
        assert!(
            findings.iter().any(|f| f.severity == FindingSeverity::Error
                && f.path.ends_with(".steps")
                && f.message.contains("hollow")),
            "expected a zero-steps error, got {findings:?}"
        );

        // The same shape WITH a step stays clean — the error is about
        // emptiness, not about this task's other properties.
        let ok = doc(vec![phase("p1", vec![task("solid", &[], vec![step("s1", "dispatch.internal")])])]);
        assert!(
            !ok.validate(&[]).iter().any(|f| f.severity == FindingSeverity::Error),
            "a one-step task must not trip the zero-step error"
        );
    }

    // ── (#2310 P4a review M3/C1) `run_on` validation ─────────────────────

    #[test]
    fn run_on_unknown_value_is_an_error() {
        let mut t = task("t", &[], vec![step("s1", "dispatch.internal")]);
        t.run_on = Some(vec!["complete".to_string(), "maybe".to_string()]);
        let cfg = doc(vec![phase("p1", vec![t])]);
        let findings = cfg.validate(&[]);
        let f = findings
            .iter()
            .find(|f| f.path.ends_with(".run_on") && f.message.contains("maybe"))
            .unwrap_or_else(|| panic!("expected a run_on unknown-value finding, got {findings:?}"));
        assert_eq!(f.severity, FindingSeverity::Error);
    }

    #[test]
    fn run_on_empty_list_is_an_error() {
        let mut t = task("t", &[], vec![step("s1", "dispatch.internal")]);
        t.run_on = Some(Vec::new());
        let cfg = doc(vec![phase("p1", vec![t])]);
        let findings = cfg.validate(&[]);
        let f = findings
            .iter()
            .find(|f| f.path.ends_with(".run_on") && f.severity == FindingSeverity::Error)
            .unwrap_or_else(|| panic!("expected an empty-run_on error finding, got {findings:?}"));
        assert!(f.message.contains("wedge") || f.message.contains("empty"), "{}", f.message);
    }

    #[test]
    fn run_on_absent_or_default_validates_with_no_run_on_finding() {
        let no_field = task("t1", &[], vec![step("s1", "dispatch.internal")]);
        let mut explicit_default = task("t2", &[], vec![step("s2", "dispatch.internal")]);
        explicit_default.run_on = Some(vec!["complete".to_string()]);
        let cfg = doc(vec![phase("p1", vec![no_field, explicit_default])]);
        let findings = cfg.validate(&[]);
        assert!(
            !findings.iter().any(|f| f.path.ends_with(".run_on")),
            "neither an absent run_on nor an explicit [\"complete\"] should produce a finding: {findings:?}"
        );
    }

    #[test]
    fn run_on_omitting_complete_is_a_warning_not_an_error() {
        // (C1) `run_on: ["error"]` alone is a legal but likely-mistaken
        // declaration — the task never becomes ready on an ordinary clean
        // run (every dependency reaches Complete, which this run_on
        // doesn't accept). Warning, never Error — a cleanup/notify task
        // that intentionally runs ONLY after a failure is a real,
        // legitimate shape.
        let mut t = task("t", &[], vec![step("s1", "dispatch.internal")]);
        t.run_on = Some(vec!["error".to_string()]);
        let cfg = doc(vec![phase("p1", vec![t])]);
        let findings = cfg.validate(&[]);
        let f = findings
            .iter()
            .find(|f| f.path.ends_with(".run_on"))
            .unwrap_or_else(|| panic!("expected a run_on omits-complete finding, got {findings:?}"));
        assert_eq!(f.severity, FindingSeverity::Warning, "{f:?}");
        assert!(f.message.contains("complete"), "{}", f.message);
        // is_valid (Errors only) stays true — a Warning never blocks usability.
        assert!(cfg.is_valid(&[]));
    }

    #[test]
    fn run_on_including_complete_alongside_error_is_not_a_warning() {
        let mut t = task("t", &[], vec![step("s1", "dispatch.internal")]);
        t.run_on = Some(vec!["complete".to_string(), "error".to_string()]);
        let cfg = doc(vec![phase("p1", vec![t])]);
        let findings = cfg.validate(&[]);
        assert!(
            !findings.iter().any(|f| f.path.ends_with(".run_on")),
            "run_on: [\"complete\", \"error\"] must not trip the omits-complete warning: {findings:?}"
        );
    }


    // (#1550 cluster item 2) The `ExpansionSpec` validation tests
    // (`expanding_task_with`, `expand_with_empty_over_is_an_error`,
    // `expand_patterns_without_index_or_name_are_errors`,
    // `expand_template_with_more_than_one_step_is_an_error`,
    // `well_formed_expand_block_validates_clean`) were removed here along
    // with the expansion primitive itself — see `MISSION_CONFIG_SCHEMA`'s
    // doc (schema 2.0) for why.

    /// (#1550 cluster item 2, operator correction 2) RED case: a document
    /// still declaring `expand` on a task parses cleanly (contract 7 —
    /// `extras` swallows the unknown key) but the key is now DEAD — schema
    /// 2.0 removed the field entirely, and unlike an additive-minor-ahead
    /// document (#1648, where ignoring an unknown field is at least
    /// arguably safe for a FUTURE schema), there is no future schema where
    /// `expand` means anything again. Silence here would let an operator's
    /// config keep "declaring" a fan-out that never happens. Must be an
    /// Error, naming the field, the schema version it was removed in, and
    /// what to do instead.
    #[test]
    fn a_task_still_declaring_expand_is_a_loud_validate_error() {
        let json = r#"{"id":"m","name":"M","phases":[{"id":"p1","tasks":[
            {"id":"t1","steps":[{"id":"s1","kind":"dispatch.internal"}],
             "expand":{"over":"items","task_id_pattern":"t-{index}","step_id_pattern":"s-{index}"}}
        ]}]}"#;
        let cfg: MissionConfig = serde_json::from_str(json).unwrap();
        // Parses cleanly — `expand` overflows into extras (contract 7).
        assert!(cfg.phases[0].tasks[0].extras.contains_key("expand"));

        let findings = cfg.validate(&["dispatch.internal"]);
        let err = findings
            .iter()
            .find(|f| f.severity == FindingSeverity::Error && f.path.ends_with(".expand"))
            .unwrap_or_else(|| panic!("expected an `expand`-removed error, got {findings:?}"));
        assert!(err.message.contains("t1"), "names the task: {}", err.message);
        assert!(
            err.message.contains("REMOVED in schema 2.0"),
            "names the removal + version: {}",
            err.message
        );
        assert!(
            err.message.contains("Declare the expanded tasks explicitly instead"),
            "says what to do instead: {}",
            err.message
        );
    }

    #[test]
    fn a_task_without_expand_in_extras_does_not_trip_the_removed_field_error() {
        let cfg = doc(vec![phase("p1", vec![task("t1", &[], vec![step("s1", "dispatch.internal")])])]);
        assert!(
            !cfg.validate(&["dispatch.internal"]).iter().any(|f| f.path.ends_with(".expand")),
            "a document with no `expand` key must never trip this check"
        );

        // (#1550 QA finding) An empty-extras task alone would not catch a
        // guard that fired on ANY extras key rather than on `expand`
        // specifically. Parsed through serde so the extras map is populated
        // the way production populates it — `notes` is a real key the
        // built-in review config carries on two tasks, so this is the shape
        // that would actually regress.
        let with_other_extras: MissionConfig = serde_json::from_str(
            r#"{
                "id": "t", "name": "T",
                "phases": [{ "id": "p1", "tasks": [{
                    "id": "t1",
                    "notes": "an ordinary extras key, not a removed field",
                    "steps": [{ "id": "s1", "kind": "dispatch.internal" }]
                }]}]
            }"#,
        )
        .expect("parses");
        assert!(
            !with_other_extras.phases[0].tasks[0].extras.is_empty(),
            "precondition: the task must actually carry an extras key"
        );
        assert!(
            !with_other_extras
                .validate(&["dispatch.internal"])
                .iter()
                .any(|f| f.path.ends_with(".expand")),
            "the check must key on `expand` specifically, never on extras being non-empty"
        );
    }

    // ─── Built-in config golden-shape tests (#1284 Packet 1) ──────────
    // Mirrors `workloads::types::tests::
    // medium_coding_embedded_manifest_has_expected_shape`'s style: no
    // separate golden-file mechanism exists in this codebase — "golden"
    // means the expected shape is locked in code. These parse the EMBEDDED
    // constants directly (`load::find_embedded`), NOT through `load::load`'s
    // user → on-disk → embedded chain (#1284 review round 1): they are
    // goldens for the embedded documents, and going through the chain both
    // tested the wrong thing and was unisolated — it raced the loader's
    // `#[serial]` tests that write user-tier stubs (serial_test does not
    // block non-serial tests), and would deterministically read a real
    // operator override at `~/.darkmux/mission-configs/<id>.json`. Chain
    // resolution of the embedded tier keeps its own `#[serial]` test in
    // `load::tests::embedded_resolves_with_no_user_or_on_disk_copy`.

    fn embedded_config(id: &str) -> MissionConfig {
        let raw = load::find_embedded(id)
            .unwrap_or_else(|| panic!("`{id}` must be in EMBEDDED_MISSION_CONFIGS"));
        serde_json::from_str(raw).unwrap_or_else(|e| panic!("embedded `{id}` must parse: {e}"))
    }

    fn known_kinds() -> Vec<String> {
        crate::step_kinds::StepKindRegistry::with_builtins().ids()
    }

    fn known_kinds_refs(known: &[String]) -> Vec<&str> {
        known.iter().map(String::as_str).collect()
    }

    /// (#2310 fix-loop C2 / C2-4) The built-in that SUBSCRIBES to the
    /// exit-1-on-delivery-failure rule declares it, and names a real,
    /// non-`grow`, enabled task — the three shapes `outcome_from`
    /// validation refuses. `crawl` deliberately declares nothing: its
    /// `summary` totals a run rather than delivering one.
    #[test]
    fn review_builtin_declares_its_delivering_task_and_crawl_does_not() {
        let cfg = embedded_config("review");
        assert_eq!(cfg.outcome_from.as_deref(), Some("deliver"));
        let deliver = cfg
            .phases
            .iter()
            .flat_map(|p| p.tasks.iter())
            .find(|t| t.id == "deliver")
            .expect("review declares a `deliver` task");
        assert!(deliver.grow.is_none(), "outcome_from may not name a grow template");
        assert!(deliver.is_enabled(), "outcome_from may not name a disabled task");
        let known = known_kinds();
        assert!(
            cfg.is_valid(&known_kinds_refs(&known)),
            "{:?}",
            cfg.validate(&known_kinds_refs(&known))
        );

        assert_eq!(
            embedded_config("crawl").outcome_from,
            None,
            "an errored crawl SUMMARY is a lost tally, not a review that never shipped"
        );
    }

    #[test]
    fn review_builtin_has_the_expected_graph_shape() {
        // (#2310 P4d) `review` is now a config on the crawl's shared blocks
        // — the funnel document (investigate/adjudicate/report over ten
        // Tier-3 `review.*` kinds) and its bespoke launcher are deleted.
        let cfg = &embedded_config("review");
        assert_eq!(cfg.id, "review");
        assert!(!cfg.inputs.is_empty(), "review declares its runtime inputs");
        // The workflow's frozen param names all stay declared, so
        // `.github/workflows/darkmux-review.yml` runs unchanged.
        for frozen in ["github", "head_sha", "diff_file", "intent_file", "mode", "bundler", "envelope_out", "emit"] {
            assert!(
                cfg.inputs.iter().any(|i| i.name == frozen),
                "the workflow's frozen `--param {frozen}=` must stay declared"
            );
        }
        // Every declared input is either referenced by the graph or marked
        // ignored — the launch-side guard refuses a config that declares an
        // input nothing consumes.
        let body = serde_json::to_string(&cfg.phases).expect("phases serialize");
        for input in &cfg.inputs {
            let referenced = body.contains(&format!("{{{{{}}}}}", input.name));
            let ignored = input.ignored.unwrap_or(false);
            // `rules`/`dry_run` are launcher-consumed (task pruning, the CLI
            // flag), the same way `crawl.json` declares them.
            let launcher_consumed = matches!(input.name.as_str(), "rules" | "dry_run");
            assert!(
                referenced || ignored || launcher_consumed,
                "input `{}` is declared but nothing references it and it is not marked ignored",
                input.name
            );
        }

        let phase_ids: Vec<&str> = cfg.phases.iter().map(|p| p.id.as_str()).collect();
        assert_eq!(phase_ids, vec!["plan", "review", "summarize", "create-mods", "deliver"]);
        // The plan phase is diff-scoped `plan.sites`, one task per rule.
        let plan = &cfg.phases[0];
        assert!(!plan.tasks.is_empty(), "one plan task per rule");
        for t in &plan.tasks {
            assert_eq!(t.steps[0].kind, "plan.sites", "task {}", t.id);
        }
        assert_eq!(cfg.outcome_from.as_deref(), Some("deliver"));
    }

    #[test]
    fn coder_phase_builtin_has_the_expected_graph_shape() {
        let cfg = &embedded_config("coder-phase");
        assert_eq!(cfg.id, "coder-phase");
        assert!(
            cfg.inputs.iter().any(|i| i.name == "workdir"),
            "workdir must be declared as a runtime-only input, not a TaskConfig field"
        );

        assert_eq!(cfg.phases.len(), 1);
        let phase = &cfg.phases[0];
        assert_eq!(phase.id, "build");

        // Task ids match `default_phase_graph`'s exact construction
        // (`{phase_id}-worktree` / `-coder` / `-verify` — NO `-task`
        // suffix), with the literal `build-` prefix standing in for the
        // launcher-composed phase id per the placeholder-prefix rule (see
        // `TaskConfig`'s doc). #1284 review round 1 caught the original
        // `-task`-suffixed divergence — persisted Task.ids surface in
        // `mission status`, the viewer, and lifecycle records, so the
        // config must not silently change them at Packet 3 cutover.
        let task_ids: Vec<&str> = phase.tasks.iter().map(|t| t.id.as_str()).collect();
        assert_eq!(task_ids, vec!["build-worktree", "build-coder", "build-verify"]);

        assert!(phase.tasks[0].depends_on.is_empty());
        assert_eq!(phase.tasks[1].depends_on, vec!["build-worktree"]);
        assert_eq!(phase.tasks[2].depends_on, vec!["build-coder"]);

        // Step ids likewise match the Rust scheme: `{phase_id}-<name>-step`.
        assert_eq!(phase.tasks[0].steps[0].id, "build-worktree-step");
        assert_eq!(phase.tasks[1].steps[0].id, "build-coder-step");
        assert_eq!(phase.tasks[2].steps[0].id, "build-verify-step");

        assert_eq!(phase.tasks[0].steps[0].kind, "mission.worktree");
        assert_eq!(phase.tasks[1].steps[0].kind, "mission.coder");
        assert_eq!(phase.tasks[2].steps[0].kind, "mission.verify");

        assert_eq!(phase.tasks[1].role_id.as_deref(), Some("coder"));
        assert_eq!(phase.tasks[2].role_id.as_deref(), Some("code-reviewer"));

        // The coder task's description matches the Rust builder's dynamic
        // form with the default role substituted (`dispatch `{role}` into
        // the worktree`) — the `role` input overrides both at launch.
        assert_eq!(
            phase.tasks[1].description.as_deref(),
            Some("dispatch `coder` into the worktree")
        );
    }

    #[test]
    fn both_builtins_validate_with_zero_error_findings() {
        let known = known_kinds();
        let known_refs = known_kinds_refs(&known);
        for id in ["review", "coder-phase"] {
            let cfg = embedded_config(id);
            let findings = cfg.validate(&known_refs);
            let errors: Vec<&ValidationFinding> = findings
                .iter()
                .filter(|f| f.severity == FindingSeverity::Error)
                .collect();
            assert!(
                errors.is_empty(),
                "`{id}` must validate with zero Error findings, got: {errors:?}"
            );
            assert!(
                cfg.is_valid(&known_refs),
                "`{id}` must report is_valid() true (Warnings don't block usability)"
            );
        }
    }

    /// (#2310 P4e review, MUST FIX) The embedded-optional warning's own
    /// premise is "whenever the operator leaves that input unset,
    /// `check_embedded_inputs_collected` refuses the launch outright". A
    /// DEFAULTED input is never unset: `mission_launch::
    /// apply_input_defaults` collects it on every launch before either
    /// placeholder pass runs, so the placeholder always resolves. Warning
    /// on it told the operator to make a genuinely-optional input
    /// required, on every `mission config show review` and every doctor
    /// pass, about a document that is correct.
    ///
    /// Red-proved by restoring the `input.required != Some(true)`
    /// condition without the `default.is_none()` half.
    #[test]
    fn a_defaulted_embedded_input_does_not_warn() {
        let known = known_kinds();
        let known_refs = known_kinds_refs(&known);
        for id in ["review", "crawl"] {
            let cfg = embedded_config(id);
            let findings = cfg.validate(&known_refs);
            let input_findings: Vec<&ValidationFinding> =
                findings.iter().filter(|f| f.path == "inputs").collect();
            assert!(
                input_findings.is_empty(),
                "`{id}` must validate with zero findings on the inputs path, got: {input_findings:?}"
            );
            let errors: Vec<&ValidationFinding> = findings
                .iter()
                .filter(|f| f.severity == FindingSeverity::Error)
                .collect();
            assert!(errors.is_empty(), "`{id}` must validate with zero Error findings, got: {errors:?}");
        }
    }

    #[test]
    fn both_builtins_reference_only_tier_3_kinds_unknown_to_tier_1() {
        // Every real step kind in both built-ins is Tier 3 (`review.*` /
        // `mission.*`) — none collide with the four Tier 1 builtins, so
        // validating against ONLY `with_builtins()`'s ids should warn on
        // every step (the doctor-visible "can't see Tier 3" case this
        // packet's doctor check documents), never silently pass them as
        // "known".
        let known = known_kinds();
        let known_refs = known_kinds_refs(&known);
        for id in ["review", "coder-phase"] {
            let cfg = embedded_config(id);
            let findings = cfg.validate(&known_refs);
            let warnings: Vec<&ValidationFinding> = findings
                .iter()
                .filter(|f| f.severity == FindingSeverity::Warning && f.path.ends_with(".kind"))
                .collect();
            assert!(
                !warnings.is_empty(),
                "`{id}` should warn on its Tier 3 kinds against a Tier-1-only known set"
            );
        }
    }

    /// (#2310 P4f review, MUST FIX) EVERY embedded built-in declares the
    /// CURRENT `MISSION_CONFIG_SCHEMA`, enumerated from the embedded set
    /// itself rather than from a list written by hand.
    ///
    /// This replaces three per-config assertions that lived inside three
    /// separate graph-shape tests. That arrangement covered `review`,
    /// `coder-phase` and `crawl` and silently did NOT cover `review` —
    /// which is the config this packet edited: mutating `review.json`'s
    /// declared version to a stale `"3.4"` left the whole suite green,
    /// while the identical mutation on `crawl.json` went red. A per-config
    /// pin only ever covers the configs somebody remembered to write one
    /// for, and the one nobody remembered is exactly the one drifting. A
    /// Every config inherits this check by existing.
    ///
    /// A declared version is not cosmetic: `validate` warns on a
    /// major-version mismatch, and the version is the only signal an older
    /// binary reading a newer document has.
    #[test]
    fn every_embedded_builtin_declares_the_current_schema_version() {
        let embedded = load::embedded_all();
        // (#2310 P4d) THREE built-ins: the funnel `review` document retired
        // and its replacement took the `review` id, so the set is
        // coder-phase + crawl + review.
        assert!(embedded.len() >= 3, "the embedded set should not have shrunk: {embedded:?}");
        for (id, _) in embedded {
            let cfg = embedded_config(id);
            assert_eq!(
                cfg.schema_version.as_deref(),
                Some(MISSION_CONFIG_SCHEMA),
                "embedded `{id}` declares `{:?}`, but the current schema is `{MISSION_CONFIG_SCHEMA}` \
                 — every built-in ships on the current version",
                cfg.schema_version
            );
        }
    }

    #[test]
    fn both_builtins_round_trip_through_json() {
        for id in ["review", "coder-phase"] {
            let cfg = embedded_config(id);
            let json = serde_json::to_string(&cfg).unwrap();
            let back: MissionConfig = serde_json::from_str(&json).unwrap();
            assert_eq!(cfg, back, "`{id}` must round-trip through JSON unchanged");
        }
    }

    // ─── crawl (#2301): the document IS the crawl ──────────────────────
    // Kept out of `both_builtins_*` above because those loops assert the
    // TWO dedicated-launcher configs' invariants; crawl's own goldens
    // assert its per-rule track structure, which neither of those has.

    #[test]
    fn crawl_builtin_validates_with_zero_error_findings_and_plans_one_task_per_rule() {
        let known = known_kinds();
        let known_refs = known_kinds_refs(&known);
        let cfg = embedded_config("crawl");
        assert_eq!(cfg.id, "crawl");
        // (#2298 + #2301) Three phases: a `crawl.plan` task per built-in
        // rule, a `crawl.unit` GROW template per rule growing from that
        // rule's own plan task, and one `crawl.summary`.
        let phase_ids: Vec<&str> = cfg.phases.iter().map(|p| p.id.as_str()).collect();
        assert_eq!(phase_ids, vec!["plan", "crawl", "summarize", "create-mods"], "{phase_ids:?}");
        const RULES: [&str; 4] =
            ["unnamed-predicate", "swallowed-error", "doc-contradicts-code", "stale-consumer"];

        let plan = &cfg.phases[0];
        let rules: Vec<&str> = plan
            .tasks
            .iter()
            .map(|t| t.steps[0].config["rule"].as_str().expect("each plan step names its rule"))
            .collect();
        assert_eq!(rules, RULES.to_vec());
        assert!(plan.tasks.iter().all(|t| t.steps.len() == 1 && t.steps[0].kind == "crawl.plan"));

        let crawl = &cfg.phases[1];
        assert_eq!(crawl.tasks.len(), RULES.len());
        for (task, rule) in crawl.tasks.iter().zip(RULES) {
            assert_eq!(task.role_id.as_deref(), Some("crawler"));
            assert_eq!(task.depends_on, vec![format!("plan-{rule}")]);
            let grow = task.grow.as_ref().unwrap_or_else(|| panic!("`{}` must be a grow template", task.id));
            assert_eq!(grow.from, format!("plan-{rule}"), "each track grows from its OWN plan");
            assert_eq!(grow.items, "units");
            // `{{from.output}}` is what hands the grown unit the plan it
            // came from without every plan item repeating the path.
            assert_eq!(grow.config["plan"], serde_json::json!("{{from.output}}"));
            assert_eq!(grow.config["unit"], serde_json::json!("{{item.id}}"));
            assert_eq!(grow.config["rule"], serde_json::json!(rule));
            assert_eq!(task.steps[0].kind, "crawl.unit");
        }

        let summarize = &cfg.phases[2];
        assert_eq!(summarize.tasks.len(), 1);
        assert_eq!(summarize.tasks[0].steps[0].kind, "crawl.summary");

        // (#2302) The create-mods phase: ONE grow template, OFF, growing a
        // `coder` dispatch per finding the summary named. `enabled: false`
        // prunes the task at mint and rule 2 then prunes the emptied phase,
        // so the default crawl still ends at `summarize` — asserted live in
        // `mission_config::prune`'s own tests and in `tests/cli.rs`.
        let create_mods = &cfg.phases[3];
        assert_eq!(create_mods.tasks.len(), 1);
        let t = &create_mods.tasks[0];
        assert_eq!(t.enabled, Some(false), "create-mods ships OFF; the FIELD is the gate");
        assert_eq!(t.role_id.as_deref(), Some("coder"));
        assert_eq!(t.depends_on, vec!["summary".to_string()]);
        assert_eq!(t.steps.len(), 1);
        assert_eq!(t.steps[0].kind, "dispatch.internal");
        let grow = t.grow.as_ref().expect("create-mods is a grow template");
        assert_eq!(grow.from, "summary");
        assert_eq!(
            grow.items, "finding_refs",
            "the summary's ROSTER of findings, not its `findings` COUNT"
        );
        assert_eq!(
            grow.id,
            "{{item.id}}",
            "the id is the key with `/` swapped out — a key is not one id segment"
        );
        assert_eq!(
            grow.config["brief_refs"],
            serde_json::json!([{"kind": "finding", "key": "{{item.key}}"}]),
            "each grown step carries the finding it was grown from"
        );
        assert_eq!(grow.config["workdir"], serde_json::json!("{{item.tree_root}}"));
        let message = grow.config["message"].as_str().expect("the create-mod brief");
        assert!(message.contains("create_mod"), "the coder's product is a mod: {message}");
        assert!(message.contains("`for`"), "named for the finding: {message}");
        let findings = cfg.validate(&known_refs);
        let errors: Vec<&ValidationFinding> =
            findings.iter().filter(|f| f.severity == FindingSeverity::Error).collect();
        assert!(errors.is_empty(), "`crawl` must validate with zero Error findings, got: {errors:?}");
        assert!(cfg.is_valid(&known_refs));
    }

    #[test]
    fn crawl_builtin_declares_every_launcher_input() {
        let cfg = embedded_config("crawl");
        let names: Vec<&str> = cfg.inputs.iter().map(|i| i.name.as_str()).collect();
        // (#2301) The generic path's inputs, exactly. `source`/`rule` (the
        // one-shot pair — a one-shot is a one-source spec file now),
        // `plan`/`plan_out` (a plan is always written under the run) and
        // `units`/`limit`/`resume` were the retired launcher's, and are
        // asserted ABSENT so a copy-paste never quietly reintroduces an
        // input nothing reads.
        assert_eq!(
            names,
            vec!["workspace", "rules", "max_sites_per_unit", "max_est_tokens_per_unit", "no_fetch", "dry_run"],
            "{names:?}"
        );
        assert!(
            cfg.inputs.iter().find(|i| i.name == "workspace").is_some_and(|i| i.required == Some(true)),
            "every `crawl.plan` step reads `workspace`, so it is required"
        );
    }

    #[test]
    fn crawl_builtin_round_trips_through_json() {
        let cfg = embedded_config("crawl");
        let json = serde_json::to_string(&cfg).unwrap();
        let back: MissionConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(cfg, back, "`crawl` must round-trip through JSON unchanged");
    }

    #[test]
    fn document_round_trips_through_json() {
        let cfg = doc(vec![phase(
            "p1",
            vec![task(
                "t1",
                &[],
                vec![StepConfig {
                    enabled: None,
                    id: "s1".to_string(),
                    kind: "dispatch.internal".to_string(),
                    config: serde_json::json!({"role": "coder"}),
                    gate: None,
                    extras: BTreeMap::new(),
                }],
            )],
        )]);
        let json = serde_json::to_string(&cfg).unwrap();
        let back: MissionConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(cfg, back);
    }

    // ─── `gate` field (#1684 Packet 2, schema 2.2) ─────────────────────

    #[test]
    fn a_pre_2_2_document_with_no_gate_field_parses_clean() {
        // The exact minimal 2.1-shaped step document — no `gate` key at
        // all. Lenient-on-read (contract 7): must parse identically to
        // before this packet, `gate` defaulting to `None`.
        let json = r#"{"id":"m","name":"M","phases":[{"id":"p1","tasks":[
            {"id":"t1","steps":[{"id":"s1","kind":"procedural.noop"}]}
        ]}]}"#;
        let cfg: MissionConfig = serde_json::from_str(json).unwrap();
        assert_eq!(cfg.phases[0].tasks[0].steps[0].gate, None);
        assert!(cfg.validate(&["procedural.noop"]).is_empty());
    }

    #[test]
    fn a_gate_operator_step_round_trips_through_json() {
        let cfg = doc(vec![phase(
            "p1",
            vec![task("t1", &[], vec![gated_step("s1", "procedural.noop", crate::gate::GATE_KIND_OPERATOR)])],
        )]);
        assert_eq!(cfg.phases[0].tasks[0].steps[0].gate.as_deref(), Some(crate::gate::GATE_KIND_OPERATOR));
        let json = serde_json::to_string(&cfg).unwrap();
        assert!(json.contains("\"gate\":\"operator\""), "{json}");
        let back: MissionConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(cfg, back);
        assert!(
            cfg.validate(&["procedural.noop"]).is_empty(),
            "a recognized gate value must validate clean: {:?}",
            cfg.validate(&["procedural.noop"])
        );
    }

    #[test]
    fn an_unrecognized_gate_value_is_a_validate_time_warning_not_an_error() {
        let cfg = doc(vec![phase(
            "p1",
            vec![task("t1", &[], vec![gated_step("s1", "procedural.noop", "some-future-kind")])],
        )]);
        let findings = cfg.validate(&["procedural.noop"]);
        let f = findings
            .iter()
            .find(|f| f.path.ends_with(".gate"))
            .unwrap_or_else(|| panic!("expected a `.gate` finding, got {findings:?}"));
        assert_eq!(f.severity, FindingSeverity::Warning, "unrecognized ≠ malformed — a Warning, not an Error");
        assert!(f.message.contains("some-future-kind"), "{}", f.message);
        assert!(
            f.message.contains("FAILS CLOSED"),
            "the finding must say the RUN-time behavior is fail-closed, not silently ungated: {}",
            f.message
        );
        // Never blocks USABILITY — `is_valid` stays true for a Warning.
        assert!(cfg.is_valid(&["procedural.noop"]));
    }

    // ── (#1398) display_name schema field ───────────────────────────────

    #[test]
    fn phase_and_task_display_name_parse_and_round_trip() {
        let json = r#"{
            "id": "m", "name": "M",
            "phases": [{
                "id": "p1", "display_name": "Investigate",
                "tasks": [{"id": "t1", "display_name": "Bundle"}]
            }]
        }"#;
        let cfg: MissionConfig = serde_json::from_str(json).unwrap();
        assert_eq!(cfg.phases[0].display_name.as_deref(), Some("Investigate"));
        assert_eq!(cfg.phases[0].tasks[0].display_name.as_deref(), Some("Bundle"));
        let back = serde_json::to_string(&cfg).unwrap();
        let cfg2: MissionConfig = serde_json::from_str(&back).unwrap();
        assert_eq!(cfg, cfg2);
    }

    #[test]
    fn display_name_absent_is_not_drift() {
        // A pre-#1398 document with no `display_name` anywhere still
        // parses cleanly and validates clean (lenient-on-read, contract 7).
        let cfg = doc(vec![phase("p1", vec![task("t1", &[], vec![step("s1", "dispatch.internal")])])]);
        assert!(cfg.phases[0].display_name.is_none());
        assert!(cfg.phases[0].tasks[0].display_name.is_none());
        assert!(cfg.validate(&["dispatch.internal"]).is_empty());
    }

    #[test]
    fn schema_version_constant_is_3_5() {
        // (#1550 cluster item 2) Retired the `expand`/`ExpansionSpec`/
        // `LaunchParams::expansions` primitive — never fed by either
        // production launcher. A field REMOVAL is a MAJOR bump per this
        // constant's own doc (and CLAUDE.md's Versioning section, the same
        // rule applied to a different data shape) — not the minor 1.5 an
        // earlier draft of this change used.
        //
        // (#1684 Packet 1) Bumped to "2.1" — additive (`panel`), so still a
        // minor bump on top of the 2.0 major.
        //
        // (#1684 Packet 2) Bumped again to "2.2" — additive (`StepConfig::
        // gate`), same minor-bump discipline.
        //
        // (#1685) Bumped again to "2.3" — additive (`MissionConfig::
        // cmd`), same minor-bump discipline.
        //
        // (#2299) Bumped to "3.1" — additive: `enabled` on phases, tasks and
        // steps, pruned at mint. A pre-3.1 reader ignores it and mints all.
        //
        // (#2300) Bumped to "3.2" — additive: `TaskConfig::grow`, the
        // run-time fan-out fed by a step's OUTPUT. A pre-3.2 reader ignores
        // the field and mints nothing for the template, which is correct:
        // a template is not executable on its own in any reader.
        //
        // (#2310 P3) Bumped to "3.3" — additive: `MissionConfig::
        // outcome_from`. A pre-3.3 reader ignores the field and keeps the
        // positional "last phase's last task" close-payload rule, which is
        // correct: the field only OVERRIDES that default, never required.
        //
        // (#2310 P4) Bumped to "3.4" — additive: `TaskConfig::run_on`. A
        // pre-3.4 reader ignores the field and every task resolves to
        // `crate::types::default_run_on()` (`["complete"]`), the exact
        // behavior every pre-3.4 document already had — so nothing already
        // shipped changes meaning under this bump.
        //
        // (#2310 P4f) Bumped to "3.5" — additive: `TaskConfig::excludes`,
        // the "two ways to staff one seat, exactly one live" relation the
        // `create-mods` phase's attended and unattended templates need. A
        // pre-3.5 reader overflows the field into `extras` and mints
        // whatever `enabled` says — and every shipped document keeps one
        // of each excluded pair disabled, so nothing already shipped
        // changes meaning under this bump either.
        assert_eq!(MISSION_CONFIG_SCHEMA, "3.5");
    }

    // ── (#2300) `grow` — the run-time fan-out ────────────────────────────

    fn grow_doc(from_phase: &str, consumer_phase: &str) -> MissionConfig {
        // `p1` produces; the template lives in whichever phase the caller
        // names, so a same-phase / later-phase `from` is expressible.
        let producer = task("plan-task", &[], vec![step("plan-step", "procedural.shell")]);
        let mut template = task("unit-task", &[], vec![step("unit-step", "procedural.noop")]);
        template.grow = Some(GrowSpec {
            from: "plan-task".into(),
            items: "units".into(),
            id: "{{item.id}}".into(),
            config: serde_json::json!({ "unit": "{{item.id}}" }),
            extras: BTreeMap::new(),
        });
        let mut phases = vec![phase("p1", vec![]), phase("p2", vec![])];
        for p in &mut phases {
            if p.id == from_phase {
                p.tasks.push(producer.clone());
            }
            if p.id == consumer_phase {
                p.tasks.push(template.clone());
            }
        }
        MissionConfig { phases, ..doc(Vec::new()) }
    }

    fn grow_errors(config: &MissionConfig) -> Vec<String> {
        config
            .validate(&["procedural.shell", "procedural.noop"])
            .into_iter()
            .filter(|f| f.severity == FindingSeverity::Error)
            .map(|f| format!("{}: {}", f.path, f.message))
            .collect()
    }

    fn grow_warnings(config: &MissionConfig) -> Vec<String> {
        config
            .validate(&["procedural.shell", "procedural.noop"])
            .into_iter()
            .filter(|f| f.severity == FindingSeverity::Warning)
            .map(|f| format!("{}: {}", f.path, f.message))
            .collect()
    }

    fn input_named(name: &str) -> MissionInput {
        MissionInput {
            name: name.to_string(),
            description: None,
            required: None,
            default: None,
            ignored: None,
            ignored_reason: None,
            extras: BTreeMap::new(),
        }
    }

    // ── (#2310 P4c-2 review MUST FIX) undeclared placeholders ──────────

    #[test]
    fn a_static_step_typo_is_a_validate_error() {
        let mut s = step("s", "procedural.noop");
        s.config = serde_json::json!({"intent_file": "{{intent_fle}}"});
        let cfg = doc(vec![phase("p1", vec![task("t", &[], vec![s])])]);
        let errs = grow_errors(&cfg);
        assert!(errs.iter().any(|e| e.contains("step `s`") && e.contains("intent_fle")), "{errs:?}");
    }

    #[test]
    fn a_grow_config_typo_is_a_validate_error() {
        let mut t = task("t", &[], vec![step("s", "procedural.noop")]);
        t.grow = Some(GrowSpec {
            from: "other".to_string(),
            items: "units".to_string(),
            id: "{{item.id}}".to_string(),
            config: serde_json::json!({"intent_file": "{{intent_fle}}"}),
            extras: BTreeMap::new(),
        });
        let cfg = doc(vec![phase("p1", vec![t])]);
        let errs = grow_errors(&cfg);
        assert!(
            errs.iter().any(|e| e.contains("grow.config") && e.contains("intent_fle")),
            "{errs:?}"
        );
    }

    #[test]
    fn a_from_dot_typo_is_a_validate_error_naming_the_grow_namespace() {
        let mut t = task("t", &[], vec![step("s", "procedural.noop")]);
        t.grow = Some(GrowSpec {
            from: "other".to_string(),
            items: "units".to_string(),
            id: "{{item.id}}".to_string(),
            config: serde_json::json!({"plan": "{{from.typo}}"}),
            extras: BTreeMap::new(),
        });
        let cfg = doc(vec![phase("p1", vec![t])]);
        let errs = grow_errors(&cfg);
        assert!(errs.iter().any(|e| e.contains("from.output")), "{errs:?}");
    }

    // ── (#2310 P4c-2 review round 2, item a) embedded + optional ───────

    #[test]
    fn an_embedded_placeholder_naming_an_optional_input_is_a_validate_warning() {
        let mut s = step("s", "procedural.noop");
        s.config = serde_json::json!({"label": "run-{{tag}}"});
        let cfg = MissionConfig {
            inputs: vec![input_named("workspace"), MissionInput { required: Some(false), ..input_named("tag") }],
            ..doc(vec![phase("p1", vec![task("t", &[], vec![s])])])
        };
        assert!(grow_errors(&cfg).is_empty(), "a document is still USABLE, just flagged");
        let warns = grow_warnings(&cfg);
        assert!(warns.iter().any(|w| w.contains("tag") && w.contains("optional")), "{warns:?}");
    }

    #[test]
    fn an_embedded_placeholder_naming_a_required_input_is_never_flagged() {
        let mut s = step("s", "procedural.noop");
        s.config = serde_json::json!({"label": "run-{{tag}}"});
        let cfg = MissionConfig {
            inputs: vec![MissionInput { required: Some(true), ..input_named("tag") }],
            ..doc(vec![phase("p1", vec![task("t", &[], vec![s])])])
        };
        let warns = grow_warnings(&cfg);
        assert!(!warns.iter().any(|w| w.contains("tag")), "{warns:?}");
    }

    #[test]
    fn a_whole_value_placeholder_for_an_optional_input_is_never_flagged() {
        let mut s = step("s", "procedural.noop");
        s.config = serde_json::json!({"tag": "{{tag}}"});
        let cfg = MissionConfig {
            inputs: vec![MissionInput { required: Some(false), ..input_named("tag") }],
            ..doc(vec![phase("p1", vec![task("t", &[], vec![s])])])
        };
        let warns = grow_warnings(&cfg);
        assert!(!warns.iter().any(|w| w.contains("tag")), "{warns:?}");
    }

    // ── (#2310 P4c-2 review item 4) ignored + required interaction ─────

    #[test]
    fn ignored_true_with_required_true_is_a_validate_error() {
        let cfg = MissionConfig {
            inputs: vec![MissionInput { required: Some(true), ignored: Some(true), ..input_named("x") }],
            ..doc(vec![])
        };
        let errs = grow_errors(&cfg);
        assert!(errs.iter().any(|e| e.contains('x') && e.contains("ignored") && e.contains("required")), "{errs:?}");
    }

    /// (#2310 P4e review, item 5) The silent half of the same
    /// contradiction: `apply_input_defaults` runs before
    /// `missing_required_inputs`, so a defaulted input always satisfies
    /// `required` and the flag never blocks anything. Red-proved by
    /// deleting the `required == Some(true) && default.is_some()` loop.
    #[test]
    fn required_true_with_a_default_is_a_validate_error() {
        let cfg = MissionConfig {
            inputs: vec![MissionInput {
                required: Some(true),
                default: Some(serde_json::json!("0")),
                ..input_named("x")
            }],
            ..doc(vec![])
        };
        let errs = grow_errors(&cfg);
        assert!(
            errs.iter().any(|e| e.contains('x') && e.contains("required") && e.contains("default")),
            "{errs:?}"
        );
    }

    /// The other side: an OPTIONAL input with a default is the ordinary,
    /// correct shape (`review`'s `mod_wait_seconds`) and must be silent.
    #[test]
    fn an_optional_input_with_a_default_is_clean() {
        let cfg = MissionConfig {
            inputs: vec![MissionInput {
                required: Some(false),
                default: Some(serde_json::json!("0")),
                ..input_named("x")
            }],
            ..doc(vec![])
        };
        assert!(grow_errors(&cfg).is_empty(), "{:?}", grow_errors(&cfg));
        assert!(
            grow_warnings(&cfg).iter().all(|w| !w.contains("default")),
            "{:?}",
            grow_warnings(&cfg)
        );
    }

    #[test]
    fn ignored_true_without_a_reason_is_a_validate_warning_not_an_error() {
        let cfg = MissionConfig {
            inputs: vec![MissionInput { ignored: Some(true), ..input_named("x") }],
            ..doc(vec![])
        };
        assert!(grow_errors(&cfg).is_empty(), "no reason is a smell, not a hard error");
        let warns = grow_warnings(&cfg);
        assert!(warns.iter().any(|w| w.contains('x') && w.contains("ignored_reason")), "{warns:?}");
    }

    #[test]
    fn ignored_true_with_a_reason_is_clean() {
        let cfg = MissionConfig {
            inputs: vec![MissionInput {
                ignored: Some(true),
                ignored_reason: Some("kept for parity".to_string()),
                ..input_named("x")
            }],
            ..doc(vec![])
        };
        assert!(grow_errors(&cfg).is_empty());
        assert!(grow_warnings(&cfg).iter().all(|w| !w.contains("ignored_reason")), "{:?}", grow_warnings(&cfg));
    }

    #[test]
    fn a_grow_block_parses_off_the_wire_and_round_trips() {
        let json = r#"{
            "id": "g", "name": "G", "phases": [
              {"id": "p1", "tasks": [{"id": "plan-task", "steps": [
                 {"id": "plan-step", "kind": "procedural.shell", "config": {}}]}]},
              {"id": "p2", "tasks": [{"id": "unit-task",
                 "grow": {"from": "plan-task", "items": "units", "id": "{{item.id}}",
                          "config": {"unit": "{{item.id}}", "rule": "{{item.rule}}"}},
                 "steps": [{"id": "unit-step", "kind": "procedural.noop", "config": {}}]}]}
            ]}"#;
        let cfg: MissionConfig = serde_json::from_str(json).expect("a 3.2 document parses");
        let grow = cfg.phases[1].tasks[0].grow.as_ref().expect("grow parsed");
        assert_eq!(grow.from, "plan-task");
        assert_eq!(grow.items, "units");
        assert_eq!(grow.id, "{{item.id}}");
        assert_eq!(grow.config["rule"], serde_json::json!("{{item.rule}}"));
        // Round-trip: `grow` survives a save/load cycle unchanged.
        let again: MissionConfig = serde_json::from_str(&serde_json::to_string(&cfg).unwrap()).unwrap();
        assert_eq!(again, cfg);
        assert!(grow_errors(&cfg).is_empty(), "{:?}", grow_errors(&cfg));
    }

    #[test]
    fn a_task_with_no_grow_omits_the_key_entirely() {
        // Additive contract: a pre-3.2 document round-trips byte-identically.
        let cfg = doc(vec![phase(
            "p1",
            vec![task("t", &[], vec![step("s", "procedural.noop")])],
        )]);
        let text = serde_json::to_string(&cfg).unwrap();
        assert!(!text.contains("grow"), "an absent grow must not serialize: {text}");
    }

    #[test]
    fn a_same_phase_grow_from_is_an_error_naming_the_phase_boundary() {
        let errs = grow_errors(&grow_doc("p2", "p2"));
        assert_eq!(errs.len(), 1, "{errs:?}");
        assert!(errs[0].contains("same phase"), "{errs:?}");
        assert!(errs[0].contains("PHASE BOUNDARY"), "{errs:?}");
    }

    #[test]
    fn a_later_phase_grow_from_is_an_error() {
        let errs = grow_errors(&grow_doc("p2", "p1"));
        assert_eq!(errs.len(), 1, "{errs:?}");
        assert!(errs[0].contains("later phase"), "{errs:?}");
    }

    #[test]
    fn an_earlier_phase_grow_from_validates_clean() {
        assert!(grow_errors(&grow_doc("p1", "p2")).is_empty());
    }

    #[test]
    fn an_unknown_or_self_referential_grow_from_is_an_error() {
        let mut cfg = grow_doc("p1", "p2");
        cfg.phases[1].tasks[0].grow.as_mut().unwrap().from = "nope".into();
        assert!(grow_errors(&cfg).iter().any(|e| e.contains("unknown task id")), "{:?}", grow_errors(&cfg));
        cfg.phases[1].tasks[0].grow.as_mut().unwrap().from = "unit-task".into();
        assert!(grow_errors(&cfg).iter().any(|e| e.contains("grows from itself")), "{:?}", grow_errors(&cfg));
    }

    #[test]
    fn empty_grow_fields_and_a_non_object_config_are_errors() {
        let mut cfg = grow_doc("p1", "p2");
        {
            let g = cfg.phases[1].tasks[0].grow.as_mut().unwrap();
            g.items = "  ".into();
            g.id = String::new();
            g.config = serde_json::json!([1, 2]);
        }
        let errs = grow_errors(&cfg);
        assert_eq!(errs.len(), 3, "{errs:?}");
        assert!(errs.iter().any(|e| e.contains("grow.items")), "{errs:?}");
        assert!(errs.iter().any(|e| e.contains("grow.id")), "{errs:?}");
        assert!(errs.iter().any(|e| e.contains("must be an object")), "{errs:?}");
    }

    #[test]
    fn depending_on_a_grow_template_is_an_error() {
        // The template is never minted, so the edge could only resolve to
        // nothing — silently, which is the whole failure class #2300 exists
        // not to repeat.
        let mut cfg = grow_doc("p1", "p2");
        let mut consumer = task("after", &["unit-task"], vec![step("after-step", "procedural.noop")]);
        consumer.reads = vec!["unit-task".into()];
        cfg.phases.push(phase("p3", vec![consumer]));
        let errs = grow_errors(&cfg);
        assert_eq!(errs.len(), 2, "one per relation: {errs:?}");
        assert!(errs.iter().all(|e| e.contains("grow` TEMPLATE")), "{errs:?}");
    }

    // ── (#2345 C2) `outcome_from` validation ────────────────────────────

    #[test]
    fn outcome_from_naming_a_real_task_validates_clean() {
        let cfg = MissionConfig {
            outcome_from: Some("t".into()),
            ..doc(vec![phase("p1", vec![task("t", &[], vec![step("s", "procedural.noop")])])])
        };
        assert!(grow_errors(&cfg).is_empty(), "{:?}", grow_errors(&cfg));
    }

    #[test]
    fn outcome_from_naming_an_unknown_task_is_an_error() {
        let cfg = MissionConfig {
            outcome_from: Some("no-such-task".into()),
            ..doc(vec![phase("p1", vec![task("t", &[], vec![step("s", "procedural.noop")])])])
        };
        let errs = grow_errors(&cfg);
        assert_eq!(errs.len(), 1, "{errs:?}");
        assert!(errs[0].starts_with("outcome_from:"), "{errs:?}");
        assert!(errs[0].contains("unknown task id \"no-such-task\""), "{errs:?}");
    }

    #[test]
    fn outcome_from_naming_a_grow_template_is_an_error() {
        // The template is never itself minted (#2300) — it has no
        // `Step.output` of its own, exactly the same "would resolve to
        // nothing" failure class `depending_on_a_grow_template_is_an_error`
        // guards for the `depends_on`/`reads` relations.
        let mut cfg = grow_doc("p1", "p2");
        cfg.outcome_from = Some("unit-task".into());
        let errs = grow_errors(&cfg);
        assert!(
            errs.iter().any(|e| e.starts_with("outcome_from:") && e.contains("TEMPLATE")),
            "{errs:?}"
        );
    }

    #[test]
    fn outcome_from_naming_a_disabled_task_is_an_error() {
        // (#2345 CONSIDER-2, round 2) A disabled task is a REAL declared
        // task (`all_task_ids` contains it, so the "unknown task id" branch
        // never fires) but is pruned at mint (`prune.rs`) and so never
        // produces a `Step.output` — same failure CLASS as a grow template,
        // different reason.
        let mut cfg = doc(vec![phase("p1", vec![task("t", &[], vec![step("s", "procedural.noop")])])]);
        cfg.phases[0].tasks[0].enabled = Some(false);
        cfg.outcome_from = Some("t".into());
        let errs = grow_errors(&cfg);
        assert!(
            errs.iter().any(|e| e.starts_with("outcome_from:") && e.contains("disabled")),
            "{errs:?}"
        );
    }

    #[test]
    fn outcome_from_naming_a_task_under_a_disabled_phase_is_an_error() {
        // The TASK's own `enabled` is absent (defaults true) — it is the
        // owning PHASE that is disabled, `prune.rs`'s other check. Same
        // "never minted" outcome, so `outcome_from` must refuse it too.
        let mut cfg = doc(vec![phase("p1", vec![task("t", &[], vec![step("s", "procedural.noop")])])]);
        cfg.phases[0].enabled = Some(false);
        cfg.outcome_from = Some("t".into());
        let errs = grow_errors(&cfg);
        assert!(
            errs.iter().any(|e| e.starts_with("outcome_from:") && e.contains("disabled")),
            "{errs:?}"
        );
    }

    #[test]
    fn outcome_from_naming_an_enabled_task_in_a_document_with_other_disabled_tasks_validates_clean() {
        // Sanity: `disabled_tasks` must not over-match — a document with
        // ONE disabled task elsewhere must not block `outcome_from` naming
        // a DIFFERENT, enabled one.
        let mut cfg = doc(vec![phase(
            "p1",
            vec![
                task("live", &[], vec![step("s1", "procedural.noop")]),
                task("dead", &[], vec![step("s2", "procedural.noop")]),
            ],
        )]);
        cfg.phases[0].tasks[1].enabled = Some(false);
        cfg.outcome_from = Some("live".into());
        assert!(grow_errors(&cfg).is_empty(), "{:?}", grow_errors(&cfg));
    }

    #[test]
    fn absent_outcome_from_is_never_a_validation_error() {
        // Every pre-3.3 document omits the field — must never trip
        // validate() on its own (the additive contract MISSION_CONFIG_
        // SCHEMA's own "3.3" doc note describes).
        let cfg = doc(vec![phase("p1", vec![task("t", &[], vec![step("s", "procedural.noop")])])]);
        assert!(cfg.outcome_from.is_none());
        assert!(grow_errors(&cfg).is_empty(), "{:?}", grow_errors(&cfg));
    }

    // ── (#1684) `panel` schema field ─────────────────────────────────────

    #[test]
    fn a_document_without_panel_still_parses_and_validates_clean() {
        // Every pre-2.1 document omits `panel` — must parse identically
        // (contract 5, additive minor) and never trip a validate() error.
        let cfg = doc(vec![]);
        assert!(cfg.panel.is_none());
        assert!(cfg.validate(&[]).is_empty());
    }

    #[test]
    fn a_document_with_panel_round_trips_through_json() {
        let mut cfg = doc(vec![]);
        cfg.panel = Some(PanelConfig {
            description: Some("PR view".to_string()),
            hint: Some("<pr number>".to_string()),
            extras: BTreeMap::new(),
        });
        let json = serde_json::to_string(&cfg).unwrap();
        let back: MissionConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(cfg, back);
        assert_eq!(back.panel.unwrap().hint.as_deref(), Some("<pr number>"));
    }

    #[test]
    fn panel_with_no_hint_parses_and_round_trips() {
        // `hint` is itself optional — a config can advertise as a panel
        // command with no input hint at all.
        let json = r#"{"id":"x","name":"X","panel":{}}"#;
        let cfg: MissionConfig = serde_json::from_str(json).unwrap();
        assert!(cfg.panel.is_some());
        assert_eq!(cfg.panel.as_ref().unwrap().hint, None);
        let back = serde_json::to_string(&cfg).unwrap();
        let cfg2: MissionConfig = serde_json::from_str(&back).unwrap();
        assert_eq!(cfg, cfg2);
    }

    #[test]
    fn unknown_fields_inside_panel_are_tolerated_lenient_on_read() {
        // (contract 7) Unrecognized keys inside `panel` overflow into its
        // own `extras`, never fail parsing — a future sub-field a fresh
        // binary doesn't know yet is safe.
        let json = r#"{"id":"x","name":"X","panel":{"hint":"h","futureField":{"a":1}}}"#;
        let cfg: MissionConfig = serde_json::from_str(json).unwrap();
        let panel = cfg.panel.expect("panel block present");
        assert_eq!(panel.hint.as_deref(), Some("h"));
        assert_eq!(panel.extras.get("futureField"), Some(&serde_json::json!({"a": 1})));
    }

    #[test]
    fn review_builtin_declares_a_panel_block() {
        // (#1684) `review` picks up panel advertising the same way any
        // other config would — no more hardcoded single command in
        // `src/acp.rs`.
        let cfg = embedded_config("review");
        let panel = cfg.panel.expect("the built-in review config must declare a panel block");
        assert!(panel.hint.is_some(), "review's panel block should carry an input hint");
        // (QA finding) `panel.description` must be a SHORT UI label, never
        // the ~2KB provenance essay `MissionConfig.description` carries —
        // that essay is developer-facing prose, not a command-palette
        // label.
        let panel_description = panel.description.expect("review's panel block should carry a short description");
        assert!(
            panel_description.len() < 120,
            "panel.description must stay a short UI label, got {} chars: {panel_description}",
            panel_description.len()
        );
        assert_ne!(
            Some(panel_description),
            cfg.description,
            "panel.description must never equal the long-form MissionConfig.description"
        );
    }

    #[test]
    fn panel_description_round_trips_and_is_distinct_from_top_level_description() {
        let mut cfg = doc(vec![]);
        cfg.description = Some("a long developer-facing provenance essay".to_string());
        cfg.panel = Some(PanelConfig {
            description: Some("Short UI label".to_string()),
            hint: None,
            extras: BTreeMap::new(),
        });
        let json = serde_json::to_string(&cfg).unwrap();
        let back: MissionConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(back.panel.as_ref().unwrap().description.as_deref(), Some("Short UI label"));
    }

    // ── (#1684 QA finding) reserved `__panel_args__` never dangles ─────

    #[test]
    fn reads_naming_the_reserved_panel_args_id_never_dangles() {
        let mut t = task("t1", &[], vec![step("s1", "procedural.shell")]);
        t.reads = vec![PANEL_ARGS_TASK_ID.to_string()];
        let cfg = doc(vec![phase("p1", vec![t])]);
        let findings = cfg.validate(&["procedural.shell"]);
        assert!(
            findings.iter().all(|f| f.severity != FindingSeverity::Error),
            "a reads entry naming the reserved panel-args id must never be treated as dangling: {findings:?}"
        );
    }

    #[test]
    fn depends_on_naming_the_reserved_panel_args_id_never_dangles() {
        let mut t = task("t1", &[], vec![step("s1", "procedural.shell")]);
        t.depends_on = vec![PANEL_ARGS_TASK_ID.to_string()];
        let cfg = doc(vec![phase("p1", vec![t])]);
        let findings = cfg.validate(&["procedural.shell"]);
        assert!(
            findings.iter().all(|f| f.severity != FindingSeverity::Error),
            "a depends_on entry naming the reserved panel-args id must never be treated as dangling: {findings:?}"
        );
    }
}
