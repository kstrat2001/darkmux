//! darkmux — a lab and multiplexer for local LLM configurations.
//!
//! v0.2 in Rust. Ports the v0.1 TS prototype + the v0.2 lab foundation.

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};

// #463 workspace split — crew extracted to its own crate (the velocity-debt
// target: touching dispatch_internal.rs now rebuilds only darkmux-crew + the
// binary stub). Re-export keeps crate::crew::* resolving for the binary +
// fleet.
pub use darkmux_crew as crew;
// #515 — doctor extracted (all deps now crates; resolves the plan's open
// sub-decision). Re-export keeps crate::doctor::* resolving for serve.
pub use darkmux_doctor as doctor;
// #515 Tier B — eureka rules engine extracted. Re-export keeps crate::eureka::*
// resolving for doctor/serve/lab.
pub use darkmux_eureka as eureka;
mod external;
// #515 — fleet extracted (deps crew/flow/types all crates now). Re-export
// keeps crate::fleet::* resolving for serve/sprint_cli/notebook/mission_propose.
pub use darkmux_fleet as fleet;
// #463 workspace split — flow extracted to the darkmux-flow crate. The
// re-export keeps all existing `crate::flow::*` paths resolving unchanged.
pub use darkmux_flow as flow;
mod flow_cli;
// #515 — zero-edge leaf extracted to darkmux-hardware. Re-export keeps
// crate::hardware::* resolving for heuristics/eureka/recommendations/doctor/etc.
pub use darkmux_hardware as hardware;
// #515 Tier B — per-tier heuristics extracted. Re-export keeps
// crate::heuristics::* resolving for recommendations/optimize/doctor.
pub use darkmux_heuristics as heuristics;
mod init;
// #515 — lab harness extracted (lab + workloads + providers). Re-exports keep
// crate::{lab,workloads,providers}::* resolving for main + notebook.
pub use darkmux_lab::lab;
mod migrate;
mod conventions;
mod mission_propose;
mod mission_status;
mod mission_run;
mod notebook;
mod optimize;
pub use darkmux_lab::providers;
// #515 — recommendation registry extracted (deps hardware/heuristics/types,
// all crates). Re-export keeps crate::recommendations::* resolving for doctor.
pub use darkmux_recommendations as recommendations;
mod role_cli;
// #515 — serve daemon extracted (final crate; deps doctor/eureka/fleet/crew/
// flow/profiles all crates). Re-export keeps crate::serve::* resolving for
// main + sprint_cli.
pub use darkmux_serve as serve;
mod skills;
mod sprint_cli;
// #463 workspace split (PR2) — profiles/swap/lms/runtime extracted to the
// darkmux-profiles crate. These re-exports keep every existing
// crate::{profiles,swap,lms,runtime}::* path resolving unchanged.
pub use darkmux_profiles::lms;
pub use darkmux_profiles::profiles;
pub use darkmux_profiles::runtime;
pub use darkmux_profiles::swap;
// #463 workspace split — types extracted to the darkmux-types crate. The
// re-export keeps all existing `crate::types::*` paths resolving unchanged.
pub use darkmux_types as types;
// #463 — workdir lifted into darkmux-types (leaf util shared by crew + fleet,
// so crew can use it without a binary-resident edge). Re-export keeps
// crate::workdir::* resolving for fleet + other binary modules.
pub use darkmux_types::workdir;
pub use darkmux_lab::workloads;

const VERSION: &str = env!("CARGO_PKG_VERSION");

#[derive(Parser)]
#[command(name = "darkmux", version = VERSION, about = "Lab and multiplexer for local LLM configurations")]
struct Cli {
    #[command(subcommand)]
    command: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Swap the LMStudio stack to a profile. With `--runtime openclaw`, ALSO
    /// patches openclaw's config to match; the default leaves
    /// `~/.openclaw/openclaw.json` untouched.
    Swap {
        /// Name of the profile to swap to (from profiles.json).
        #[arg(required = false)]
        profile: Option<String>,
        /// Swap to the tier-recommended profile for this machine instead of
        /// a named profile.
        #[arg(long)]
        recommended: bool,
        /// Profiles-registry path (profiles.json). Overrides DARKMUX_PROFILES
        /// and the default search locations. (renamed from --config, #661)
        #[arg(long = "profiles-file")]
        profiles: Option<String>,
        #[arg(long, short = 'n')]
        dry_run: bool,
        #[arg(long, short = 'q')]
        quiet: bool,
        /// Dispatch runtime this profile targets. `internal` (default) — swap
        /// loads LMStudio and touches NOTHING else. `openclaw` — ALSO patch
        /// `~/.openclaw/openclaw.json` to match the profile (model pins,
        /// contextWindow sync, namespaced registry entries). Openclaw config
        /// is patched on explicit opt-in only, never via passive file-presence
        /// — mirrors `crew dispatch` / `lab run` (#590 openclaw independence).
        #[arg(long, default_value = "internal")]
        runtime: String,
    },
    /// Show what's loaded and which profile (if any) it matches.
    Status {
        /// Profiles-registry path (profiles.json). Overrides DARKMUX_PROFILES
        /// and the default search locations. (renamed from --config, #661)
        #[arg(long = "profiles-file")]
        profiles: Option<String>,
        /// Emit machine-readable JSON instead of styled text (#907).
        #[arg(long)]
        json: bool,
    },
    /// List profiles in the registry.
    Profiles {
        /// Profiles-registry path (profiles.json). Overrides DARKMUX_PROFILES
        /// and the default search locations. (renamed from --config, #661)
        #[arg(long = "profiles-file")]
        profiles: Option<String>,
        /// Emit machine-readable JSON instead of styled text (#907).
        #[arg(long)]
        json: bool,
    },
    /// Lab subcommands.
    Lab {
        #[command(subcommand)]
        sub: LabCmd,
    },
    /// Manage agent-invokable skills bundled with darkmux.
    Skills {
        #[command(subcommand)]
        sub: SkillsCmd,
    },
    /// Notebook commands — agent-as-scribe for lab notebook entries.
    Notebook {
        #[command(subcommand)]
        sub: NotebookCmd,
    },
    /// Run pre-flight diagnostic checks. Verifies the local setup (profile
    /// registry, LMStudio, models, runtime, RAM, power) and reports
    /// pass/warn/fail with actionable hints. Exit 0 if no failures, else 1.
    ///
    /// By default only runs internal-runtime-relevant checks. Pass
    /// `--include-openclaw` to also run checks against
    /// ~/.openclaw/openclaw.json (model pin drift, runtime binary
    /// discovery, version, agent definitions). See DESIGN.md "Schema
    /// isolation: each runtime owns its own config".
    Doctor {
        /// Attempt to auto-apply known-safe fixes for failing or warning
        /// checks where a handler is registered (currently:
        /// `eureka: ctx-window-mismatch` realigns openclaw.json
        /// `contextWindow` values to match what `lms ps` reports). After
        /// the fixes run, doctor re-evaluates and prints the updated report.
        #[arg(long)]
        fix: bool,
        /// (#393) Include openclaw-specific checks.
        /// Default: doctor output is internal-runtime-only. With this
        /// flag, also runs checks against ~/.openclaw/openclaw.json
        /// (model pin drift, runtime binary discovery, version, agent
        /// definitions). See DESIGN.md "Schema isolation: each runtime
        /// owns its own config".
        #[arg(long)]
        include_openclaw: bool,
    },
    /// Scan the LMStudio model catalog for downloaded models that aren't yet
    /// covered by any profile. For each uncovered model, suggests a task class
    /// and rough memory impact. Run after downloading a new model in LMStudio
    /// to see whether you'd want to define a profile for it.
    Scan {
        /// Profiles-registry path (profiles.json). Overrides DARKMUX_PROFILES
        /// and the default search locations. (renamed from --config, #661)
        #[arg(long = "profiles-file")]
        profiles: Option<String>,
    },
    /// Profile management subcommands.
    Profile {
        #[command(subcommand)]
        sub: ProfileCmd,
    },
    /// Model lifecycle subcommands — operate on the darkmux-managed model
    /// group (anything in `lms ps` under the `darkmux:` namespace).
    /// User-loaded models (non-namespaced identifiers) are off-limits to
    /// these commands by design.
    Model {
        #[command(subcommand)]
        sub: ModelCmd,
    },
    /// Fleet management — declare which machines compose your darkmux
    /// fleet and probe their reachability. The substrate for tier-aware
    /// dispatch routing (PR-C / #247) and the topology view's fleet
    /// pane. Single-machine fleets work without any roster entries;
    /// multi-machine fleets need `darkmux fleet add <id>` per peer.
    /// (#246 / #248)
    Fleet {
        #[command(subcommand)]
        sub: FleetCmd,
    },
    /// Crew subcommands — dispatch a role for a single turn, or reconcile
    /// the openclaw agent registry with the on-disk crew manifests.
    Crew {
        #[command(subcommand)]
        sub: CrewCmd,
    },
    /// Engagement-context lessons — operator-authored conventions,
    /// constraints, and decisions (with the reasoning behind them) that surface
    /// to coder dispatches as a `<lessons>` block. Stored in a durable,
    /// concurrent-safe SQLite `lessons.db`. Per-repo by default
    /// (`<repo>/.darkmux/lessons.db`, engagement-scoped); `--global` targets
    /// the cross-engagement `~/.darkmux/lessons.db`. (#994)
    Lessons {
        #[command(subcommand)]
        sub: LessonsCmd,
    },
    /// Role management — list and show role details from the SQLite index.
    Role {
        #[command(subcommand)]
        sub: RoleCmd,
    },
    /// Sprint planning — pre-dispatch budget oracle.
    /// `darkmux sprint estimate <spec.json>` computes token consumption +
    /// recommends a profile. `--narrate` adds a one-sentence operator-facing
    /// wrap from the 4B compactor.
    Sprint {
        #[command(subcommand)]
        sub: SprintCmd,
    },
    /// Mission lifecycle — transition missions through their state machine.
    /// Mission status flows: Active ↔ Paused → Closed. All transitions are
    /// operator-explicit; nothing auto-decides a mission is paused or done.
    /// Wall-clock UI consumes mission timestamps via `darkmux serve`.
    Mission {
        #[command(subcommand)]
        sub: MissionCmd,
    },
    /// Per-hardware-tier recommendations — primary + compactor model
    /// picks validated via the project's bake-off methodology. Read-only;
    /// surfaces the registry's pick for the active or a specified tier.
    Recommendations {
        #[command(subcommand)]
        sub: RecommendationsCmd,
    },
    /// Flow observability — record operator-facing flow events.
    Flow {
        #[command(subcommand)]
        sub: flow_cli::FlowCmd,
    },
    /// Start an HTTP daemon for flow record retrieval.
    Serve {
        /// Port to listen on (default: 8765).
        #[arg(long, default_value = "8765")]
        port: u16,
        /// Address to bind (default: 127.0.0.1).
        #[arg(long, default_value = "127.0.0.1")]
        bind: String,
        /// Directory to serve flow records from (default: ~/.darkmux/flows/).
        #[arg(long = "flows-dir")]
        flows_dir: Option<std::path::PathBuf>,
    },
    /// Optimize for your workload — guided wizard (Phase 1 scaffold).
    /// Composes scan, lab characterize/tune, heuristics, and eureka rules
    /// into an opinionated optimization loop.
    Optimize,
    /// One-command setup: install skills, optionally add session-start hook
    /// and CLAUDE.md integration so Claude Code knows about darkmux.
    Init {
        /// Add a SessionStart hook to ~/.claude/settings.json that runs
        /// `darkmux status` so Claude sees the current stack at session start.
        #[arg(long)]
        with_hook: bool,
        /// Append a darkmux integration section to the given CLAUDE.md.
        /// Use `~/.claude/CLAUDE.md` for global, or a project-relative path.
        #[arg(long)]
        with_claude_md: Option<std::path::PathBuf>,
        /// Append a darkmux integration section to the given AGENTS.md.
        /// Use `./AGENTS.md` for a project-relative path, or any custom path.
        #[arg(long)]
        with_agents_md: Option<std::path::PathBuf>,
        /// Overwrite existing skills / hook entries.
        #[arg(long, short = 'f')]
        force: bool,
        /// Show what would be installed without writing.
        #[arg(long, short = 'n')]
        dry_run: bool,
    },
    /// External-source plugins: pull text/markdown from a single artifact
    /// out to stdout. Composes with `darkmux mission propose` and other
    /// downstream commands via shell pipes. Each invocation hits exactly
    /// one plugin (mutually exclusive flags).
    External {
        #[command(subcommand)]
        sub: ExternalCmd,
    },
}

#[derive(Subcommand)]
enum ExternalCmd {
    /// Pull text/markdown from an external source to stdout. Plugin
    /// contract: produce text/markdown (any reasonable shape) to
    /// stdout; downstream verbs read it as input. Exactly one source
    /// flag must be provided.
    #[command(group(clap::ArgGroup::new("source").required(true).multiple(false).args(["gh", "url", "stdin"])))]
    Pull {
        /// Pull from a GitHub issue or PR URL (wraps `gh issue view`
        /// or `gh pr view`). Requires `gh` on PATH.
        #[arg(long, group = "source")]
        gh: Option<String>,
        /// Pull from any URL (HTTP GET via `curl -s -L --max-time 30`).
        /// Output is whatever the URL responds with — HTML is
        /// passed through unchanged for now.
        #[arg(long, group = "source")]
        url: Option<String>,
        /// Read from stdin and echo to stdout (passthrough). Useful
        /// for `pbpaste | darkmux external pull --stdin | ...`.
        #[arg(long, group = "source")]
        stdin: bool,
    },
}

#[derive(Subcommand)]
enum RecommendationsCmd {
    /// Show the recommendation registry entry for a hardware tier.
    /// Defaults to the active tier (resolved via the same hardware
    /// fingerprint `darkmux doctor` uses); pass `<tier>` to inspect a
    /// non-active tier (`m-series-128`, `m-series-64`, `m-series-32`,
    /// `generic`). Operator-readable output: status, profile name,
    /// primary + compactor model ids, rationale.
    Show {
        /// Optional tier id; defaults to the active hardware tier.
        tier: Option<String>,
        /// Emit machine-readable JSON instead of styled text (#907).
        #[arg(long)]
        json: bool,
    },
}

// The `Dispatch` variant aggregates the full operator-visible dispatch
// surface (role + message + ~15 flags) and crosses clippy's enum-size
// threshold relative to the smaller `List`/`Show` variants. Boxing the
// variant would require pattern-match adjustments throughout the
// dispatch handler; the variant is constructed once per CLI invocation
// so the stack-size hit doesn't matter at runtime.
#[allow(clippy::large_enum_variant)]
#[derive(Subcommand)]
enum CrewCmd {
    /// List every crew in the index.
    List,
    /// Show full details for a single crew.
    Show {
        /// Crew id to show.
        id: String,
    },
    /// Dispatch a single turn to the named role. Loads the role manifest +
    /// `.md` system prompt and runs the role through the **internal runtime**
    /// by default — a per-dispatch `darkmux-runtime` Docker container with the
    /// assembled message. Pass `--runtime openclaw` to opt into the openclaw
    /// shell-out path instead (which pre-flight-verifies the `darkmux/<role-id>`
    /// openclaw agent matches the manifest, then invokes `openclaw agent`).
    Dispatch {
        /// Role id (e.g. `code-reviewer`). Must have a manifest at
        /// `templates/builtin/roles/<id>.json` (or under
        /// `~/.darkmux/roles/`) AND a sibling `.md` prompt file.
        role: String,
        /// Message body for the dispatch.
        #[arg(long, short = 'm')]
        message: String,
        /// Optional delivery target in `<channel>:<target>` form
        /// (e.g. `discord:1500166601909993503`). When set, openclaw's
        /// reply is delivered to that channel in addition to being
        /// returned on stdout.
        #[arg(long)]
        deliver: Option<String>,
        /// Override the dispatch session id. Default: a fresh
        /// `crew-dispatch-<role>-<unix-micros>-<process-counter>` is
        /// generated per call, so consecutive dispatches don't share
        /// openclaw session state (which would otherwise pollute one
        /// task with another's context).
        #[arg(long)]
        session_id: Option<String>,
        /// Timeout in seconds (default: 600).
        #[arg(long, default_value = "600")]
        timeout: u32,
        /// Path to snapshot for the post-dispatch state echo (#89 — SIGNOFF
        /// verification visibility). Repeatable: pass `--watch` multiple
        /// times to capture multiple directories. If omitted, defaults to
        /// the role's openclaw workspace dir (~/.openclaw/workspace-darkmux-<role>/).
        /// After the dispatch returns, the dispatcher walks each path
        /// (top-level + one level deep) and emits a stderr summary of
        /// regular files + sizes so the operator can compare the actual
        /// state on disk against the SIGNOFF block's "files written" claims.
        #[arg(long = "watch", value_name = "PATH")]
        watch: Vec<std::path::PathBuf>,
        /// Explicit working directory override (#143). When set, the
        /// dispatcher creates/replaces a `repo` symlink in the role's
        /// openclaw workspace pointing at this path, so the agent
        /// operates against the operator-named scope. When omitted, the
        /// dispatcher does NOT touch the workspace — whatever symlink
        /// already exists (or none at all) is what the agent sees. Per
        /// the operator-sovereignty contract: darkmux doesn't auto-
        /// fabricate scope, but it also doesn't auto-strip scope the
        /// operator has set up manually.
        #[arg(long = "workdir", value_name = "PATH")]
        workdir: Option<std::path::PathBuf>,
        /// Sprint id binding this dispatch to a sprint in a mission
        /// (#146 Stage 1). When set, the dispatcher reads the sprint's
        /// `depends_on` parents and prepends each recorded output as a
        /// "Prior sprint outputs" context block on the message. After
        /// the dispatch returns, the agent's reply text is persisted to
        /// `<crew_root>/sprints/<sprint-id>-output.txt` so downstream
        /// sprints can read it on their own dispatch. One-hop only —
        /// transitive ancestors are not walked (Stage 1 scope).
        #[arg(long = "sprint-id", value_name = "ID")]
        sprint_id: Option<String>,
        /// Skip the pre-flight checks. Use only for debugging.
        #[arg(long, hide = true)]
        skip_preflight: bool,
        /// Emit the runtime's response as a machine-parseable JSON
        /// envelope on stdout, with status lines routed to stderr.
        /// Schema: `{ result, final_assistant, metrics, trajectory_path }`.
        /// Today only the internal runtime honors this flag; the
        /// openclaw path emits its own JSON envelope regardless.
        #[arg(long)]
        json: bool,
        /// Which agent runtime to dispatch through. The default
        /// `internal` path is darkmux's in-house container-bounded
        /// Rust runtime at `runtime/` (Alpine docker container with
        /// kernel-enforced workspace path isolation). `openclaw`
        /// opts into the legacy shell-out path — available for
        /// operators who already use openclaw or want the runtime
        /// the article-series numbers were measured against. See
        /// `runtime/README.md` for the internal runtime's scope.
        #[arg(long, default_value = "internal")]
        runtime: String,
        /// Executable to invoke for the openclaw shell-out (Sprint-E
        /// replacement for the removed `DARKMUX_RUNTIME_CMD` env var).
        /// Defaults to `openclaw`; override to point at Aider, Cline,
        /// or any tool exposing the `<cmd> agent --agent <id> --json
        /// ...` calling convention. **Only consulted when `--runtime
        /// openclaw`** — the internal runtime ignores this flag.
        #[arg(long = "runtime-cmd", value_name = "PATH", default_value = "openclaw")]
        runtime_cmd: String,
        /// Advisory target machine for the dispatch (#246 PR-C.3). When
        /// set to an id that's NOT the local `DARKMUX_MACHINE_ID`, the
        /// dispatch is published to the single global fleet work queue
        /// (`darkmux:work`) and the first available runner picks it up.
        /// The id is an advisory hint (#590): any runner may claim it;
        /// a non-target runner logs a soft warning and proceeds. When
        /// omitted, the dispatch runs locally. Requires
        /// `DARKMUX_REDIS_URL` set on the dispatching machine +
        /// `darkmux serve` running on the runner.
        #[arg(long, value_name = "ID")]
        machine: Option<String>,
        /// Return immediately after publishing to the queue instead of
        /// blocking on the runner's `dispatch.complete` (#246 PR-C.3).
        /// Default is `--wait` (block) so today's "spawn, see result"
        /// ergonomics are preserved. With `--no-wait`, the CLI prints
        /// the `session_id` and exits 0; the operator polls completion
        /// via `darkmux flow tail --session <id>` (or `darkmux mission
        /// dispatch` for fan-out — PR-D). Ignored for local
        /// dispatches (those are always synchronous).
        #[arg(long)]
        no_wait: bool,
        /// (#703) Dispatch into a specific Docker image. Default:
        /// `darkmux-runtime:latest` (slim — python + node). Pass ANY Linux
        /// image (e.g. `rust:slim`, your project's own CI image) and darkmux
        /// injects its static runtime binary into it, so the coder runs in
        /// that environment and can `cargo check`/`test` in-sandbox — the
        /// inner verify loop. No per-language darkmux images. The image needs
        /// `bash` + coreutils (debian/ubuntu-family have them; bare-alpine
        /// needs them added). Local dispatch only: ignored on
        /// `--runtime openclaw` and on cross-machine `--machine` dispatch.
        #[arg(long, value_name = "TAG")]
        image: Option<String>,
    },
    /// Reconcile openclaw's `agents.list[]` with the crew role manifests.
    /// For every role with both a JSON manifest and a `.md` prompt, ensures
    /// a `darkmux/<role-id>` openclaw agent exists with the manifest's
    /// system prompt + tool palette. Idempotent.
    Sync {
        /// Skip the diff preview and write directly.
        #[arg(long, short = 'y')]
        yes: bool,
        /// Show what would change without writing.
        #[arg(long, short = 'n')]
        dry_run: bool,
    },
    /// SQLite-backed derived index over crew manifests (Phase B of #45).
    /// The index is derived state — JSON manifests under the crew root are
    /// the source of truth — and the `role`/`crew` read-verbs rebuild it on
    /// demand, so an explicit `rebuild` is rarely needed.
    Index {
        #[command(subcommand)]
        sub: CrewIndexCmd,
    },
}

#[derive(Subcommand)]
enum CrewIndexCmd {
    /// Rebuild the index from manifests on disk. Drops + recreates the
    /// derived tables, then repopulates — idempotent and self-healing across
    /// schema changes.
    Rebuild,
    /// Report index status: last-rebuild timestamp, source counts, drift.
    Status,
}

#[derive(Subcommand)]
enum RoleCmd {
    /// List every role in the index.
    List {
        /// Emit machine-readable JSON instead of styled text (#907).
        #[arg(long)]
        json: bool,
    },
    /// Show full details for a single role.
    Show {
        /// Role id to show.
        id: String,
        /// Emit machine-readable JSON instead of styled text (#907).
        #[arg(long)]
        json: bool,
    },
}

#[derive(Subcommand)]
enum SprintCmd {
    /// Pre-dispatch budget oracle. Reads a workload-spec JSON, computes
    /// predicted token consumption across planned turns, picks the
    /// smallest adequate profile, emits structured JSON.
    Estimate {
        /// Path to the workload-spec JSON file.
        spec: std::path::PathBuf,
        /// Add a one-sentence operator-facing recommendation wrap from the
        /// 4B compactor (`darkmux:qwen3-4b-instruct-2507`). Adds ~500ms
        /// latency; gracefully degrades if the model isn't loaded.
        #[arg(long)]
        narrate: bool,
    },
    /// Run a code review on the current branch vs base.  Auto-detects
    /// target, computes `git diff`, dispatches the `code-reviewer` role,
    /// parses the QA-REVIEW-SIGNOFF block, and emits structured JSON.
    Review {
        /// Base branch to diff against. Defaults to `main`.
        #[arg(long)]
        base: Option<String>,
        /// Exit nonzero if any BLOCK-severity findings.
        #[arg(long)]
        require_clean: bool,
        /// Optional sprint identifier passed through to flow records.
        #[arg(long = "sprint-id")]
        sprint_id: Option<String>,
    },
    /// Transition a sprint to `Running`. From `Planned` (first start) or
    /// `Abandoned` (restart — clears abandoned_ts). Stamps `started_ts=now()`.
    Start {
        /// Sprint id (filename stem under ~/.darkmux/sprints/).
        id: String,
    },
    /// Transition a `Running` sprint to `Complete` (terminal). Stamps
    /// `completed_ts=now()`. Wall-clock duration = completed_ts - started_ts.
    Complete { id: String },
    /// Transition a `Planned` or `Running` sprint to `Abandoned`. Operator-
    /// sovereign: only the operator marks a sprint dead; nothing auto-
    /// abandons on staleness. A subsequent `sprint start` clears the
    /// abandonment (operator changed their mind).
    Abandon { id: String },
}

#[derive(Subcommand)]
enum MissionCmd {
    /// Global mission-control read (#829): the whole board — every mission
    /// grouped by status with sprint progress, the inconsistencies that need
    /// attention (a Closed mission with a non-terminal sprint; an open mission
    /// whose sprints are all done), and copy-pasteable reconcile commands.
    /// READ-ONLY — surfaces and suggests, never mutates. The CLI twin of the
    /// viewer's missions lens; run it as session-start housekeeping.
    Status {
        /// Emit the board as structured JSON (for the frontier orchestrator
        /// or CI/cron) instead of the human-readable view.
        #[arg(long)]
        json: bool,
    },
    /// Debrief a mission (#1000) — the post-mission review ceremony's raw
    /// material in one place: the loop pathologies darkmux's detectors flagged
    /// across the mission's runs (cautions), the corrections the reviewer
    /// recorded (#849), and the mission's sprints + how each ended. READ-ONLY.
    /// Run it (or let the close nudge prompt it) at mission completion; the
    /// `darkmux-mission-debrief` skill consumes `--json` to distill durable
    /// `lessons` (with the why) for the next crew. NASA vocabulary:
    /// Mission · Crew · Debrief · Lessons.
    Debrief {
        /// Mission id (filename stem under ~/.darkmux/missions/).
        id: String,
        /// Emit the debrief material as structured JSON (for the
        /// `darkmux-mission-debrief` skill) instead of the human-readable view.
        #[arg(long)]
        json: bool,
    },
    /// Transition a mission to `Active`. Stamps `started_ts=now()` if not
    /// already set. Mission must be currently `Active` with no started_ts,
    /// OR — note: missions get created in `Active` status by convention,
    /// so this is the "I'm starting to work on it now" verb, not a status
    /// flip.
    Start {
        /// Mission id (filename stem under ~/.darkmux/missions/).
        id: String,
        /// Optional operator-supplied reasoning for the transition.
        /// Lands on the emitted flow record so the audit substrate
        /// captures *why* the state change happened.
        #[arg(long)]
        reasoning: Option<String>,
    },
    /// Transition a mission to `Closed` (terminal). From `Active` or `Paused`.
    /// Stamps `closed_ts=now()`.
    Close {
        id: String,
        /// Optional operator-supplied reasoning for closing the mission.
        #[arg(long)]
        reasoning: Option<String>,
    },
    /// Transition an `Active` mission to `Paused`. Stamps `paused_ts=now()`.
    Pause {
        id: String,
        /// Optional operator-supplied reasoning for pausing the mission.
        #[arg(long)]
        reasoning: Option<String>,
    },
    /// Transition a `Paused` mission back to `Active`. Does NOT clear
    /// `paused_ts` — the operator may want to see when the most recent
    /// pause occurred even after resuming.
    Resume {
        id: String,
        /// Optional operator-supplied reasoning for resuming the mission.
        #[arg(long)]
        reasoning: Option<String>,
    },
    /// Propose a Mission + Sprints from unstructured input (#113 Sprint 3).
    /// Dispatches the `mission-compiler` utility agent against the input,
    /// renders the proposal to the operator for approve/edit/reject/regen,
    /// and writes the JSONs only after approval. The operator approval
    /// gate is non-negotiable per operator-sovereignty (#44).
    ///
    /// Engagement context is intentionally NOT a CLI arg here — see
    /// CLAUDE.md's "Engagements (operator-defined dreamscapes)" section
    /// for doctrine. Operators carry engagement nuance into the input
    /// text itself (where the frontier orchestrator can thread it
    /// natively); the mission-compiler structures whatever's in the
    /// input without needing to interpret engagement.
    #[command(group(
        clap::ArgGroup::new("input_source").required(true).multiple(false)
    ))]
    Propose {
        /// Read the unstructured input from stdin. Useful for piping:
        /// `pbpaste | darkmux mission propose --from-stdin`.
        #[arg(long, group = "input_source")]
        from_stdin: bool,
        /// Read the unstructured input from a file path.
        #[arg(long, group = "input_source", value_name = "PATH")]
        from_file: Option<std::path::PathBuf>,
        /// Bypass the interactive approval flow and accept the first
        /// proposal as-is. Defaults to false — operator-approval gate
        /// is mandatory by default. Provided for non-interactive
        /// pipelines and tests.
        #[arg(long)]
        yes: bool,
        /// After approval, immediately invoke `darkmux mission start <id>`
        /// on the newly-persisted mission. Skips the manual two-step.
        /// Defaults to false — operators who want to inspect the persisted
        /// files before starting can omit this flag.
        #[arg(long)]
        start: bool,
        /// Work-item / ticket id this mission realizes (e.g. `SYS-2598`).
        /// Stamped on the mission record; referenced as `{ticket}` by the
        /// repo's `.darkmux/conventions.json` templates (#816) for branch
        /// names, commit subjects, and PR titles.
        #[arg(long, value_name = "ID")]
        ticket: Option<String>,
    },
    /// Add a new Sprint to an existing Mission mid-flight (#107).
    /// Operator-sovereign scope growth — alternative to either hand-
    /// editing JSON or filing a separate Mission for work that
    /// composes with the in-flight arc. Idempotent on exact-match
    /// (same id + mission + description); errors on collision or
    /// dangling depends_on.
    AddSprint {
        /// Mission id to extend (must exist).
        mission_id: String,
        /// Id for the new Sprint (must not collide with any existing
        /// sprint under a different mission; idempotent if same).
        #[arg(long = "sprint-id")]
        sprint_id: String,
        /// Description of the new Sprint's scope.
        #[arg(long)]
        description: String,
        /// Optional dependencies — other sprint ids that should
        /// complete first. Each must reference an existing sprint.
        #[arg(long = "depends-on")]
        depends_on: Vec<String>,
        /// Insert the new sprint immediately after this existing
        /// sprint id (insert-in-middle). When omitted, the new
        /// sprint is appended to the end of the mission's sprint
        /// list (queue-on-end). The named id must already be in
        /// the mission's sprint_ids — errors otherwise to surface
        /// typos and stale references.
        #[arg(long)]
        after: Option<String>,
        /// Optional operator-supplied reasoning for the mid-flight
        /// scope growth. Lands on the emitted flow record so the
        /// audit substrate captures *why* the mission grew here.
        #[arg(long)]
        reasoning: Option<String>,
    },
    /// Migrate mission + sprint storage from the pre-#148 flat layout
    /// (`<crew>/missions/<id>.json`, `<crew>/sprints/<id>.json`) into the
    /// per-mission nested layout (`<crew>/missions/<id>/mission.json`,
    /// `<crew>/missions/<id>/sprints/<sprint-id>.json`).
    ///
    /// Dry-run by default — prints the proposed moves without touching any
    /// files. Pass `--apply` to commit the migration. Idempotent: re-running
    /// after a successful apply is a no-op. Orphan sprints (whose
    /// `mission_id` has no matching mission on disk) are reported but never
    /// auto-moved; operator resolves them manually.
    Migrate {
        /// Apply the migration. Without this flag, only the proposed
        /// moves are printed (dry-run).
        #[arg(long)]
        apply: bool,
    },
    /// Fan-out dispatch all initial-depends sprints (depends_on=[]) of a
    /// mission across the fleet in parallel (#247, PR-D.1). One role
    /// applies to every dispatched sprint — operator-explicit per the
    /// CLAUDE.md doctrine that mission planning is judgment-bearing
    /// work the operator owns.
    ///
    /// Each sprint becomes a WorkJob published to the single global
    /// `darkmux:work` stream (#590); the first available runner claims
    /// and runs each one. Default `--wait` blocks until all sprints emit
    /// `dispatch.complete` (or timeout). `--no-wait` returns immediately
    /// with the session_ids for later polling.
    ///
    /// This is the keystone for Article 4's "operator hands off a
    /// mission and the fleet runs it" narrative.
    Dispatch {
        /// Mission id to dispatch.
        mission_id: String,
        /// Role to dispatch each sprint under (e.g. `coder`,
        /// `code-reviewer`). One role applies to every dispatched sprint.
        #[arg(long)]
        role: String,
        /// Optional advisory target machine for every sprint. When
        /// omitted, jobs publish with no `target_machine` hint — the
        /// first available runner claims each (pull semantics). The hint
        /// is advisory (#590): any runner may claim regardless.
        #[arg(long, value_name = "ID")]
        machine: Option<String>,
        /// Per-sprint dispatch timeout (seconds). Default 600.
        #[arg(long, default_value = "600")]
        timeout: u32,
        /// Return immediately after publishing all sprint jobs instead
        /// of blocking on each `dispatch.complete`. Default is `--wait`.
        #[arg(long)]
        no_wait: bool,
    },
    /// Run one sprint's dispatch-to-PR loop LOCALLY, up to the sign-off
    /// gate. Unlike `dispatch` (which fans sprints onto the fleet work
    /// queue), `run` is synchronous and single-sprint on this machine:
    /// it creates an isolated git worktree, dispatches the coder into it,
    /// runs the local `code-reviewer` QA against the diff, surfaces the
    /// result + tokens-off-meter + findings, then STOPS — nothing is
    /// committed, PR'd, or merged. Adjudicating findings and merging are
    /// gate steps for the operator/frontier (never auto-merge). After
    /// sign-off, `darkmux mission ship <id> --sprint <sprint-id>` finishes
    /// the loop. (#782)
    Run {
        /// Mission id to run a sprint from.
        mission_id: String,
        /// Sprint to run. Optional — when omitted, the single ready sprint
        /// (Planned, no unmet dependencies) is auto-selected; 0 or >1 ready
        /// sprints is ambiguous and bails asking for an explicit `--sprint`.
        #[arg(long, value_name = "ID")]
        sprint: Option<String>,
        /// Role to dispatch the sprint under. Default `coder`.
        #[arg(long, default_value = "coder")]
        role: String,
        /// Dispatch image. The default slim runtime image is used when
        /// omitted; naming a language image (e.g. `rust:latest`) makes
        /// darkmux inject its runtime binary so the coder can build/test
        /// in-sandbox (#703). For Rust in-sandbox lint, pick an image with
        /// the clippy component (`rust:latest` has it; bare `rust:slim`
        /// may not) — darkmux ships no per-language image.
        #[arg(long, value_name = "IMG")]
        image: Option<String>,
        /// Base ref the worktree branches off (and the QA diff compares
        /// against). Default `main`.
        #[arg(long, default_value = "main")]
        base: String,
        /// Coder dispatch timeout (seconds). Default 600.
        #[arg(long, default_value = "600")]
        timeout: u32,
    },
    /// Abort a `mission run` cleanly: remove the sprint's worktree + branch
    /// and flip the sprint to Abandoned. The explicit teardown for a run
    /// the operator/frontier decides to back out of (vs. leaving an orphan
    /// worktree). (#782)
    Abort {
        /// Mission id.
        mission_id: String,
        /// Sprint to abort. Optional — when omitted, the single ready sprint
        /// is selected (pass `--sprint` explicitly to abort a Running one).
        #[arg(long, value_name = "ID")]
        sprint: Option<String>,
    },
    /// Ship a `mission run`'s work: commit the worktree, push the branch,
    /// and open (or reuse) the PR. By DEFAULT stops at the PR — merging is
    /// the operator/frontier's explicit act (never auto-merge). `--wait-ci`
    /// blocks on CI; `--merge` (opt-in, green-gated) squash-merges, flips the
    /// sprint to Complete, and tears down the worktree. (#782)
    Ship {
        /// Mission id.
        mission_id: String,
        /// Sprint to ship. Optional — when omitted the single ready sprint
        /// is selected (pass `--sprint` explicitly for a Running sprint).
        #[arg(long, value_name = "ID")]
        sprint: Option<String>,
        /// Base branch the PR targets. Default `main`.
        #[arg(long, default_value = "main")]
        base: String,
        /// Block on CI checks until they finish, then report green/red.
        #[arg(long)]
        wait_ci: bool,
        /// After CI is green, squash-merge the PR, mark the sprint Complete,
        /// and remove the worktree. Opt-in — refuses to merge if CI isn't
        /// green. Without this flag, ship stops at the PR.
        #[arg(long)]
        merge: bool,
    },
}

#[derive(Subcommand)]
enum ModelCmd {
    /// Show models currently loaded in LMStudio, grouped by ownership:
    /// darkmux-managed (under the `darkmux:` namespace) vs user state
    /// (everything else). Read-only.
    Status {
        /// Emit machine-readable JSON instead of styled text (#907).
        #[arg(long)]
        json: bool,
    },
    /// Eject all darkmux-managed model loads (anything in the `darkmux:`
    /// namespace). User-loaded models are never touched. Use this when
    /// you want to release darkmux's RAM footprint without affecting
    /// other tools using LMStudio.
    Eject {
        /// Show what would be ejected without actually unloading.
        #[arg(long, short = 'n')]
        dry_run: bool,
    },
    /// Download the bake-off-validated models for the active hardware
    /// tier (per `templates/builtin/recommendations/<tier>.json`).
    /// Composes with `darkmux swap --recommended` — the swap verb errors
    /// loudly when the prescribed models aren't on disk; this verb is
    /// the fix-it.
    ///
    /// Skips models that are already downloaded. Errors with the
    /// recommendation's rationale when the active tier has no
    /// validated recommendation (pending-bake-off or no-recommendation
    /// status). (#159)
    PullRecommended,
}

#[derive(Subcommand)]
enum FleetCmd {
    /// Register a machine in the fleet roster. Idempotent — calling
    /// again with the same `<id>` updates fields but preserves the
    /// original `added_unix_ms` so the fleet-age signal stays honest.
    Add {
        /// Logical machine id (what flow records carry as `machine_id`).
        /// Example: `studio`, `laptop`, `mini-1`.
        id: String,
        /// Tailnet address or DNS name to reach the daemon on. Example:
        /// `100.74.208.36`, `100.74.208.36:8765`, `studio.tailnet`. If
        /// no `:port` suffix, port 8765 is assumed.
        #[arg(long)]
        address: String,
        /// Optional one-line description for `fleet status` + topology
        /// tooltips.
        #[arg(long)]
        description: Option<String>,
    },
    /// Remove a machine from the fleet roster. Doesn't touch the actual
    /// remote machine — just removes the local routing reference.
    /// Historical flow records from that machine remain in the audit
    /// chain and are still visible in the topology view.
    Remove {
        /// Logical machine id to remove.
        id: String,
    },
    /// Print the fleet roster + per-machine reachability. Each machine
    /// gets a TCP-probe to its daemon port (300ms budget per probe).
    /// `--deep` additionally fetches each reachable peer's spec sheet
    /// (RAM, CPU, loaded models, darkmux version) via the daemon's
    /// `/machine/specs` endpoint (#275). `--json` for scripting;
    /// default is a table for operator eyes.
    Status {
        /// Emit JSON instead of the human-readable table.
        #[arg(long)]
        json: bool,
        /// Aggregate `/machine/specs` from each reachable peer in
        /// addition to the reachability probe. Adds one HTTP GET per
        /// peer (~hundreds of ms over a tailnet).
        #[arg(long)]
        deep: bool,
    },
}

#[derive(Subcommand)]
enum ProfileCmd {
    /// Generate a starter profile JSON for a model + task class. Output is
    /// printed to stdout — copy-paste into your `~/.darkmux/profiles.json`
    /// (or pipe into a file) and tune from there.
    Draft {
        /// Profile name to use as the JSON key (e.g. "phi-fast").
        name: String,
        /// LMStudio modelKey for the primary. Run `lms ls` to see ids.
        #[arg(long, short = 'm')]
        model: String,
        /// Task class: `fast` (single-turn), `mid` (balanced), `long` (deep agentic).
        #[arg(long, short = 't', default_value = "mid")]
        task_class: String,
        /// Required when the model isn't in `lms ls` (i.e., not yet
        /// downloaded). Format: "4B", "13B", "35B", etc. Without this flag,
        /// drafting an unknown model would silently produce a Tiny-bucket
        /// profile (32K, no compactor) regardless of the model's real size.
        #[arg(long)]
        params: Option<String>,
        /// Override max context length (in tokens). Useful when the model
        /// isn't in `lms ls` and you want a draft that doesn't cap at the
        /// 32K default. Pair with --params for tight heuristics.
        #[arg(long)]
        max_ctx: Option<u32>,
    },
}

#[derive(Subcommand)]
enum NotebookCmd {
    /// Draft a notebook entry from a recorded run via the active role.
    Draft {
        run_id: String,
        /// DM role id to dispatch the drafting prompt through (Sprint-H
        /// rename from `--agent`; Beat 36 — DM concepts primary on
        /// DM-side surfaces). Resolves through
        /// `templates/builtin/roles/<role>.{json,md}` under the
        /// internal runtime; under `--runtime openclaw` it's passed
        /// verbatim to openclaw's `--agent <id>` flag.
        #[arg(long, default_value = "scribe")]
        role: String,
        /// Override the entry's filename slug (default derived from workload + run id).
        #[arg(long)]
        slug: Option<String>,
        /// Build the prompt and target filename without dispatching the role.
        #[arg(long, short = 'n')]
        dry_run: bool,
        /// Override the machine id (overrides DARKMUX_MACHINE_ID env var).
        #[arg(long)]
        machine: Option<String>,
        /// Which agent runtime to dispatch through. Default `internal`
        /// is darkmux's in-house container-bounded runtime (Sprint-H
        /// — Beat 36 finish); `openclaw` opts into the legacy
        /// shell-out path.
        #[arg(long, default_value = "internal")]
        runtime: String,
        /// Executable to invoke for the openclaw shell-out (Sprint-E
        /// pattern). Only consulted when `--runtime openclaw`.
        #[arg(long = "runtime-cmd", value_name = "PATH", default_value = "openclaw")]
        runtime_cmd: String,
    },
    /// List notebook entries (parsed from entry headers).
    ///
    /// Enumerates .md files in the notebook directory, reads each entry's
    /// `<!-- darkmux:notebook-entry: run=X machine=Y date=Z -->` header,
    /// and prints a summary table.  Optionally filter entries by machine.
    List {
        /// Only show entries from this machine (optional).
        #[arg(long)]
        machine: Option<String>,
    },
}

#[derive(Subcommand)]
enum LessonsCmd {
    /// Record an engagement-context lesson — a convention, constraint, or
    /// decision, WITH the reasoning behind it (explain the why, not just the
    /// rule). Appended to the durable `lessons.db`; surfaced to coder
    /// dispatches as a `<lessons>` block.
    Add {
        /// Short statement of the rule / decision.
        #[arg(long)]
        title: String,
        /// The detail — explain the WHY, not just the rule.
        #[arg(long)]
        body: String,
        /// Optional file scope (default: engagement-level — applies everywhere
        /// in this repo).
        #[arg(long)]
        file: Option<String>,
        /// Record into the cross-engagement user-global store
        /// (`~/.darkmux/lessons.db`) instead of this repo's. For conventions
        /// that apply to ALL your work (house style, language).
        #[arg(long)]
        global: bool,
    },
    /// List recorded lessons (this repo's + the user-global store,
    /// labeled by tier).
    List {
        /// Emit machine-readable JSON instead of styled text.
        #[arg(long)]
        json: bool,
    },
    /// Edit a recorded lesson in place by its id (from `lessons list --json`).
    /// Only the flags you pass change; `created_ts` is preserved.
    Edit {
        /// The lesson's rowid (ids are per-tier — pass `--global` to target the
        /// user-global store's ids).
        id: i64,
        /// New rule statement.
        #[arg(long)]
        title: Option<String>,
        /// New detail / why.
        #[arg(long)]
        body: Option<String>,
        /// Re-scope to a file.
        #[arg(long, conflicts_with = "clear_file")]
        file: Option<String>,
        /// Clear the file scope back to engagement-level (applies everywhere).
        #[arg(long)]
        clear_file: bool,
        /// Target the user-global store instead of this repo's.
        #[arg(long)]
        global: bool,
    },
    /// Remove a recorded lesson by its id (from `lessons list --json`).
    Remove {
        /// The lesson's rowid (per-tier — pass `--global` for the global store).
        id: i64,
        /// Target the user-global store instead of this repo's.
        #[arg(long)]
        global: bool,
    },
    /// Export a tier's lessons to a self-describing JSON envelope on stdout —
    /// for a hand-edit / git-commit / restore roundtrip.
    Export {
        /// Export the user-global store instead of this repo's.
        #[arg(long)]
        global: bool,
    },
    /// Import a previously-exported (or hand-authored) JSON envelope into a
    /// tier. Upserts by id (idempotent re-import; new entries append); never
    /// deletes. Reads stdin when `--file` is omitted.
    Import {
        /// Path to the JSON envelope (omit to read stdin).
        #[arg(long)]
        file: Option<std::path::PathBuf>,
        /// Import into the user-global store instead of this repo's.
        #[arg(long)]
        global: bool,
    },
    /// Read-only recall: search recorded lessons (both tiers) by a
    /// case-insensitive term and/or an exact file scope. Results span both
    /// tiers; ids are tier-local, so to edit/remove a hit, target its tier
    /// (`--global` for global-store ids).
    Recall {
        /// Case-insensitive substring matched against title OR body.
        #[arg(long)]
        term: Option<String>,
        /// Exact file scope to filter on.
        #[arg(long)]
        file: Option<String>,
        /// Emit machine-readable JSON instead of styled text.
        #[arg(long)]
        json: bool,
    },
}

#[derive(Subcommand)]
enum SkillsCmd {
    /// Copy bundled skills into a Claude Code (or compatible) skills dir.
    Install {
        /// Target dir (default: ~/.claude/skills/darkmux/).
        #[arg(long)]
        target: Option<std::path::PathBuf>,
        /// Overwrite existing SKILL.md files.
        #[arg(long, short = 'f')]
        force: bool,
        /// Show what would be installed without writing.
        #[arg(long, short = 'n')]
        dry_run: bool,
    },
    /// List currently-installed skills under the target dir.
    List {
        #[arg(long)]
        target: Option<std::path::PathBuf>,
    },
}

#[derive(Subcommand)]
enum LabCmd {
    /// List available workloads.
    Workloads,
    /// Run a workload (one or more times).
    Run {
        workload: String,
        #[arg(long, short = 'p')]
        profile: Option<String>,
        #[arg(long, short = 'n', default_value = "1")]
        runs: u32,
        /// Profiles-registry path (profiles.json). Overrides DARKMUX_PROFILES
        /// and the default search locations. (renamed from --config, #661)
        #[arg(long = "profiles-file")]
        profiles: Option<String>,
        #[arg(long, short = 'q')]
        quiet: bool,
        /// Which agent runtime to dispatch through. Default `internal`
        /// is darkmux's in-house container-bounded Rust runtime.
        /// `openclaw` opts into the legacy shell-out path. Beat 36
        /// directional principle: openclaw is a downstream translation
        /// target the operator opts into per dispatch.
        #[arg(long, default_value = "internal")]
        runtime: String,
        /// Executable to invoke for the openclaw shell-out (Sprint-E
        /// replacement for the removed `DARKMUX_RUNTIME_CMD` env var).
        /// Defaults to `openclaw`; override to point at Aider, Cline,
        /// or any tool exposing the `<cmd> agent --message` calling
        /// convention. **Only consulted when `--runtime openclaw`** —
        /// the internal runtime ignores this flag.
        #[arg(long = "runtime-cmd", value_name = "PATH", default_value = "openclaw")]
        runtime_cmd: String,
    },
    /// Loop lab (#986) — run ONE dispatch under a chosen harness config and
    /// classify how the loop behaved: productive / struggled / inert-false-pass
    /// / failed. The loop-engineering bench — vary the HARNESS (turn/token
    /// caps + compaction knobs) against a fixed model + fixture and see which
    /// loop config catches or survives the struggle. The model axis comes from
    /// the profile (`--profile` / `--profiles-file`); the loop axis from the
    /// override flags below.
    Loop {
        /// Workload to dispatch (a coding-task / fixture-backed workload —
        /// that's where loop behavior is interesting).
        workload: String,
        /// Profile (the model axis) — defaults to the registry's default_profile.
        #[arg(long, short = 'p')]
        profile: Option<String>,
        /// Profiles-registry path (profiles.json). Overrides DARKMUX_PROFILES
        /// and the default search locations (#984 makes this reach the
        /// dispatch's model resolution).
        #[arg(long = "profiles-file")]
        profiles: Option<String>,
        // ── loop-variation axis 1: caps ──────────────────────────────
        // Applied via the documented live env-override tier
        // (`DARKMUX_RUNTIME_MAX_TURNS` / `_MAX_TOKENS` /
        // `DARKMUX_INACTIVITY_TIMEOUT_SECONDS`) for this dispatch only.
        /// Cap the agent loop at N turns (overrides profile/config).
        #[arg(long = "max-turns")]
        max_turns: Option<u32>,
        /// Cap cumulative completion tokens at N (overrides profile/config).
        #[arg(long = "max-tokens")]
        max_tokens: Option<u32>,
        /// Inactivity-watchdog window in seconds (the per-dispatch
        /// no-proof-of-work timeout).
        #[arg(long = "timeout")]
        timeout: Option<u64>,
        // ── loop-variation axis 2: compaction ────────────────────────
        // Overlaid on the resolved profile's compaction config for this run.
        /// Compaction absolute trigger (tokens).
        #[arg(long = "compact-threshold-tokens")]
        compact_threshold_tokens: Option<u32>,
        /// Compaction adaptive trigger fraction (0.1–0.9).
        #[arg(long = "compact-threshold-ratio")]
        compact_threshold_ratio: Option<f32>,
        /// Compaction strategy: `narrative` or `structured-slot`.
        #[arg(long = "compact-strategy")]
        compact_strategy: Option<String>,
        /// Escalate + exit after this many compactions.
        #[arg(long = "bail-after-compactions")]
        bail_after_compactions: Option<u32>,
        /// Context window (tokens) the compaction formula trigger uses.
        #[arg(long = "context-window")]
        context_window: Option<u32>,
        // ── (#1004) engagement-context A/B ──────────────────────────
        /// Run the workload TWICE — once WITH the engagement-context blocks
        /// (lessons + detected cautions) injected into the prompt, once
        /// WITHOUT — and report the verdict shift. Validates the doom-loop
        /// cure: does injected institutional memory change loop behavior?
        #[arg(long)]
        ab: bool,
        /// Scope the injected cautions + corrections to this mission's
        /// dispatches (its `mission-run-<id>-<sprint>` sessions). Without it,
        /// only the repo's authored lessons inject. Requires `--ab` (clap
        /// errors otherwise, so the flag is never a silent no-op).
        #[arg(long = "inject-from-mission", requires = "ab")]
        inject_from_mission: Option<String>,
        /// Emit the loop report as JSON instead of the human table.
        #[arg(long)]
        json: bool,
    },
    /// List recent runs (most recent first).
    Runs {
        /// Show at most N runs (default: 5).
        #[arg(long, short = 'l', default_value = "5")]
        limit: usize,
        /// Show all runs (overrides --limit).
        #[arg(long, short = 'a')]
        all: bool,
    },
    /// Inspect a previously-recorded run.
    Inspect {
        run: String,
        /// Also dump the full compaction summary text(s) the compactor model
        /// wrote during this run (read from trajectory.jsonl). Useful for
        /// methodology validation — confirming the compactor is producing
        /// substantive summaries rather than degenerate / empty output.
        #[arg(long)]
        summary: bool,
    },
    /// Compare two runs.
    Compare { run_a: String, run_b: String },
    /// Run an opinionated single-command characterization of the local setup.
    /// Dispatches a single workload (default `quick-q`) on the active profile
    /// and returns a one-screen verdict — wall clock, verify outcome, hint at
    /// next steps. The "QA my Mac" entry point.
    Characterize {
        /// Workload to dispatch (default: quick-q smoke prompt).
        #[arg(default_value = "quick-q")]
        workload: String,
        #[arg(long, short = 'p')]
        profile: Option<String>,
        /// Profiles-registry path (profiles.json). Overrides DARKMUX_PROFILES
        /// and the default search locations. (renamed from --config, #661)
        #[arg(long = "profiles-file")]
        profiles: Option<String>,
    },
    /// Multi-run distribution characterization with bimodal cluster detection.
    /// Run a workload N times on a profile, then report fast cluster / slow
    /// cluster / slow-rate. The bimodal model captures the variance shape of
    /// long-agentic dispatches better than a naive mean ± stdev would.
    Tune {
        workload: String,
        #[arg(long, short = 'p')]
        profile: Option<String>,
        /// Number of dispatches (default 6 — enough for a meaningful bimodal
        /// signal without burning hours on Apple Silicon).
        #[arg(long, short = 'n', default_value = "6")]
        runs: u32,
        /// Profiles-registry path (profiles.json). Overrides DARKMUX_PROFILES
        /// and the default search locations. (renamed from --config, #661)
        #[arg(long = "profiles-file")]
        profiles: Option<String>,
    },
    /// Register a fixture directory in the lab registry by name (#491).
    /// Reads `.fixture.json` from `<path>`, computes a BLAKE3 content
    /// hash, records the pointer in `~/.darkmux/lab-registry.json`.
    /// The dir itself stays where it is — registry is just a lookup
    /// table.
    Register {
        /// Path to the fixture directory (must contain `.fixture.json`).
        path: std::path::PathBuf,
        /// Override the manifest's name field (registry key).
        #[arg(long)]
        name: Option<String>,
        /// Replace an existing registry entry with the same name.
        /// Without this, duplicate names error out.
        #[arg(long)]
        force: bool,
        /// Idempotent register: if the fixture is already registered,
        /// skip with a no-op success instead of erroring. Lets scripts
        /// (e.g. scripts/lab-init.sh) re-run cleanly without parsing
        /// error text. Ignored when `--force` is also passed.
        #[arg(long = "if-absent")]
        if_absent: bool,
    },
    /// Remove a fixture from the lab registry by name (#491).
    /// NEVER touches the underlying directory — operator-sovereignty
    /// preserved.
    Unregister {
        /// Registry key (name from `.fixture.json` or `--name` at
        /// register time).
        name: String,
    },
    /// List registered fixtures + their paths + hashes (#491).
    Fixtures,
    /// Lint the lab registry — schema check, path existence, content
    /// hash recompute, required-files presence (#491). Cheap + offline:
    /// no dispatches, no network. Doctor is the discoverability layer
    /// for the lab subsystem.
    Doctor,
}

fn main() -> Result<()> {
    providers::register_builtins()?;
    let cli = Cli::parse();
    let code = run(cli.command)?;
    std::process::exit(code);
}

fn run(cmd: Cmd) -> Result<i32> {
    match cmd {
        Cmd::External { sub } => match sub {
            ExternalCmd::Pull { gh, url, stdin } => {
                external::pull(gh.as_deref(), url.as_deref(), stdin)?;
                Ok(0)
            }
        },
        Cmd::Swap {
            profile,
            profiles,
            dry_run,
            quiet,
            runtime,
            recommended,
        } => cmd_swap(profile.as_deref(), profiles.as_deref(), dry_run, quiet, &runtime, recommended),
        Cmd::Status { profiles, json } => cmd_status(profiles.as_deref(), json),
        Cmd::Profiles { profiles, json } => cmd_profiles(profiles.as_deref(), json),
        Cmd::Lab { sub } => cmd_lab(sub),
        Cmd::Skills { sub } => cmd_skills(sub),
        Cmd::Notebook { sub } => cmd_notebook(sub),
        Cmd::Doctor { fix, include_openclaw } => cmd_doctor(fix, include_openclaw),
        Cmd::Scan { profiles } => cmd_scan(profiles.as_deref()),
        Cmd::Profile { sub } => cmd_profile(sub),
        Cmd::Model { sub } => cmd_model(sub),
        Cmd::Fleet { sub } => cmd_fleet(sub),
        Cmd::Crew { sub } => cmd_crew(sub),
        Cmd::Lessons { sub } => cmd_lessons(sub),
        Cmd::Role { sub } => cmd_role(sub),
        Cmd::Sprint { sub } => cmd_sprint(sub),
        Cmd::Mission { sub } => cmd_mission(sub),
        Cmd::Recommendations { sub } => cmd_recommendations(sub),
        Cmd::Flow { sub } => {
            flow_cli::run(sub)?;
            Ok(0)
        }
        Cmd::Init {
            with_hook,
            with_claude_md,
            with_agents_md,
            force,
            dry_run,
        } => cmd_init(with_hook, with_claude_md, with_agents_md, force, dry_run),
        Cmd::Serve {
            port,
            bind,
            flows_dir,
        } => {
            let flows_dir = flows_dir.unwrap_or_else(crate::flow::flows_dir);
            serve::run(port, bind, flows_dir)?;
            Ok(0)
        }
        Cmd::Optimize => optimize::run(),
    }
}

fn cmd_lessons(sub: LessonsCmd) -> Result<i32> {
    use darkmux_crew::lessons;
    match sub {
        LessonsCmd::Add {
            title,
            body,
            file,
            global,
        } => {
            let (path, tier) = if global {
                (lessons::global_db_path(), "global")
            } else {
                (lessons::repo_db_path(), "repo")
            };
            let conn = lessons::open_at(&path)?;
            lessons::add(&conn, &title, &body, file.as_deref(), None)?;
            println!(
                "{}",
                darkmux_types::style::success(&format!("recorded lesson ({tier}): {title}"))
            );
            println!("{}", darkmux_types::style::dim(&format!("  {}", path.display())));
            Ok(0)
        }
        LessonsCmd::List { json } => {
            let repo_path = lessons::repo_db_path();
            let global_path = lessons::global_db_path();
            let repo = lessons::load_entries_best_effort(&repo_path);
            // When `$DARKMUX_HOME` collapses both tiers to one root the paths are
            // identical — read once, don't double-display the same entries.
            let global = if global_path == repo_path {
                Vec::new()
            } else {
                lessons::load_entries_best_effort(&global_path)
            };

            if json {
                let out = serde_json::json!({ "repo": repo, "global": global });
                println!("{}", serde_json::to_string_pretty(&out)?);
                return Ok(0);
            }
            if repo.is_empty() && global.is_empty() {
                println!(
                    "{}",
                    darkmux_types::style::dim(
                        "no lessons recorded yet — darkmux lessons add --title <t> --body <b>"
                    )
                );
                return Ok(0);
            }
            print_lessons_tier("repo (this engagement)", &repo);
            print_lessons_tier("global (all engagements)", &global);
            Ok(0)
        }
        LessonsCmd::Edit {
            id,
            title,
            body,
            file,
            clear_file,
            global,
        } => {
            let (path, tier) = lessons_tier(global);
            // tri-state: --clear-file wins (Some(None)); else --file (Some(Some));
            // else leave unchanged (None).
            let file_update: Option<Option<&str>> = if clear_file {
                Some(None)
            } else {
                file.as_deref().map(Some)
            };
            if title.is_none() && body.is_none() && file_update.is_none() {
                eprintln!(
                    "{}",
                    darkmux_types::style::error(
                        "nothing to edit — pass at least one of --title / --body / --file / --clear-file"
                    )
                );
                return Ok(2);
            }
            let conn = lessons::open_at(&path)?;
            let changed = lessons::edit(
                &conn,
                id,
                title.as_deref(),
                body.as_deref(),
                file_update,
                None,
            )?;
            if changed {
                println!(
                    "{}",
                    darkmux_types::style::success(&format!("edited lesson #{id} ({tier})"))
                );
                Ok(0)
            } else {
                eprintln!(
                    "{}",
                    darkmux_types::style::error(&format!(
                        "no lesson #{id} in the {tier} store (ids are per-tier — try --global?)"
                    ))
                );
                Ok(1)
            }
        }
        LessonsCmd::Remove { id, global } => {
            let (path, tier) = lessons_tier(global);
            let conn = lessons::open_at(&path)?;
            if lessons::remove(&conn, id)? {
                println!(
                    "{}",
                    darkmux_types::style::success(&format!("removed lesson #{id} ({tier})"))
                );
                Ok(0)
            } else {
                eprintln!(
                    "{}",
                    darkmux_types::style::error(&format!(
                        "no lesson #{id} in the {tier} store (ids are per-tier — try --global?)"
                    ))
                );
                Ok(1)
            }
        }
        LessonsCmd::Export { global } => {
            let (path, _) = lessons_tier(global);
            // export reads the store; if absent, emit an empty envelope rather
            // than creating the db (a read must not write). Build the envelope
            // through `LessonsExport` so the wire shape is single-sourced with
            // `import_json` — the two can't drift.
            let env = lessons::LessonsExport {
                schema_version: lessons::LESSONS_SCHEMA_VERSION,
                lessons: lessons::load_entries_best_effort(&path),
            };
            println!("{}", serde_json::to_string_pretty(&env)?);
            Ok(0)
        }
        LessonsCmd::Import { file, global } => {
            let (path, tier) = lessons_tier(global);
            let data = match file {
                Some(p) => std::fs::read_to_string(&p)
                    .with_context(|| format!("reading {}", p.display()))?,
                None => {
                    use std::io::Read;
                    let mut buf = String::new();
                    std::io::stdin()
                        .read_to_string(&mut buf)
                        .context("reading lessons import from stdin")?;
                    buf
                }
            };
            let mut conn = lessons::open_at(&path)?;
            let stats = lessons::import_json(&mut conn, &data)?;
            println!(
                "{}",
                darkmux_types::style::success(&format!(
                    "imported into {tier}: {} inserted, {} updated",
                    stats.inserted, stats.updated
                ))
            );
            Ok(0)
        }
        LessonsCmd::Recall { term, file, json } => {
            let repo_path = lessons::repo_db_path();
            let global_path = lessons::global_db_path();
            let recall_tier = |path: &std::path::Path| -> Vec<lessons::Lesson> {
                if !path.exists() {
                    return Vec::new();
                }
                lessons::open_at(path)
                    .and_then(|conn| lessons::recall(&conn, term.as_deref(), file.as_deref()))
                    .unwrap_or_default()
            };
            let repo = recall_tier(&repo_path);
            let global = if global_path == repo_path {
                Vec::new()
            } else {
                recall_tier(&global_path)
            };
            if json {
                let out = serde_json::json!({ "repo": repo, "global": global });
                println!("{}", serde_json::to_string_pretty(&out)?);
                return Ok(0);
            }
            if repo.is_empty() && global.is_empty() {
                println!("{}", darkmux_types::style::dim("no lessons match"));
                return Ok(0);
            }
            print_lessons_tier("repo (this engagement)", &repo);
            print_lessons_tier("global (all engagements)", &global);
            Ok(0)
        }
    }
}

/// Resolve the `(db path, label)` for a lessons tier — repo (this engagement)
/// by default, user-global when `--global`. Shared by the mutating verbs.
fn lessons_tier(global: bool) -> (std::path::PathBuf, &'static str) {
    use darkmux_crew::lessons;
    if global {
        (lessons::global_db_path(), "global")
    } else {
        (lessons::repo_db_path(), "repo")
    }
}

fn print_lessons_tier(label: &str, entries: &[darkmux_crew::lessons::Lesson]) {
    if entries.is_empty() {
        return;
    }
    println!("{}", darkmux_types::style::header(label));
    for e in entries {
        let scope = e
            .file
            .as_deref()
            .map(|f| format!(" [{f}]"))
            .unwrap_or_default();
        println!(
            "  {}{}",
            darkmux_types::style::accent(&e.title),
            darkmux_types::style::dim(&scope)
        );
        println!("    {}", e.body);
    }
}

fn cmd_notebook(sub: NotebookCmd) -> Result<i32> {
    match sub {
        NotebookCmd::Draft {
            run_id,
            role,
            slug,
            dry_run,
            machine,
            runtime,
            runtime_cmd,
        } => {
            let runtime_flag = crew::dispatch::Runtime::parse(&runtime)?;
            // Sprint-E loud-bail gate: --runtime-cmd is only valid under
            // --runtime openclaw. Silent ignore would re-introduce the
            // implicit-state pattern Sprint-E removed.
            if runtime_flag != crew::dispatch::Runtime::Openclaw && runtime_cmd != "openclaw" {
                anyhow::bail!(
                    "--runtime-cmd `{runtime_cmd}` is only valid with --runtime openclaw \
                     (got --runtime {runtime}). Either drop --runtime-cmd, or add --runtime openclaw."
                );
            }
            let report = notebook::draft_entry(&notebook::DraftOptions {
                run_id,
                role,
                slug,
                dry_run,
                machine_override: machine,
                runtime: runtime_flag,
                runtime_cmd,
            })?;
            println!("source run: {}", report.run_dir.display());
            println!("entry path: {}", report.entry_path.display());
            println!("prompt chars: {}", report.prompt_chars);
            println!("reply chars:  {}", report.reply_chars);
            if dry_run {
                println!("[DRY RUN — nothing was written]");
            }
            Ok(0)
        }
        NotebookCmd::List { machine } => {
            // env > config.dirs.notebook > <root>/notebook (#661 Slice 3).
            let notebook_dir = darkmux_types::config_access::notebook_dir();
            if !notebook_dir.exists() {
                // (#895) An absent notebook dir is "nothing to list", not an
                // error — a fresh user, or `notebook list && …` chaining,
                // must not see a false failure. Mirrors the empty-dir case
                // below (also Ok(0)).
                println!("no notebook directory yet: {}", notebook_dir.display());
                return Ok(0);
            }
            let entries = notebook::list_entries(&notebook_dir, machine.as_deref())?;
            if entries.is_empty() {
                println!("no notebook entries found");
                return Ok(0);
            }
            // Column widths (dynamic based on longest value).
            let max_date: usize = entries.iter().map(|e| e.date.len()).max().unwrap_or(10);
            let max_machine: usize = entries.iter().map(|e| e.machine.len()).max().unwrap_or(10);
            let max_run: usize = entries.iter().map(|e| e.run.len()).max().unwrap_or(12);
            for entry in &entries {
                println!(
                    "{date:<width_date$}  {machine:<width_machine$}  {run:<width_run$}  {path}",
                    date = entry.date,
                    width_date = max_date.max(4),
                    machine = entry.machine,
                    width_machine = max_machine.max(8),
                    run = entry.run,
                    width_run = max_run.max(4),
                    path = entry.path.display(),
                );
            }
            Ok(0)
        }
    }
}

fn cmd_doctor(fix: bool, include_openclaw: bool) -> Result<i32> {
    let report = doctor::run(include_openclaw);
    doctor::print_report(&report)?;

    // --fix path: attempt known-safe auto-fixes for failing/warning rules,
    // then re-run the full check set so the operator sees the post-fix
    // state. Without --fix, doctor is read-only — exits based on `report`.
    let final_report = if fix {
        let outcomes = doctor::try_fix(&report)?;
        if outcomes.is_empty() {
            println!();
            println!("--fix: no auto-fix available for any failing or warning check.");
            report
        } else {
            println!();
            println!("--fix: applied {} auto-fix(es):", outcomes.len());
            for o in &outcomes {
                let marker = if o.applied { "✓" } else { "·" };
                println!("  {marker} {} — {}", o.rule_id, o.message);
            }
            println!();
            println!("Re-running doctor…");
            println!();
            let report2 = doctor::run(include_openclaw);
            doctor::print_report(&report2)?;
            report2
        }
    } else {
        report
    };

    Ok(match final_report.worst_status() {
        doctor::Status::Fail => 1,
        _ => 0,
    })
}

/// Surface LMStudio models not yet covered by any profile, with task-class
/// hints and a one-liner reason per model. Helps a user discover that a
/// freshly-downloaded model could be added to the registry.
fn cmd_scan(config: Option<&str>) -> Result<i32> {
    // Distinguish "no registry yet" (silent empty — fresh user) from
    // "registry exists but failed to parse / validate" (warn loudly so the
    // user knows their registry is broken — silent fallthrough would
    // misleadingly flag every loaded model as uncovered).
    let registry_loaded = match profiles::load_registry(config) {
        Ok(r) => Some(r),
        Err(e) => {
            let msg = e.to_string();
            // Heuristic: "not found" / "no profile registry" → first-run case;
            // anything else is a real load failure worth surfacing.
            if msg.contains("no profile registry") {
                None
            } else {
                eprintln!("warning: profile registry could not be loaded — {msg}");
                eprintln!("         continuing as if no profiles are defined.");
                None
            }
        }
    };
    let covered: std::collections::HashSet<String> = match registry_loaded.as_ref() {
        Some(r) => r
            .registry
            .profiles
            .values()
            .flat_map(|p| p.models.iter().map(|m| m.id.clone()))
            .collect(),
        None => std::collections::HashSet::new(),
    };

    let available = lms::list_available()?;
    let llms: Vec<&lms::ModelMeta> = available.iter().filter(|m| m.model_type == "llm").collect();

    let uncovered: Vec<&lms::ModelMeta> = llms
        .iter()
        .filter(|m| !covered.contains(&m.model_key))
        .copied()
        .collect();

    println!(
        "{}",
        darkmux_types::style::header(&format!(
            "darkmux scan — {} model(s) in LMStudio, {} not yet in any profile",
            llms.len(),
            uncovered.len()
        ))
    );
    if uncovered.is_empty() {
        if !llms.is_empty() {
            println!();
            println!("All loaded LLMs are already covered. Nothing to suggest.");
        }
        return Ok(0);
    }

    // Pre-pass: detect derived-name collisions between uncovered models.
    // Two models with different publishers but the same base name (e.g.
    // unsloth/Qwen-7B and lmstudio-community/Qwen-7B) would each draft into
    // the same profile name and silently clobber each other in the registry.
    let mut name_collisions: std::collections::HashMap<String, usize> =
        std::collections::HashMap::new();
    for m in &uncovered {
        let bucket = heuristics::classify_size_from_meta(m);
        let suggested_class = match bucket {
            heuristics::SizeBucket::Tiny => heuristics::TaskClass::Fast,
            heuristics::SizeBucket::Small => heuristics::TaskClass::Mid,
            heuristics::SizeBucket::Medium => heuristics::TaskClass::Long,
            heuristics::SizeBucket::Large => heuristics::TaskClass::Mid,
            heuristics::SizeBucket::Xl => heuristics::TaskClass::Fast,
        };
        let name = derive_profile_name(&m.model_key, suggested_class);
        *name_collisions.entry(name).or_insert(0) += 1;
    }

    println!();
    for m in &uncovered {
        let bucket = heuristics::classify_size_from_meta(m);
        let arch = heuristics::classify_architecture(m);
        let suggested_class = match bucket {
            heuristics::SizeBucket::Tiny => heuristics::TaskClass::Fast,
            heuristics::SizeBucket::Small => heuristics::TaskClass::Mid,
            heuristics::SizeBucket::Medium => heuristics::TaskClass::Long,
            heuristics::SizeBucket::Large => heuristics::TaskClass::Mid,
            heuristics::SizeBucket::Xl => heuristics::TaskClass::Fast,
        };
        let suggestion = heuristics::suggest_profile(m, suggested_class);
        let size_gb = (m.size_bytes as f64) / (1024.0 * 1024.0 * 1024.0);
        let display = if m.display_name.is_empty() {
            m.model_key.clone()
        } else {
            m.display_name.clone()
        };

        let icon = if m.trained_for_tool_use {
            darkmux_types::style::success("✓")
        } else {
            darkmux_types::style::warn("⚠")
        };
        println!("{} {}", icon, darkmux_types::style::accent(&display));
        println!("    {}",
            darkmux_types::style::dim(&format!(
                "id={}  params={}  arch={:?}  size={:.1}GB  maxCtx={}",
                m.model_key,
                m.params_string.as_deref().unwrap_or("?"),
                arch,
                size_gb,
                m.max_context_length.unwrap_or(0)
            ))
        );
        println!(
            "    suggested task class: `{}` (n_ctx={}, compactor={})",
            darkmux_types::style::accent(suggested_class.as_str()),
            suggestion.primary_n_ctx,
            suggestion
                .compactor
                .as_ref()
                .map(|c| format!("{} @ {}", c.model_id, c.n_ctx))
                .unwrap_or_else(|| "none".into())
        );
        if !m.trained_for_tool_use {
            println!("    {}", darkmux_types::style::warn(
                "⚠ NOT marked trainedForToolUse — agentic dispatch may be unreliable"
            ));
        }
        let safe_name = derive_profile_name(&m.model_key, suggested_class);
        if name_collisions.get(&safe_name).copied().unwrap_or(0) > 1 {
            println!(
                "    {}",
                darkmux_types::style::warn(&format!(
                    "⚠ derived name `{safe_name}` collides with another uncovered model — \
                     customize the name when drafting (publisher prefix gets stripped)"
                ))
            );
        }
        println!(
            "    draft: `darkmux profile draft {safe_name} --model {} --task-class {}`",
            m.model_key,
            suggested_class.as_str()
        );
        println!();
    }
    Ok(0)
}

/// Compose a sensible default profile name from a model id + task class.
/// Strips publisher prefixes (e.g. `mlx-community/`), lowercases, replaces
/// underscores/spaces with dashes, drops anything that isn't alphanumeric +
/// dash + dot. Trims leading/trailing dashes; if the result starts with a
/// non-alphanumeric, prepends "model".
fn derive_profile_name(model_id: &str, task: heuristics::TaskClass) -> String {
    let last_segment = model_id.rsplit('/').next().unwrap_or(model_id);
    let cleaned: String = last_segment
        .chars()
        .map(|c| {
            if c == '_' || c == ' ' {
                '-'
            } else {
                c.to_ascii_lowercase()
            }
        })
        .filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '.')
        .collect();
    let trimmed = cleaned.trim_matches('-').to_string();
    let safe_base = if trimmed.is_empty()
        || !trimmed
            .chars()
            .next()
            .map(|c| c.is_ascii_alphanumeric())
            .unwrap_or(false)
    {
        format!("model-{}", trimmed.trim_start_matches('-'))
    } else {
        trimmed
    };
    format!("{}-{}", safe_base, task.as_str())
}

/// True if the model id has a publisher prefix that gets stripped by
/// `derive_profile_name`. Reserved for future per-model warnings; the
/// scan currently catches collisions globally instead.
#[allow(dead_code)]
fn has_stripped_publisher(model_id: &str) -> bool {
    model_id.contains('/')
}

fn cmd_role(sub: RoleCmd) -> Result<i32> {
    match sub {
        RoleCmd::List { json } => role_cli::role_list(json),
        RoleCmd::Show { id, json } => role_cli::role_show(&id, json),
    }
}

fn cmd_sprint(sub: SprintCmd) -> Result<i32> {
    match sub {
        SprintCmd::Estimate { spec, narrate } => sprint_cli::estimate(&spec, narrate),
        SprintCmd::Review {
            base,
            require_clean,
            sprint_id,
        } => {
            let sid = sprint_id.as_deref();
            sprint_cli::sprint_review(base.as_deref(), require_clean, sid)
        }
        SprintCmd::Start { id } => {
            let s = crew::lifecycle::sprint_start(&id)?;
            println!(
                "sprint `{}` → Running  started_ts={}",
                s.id,
                s.started_ts.unwrap_or(0)
            );
            Ok(0)
        }
        SprintCmd::Complete { id } => {
            let s = crew::lifecycle::sprint_complete(&id)?;
            let started = s.started_ts.unwrap_or(0);
            let completed = s.completed_ts.unwrap_or(0);
            let dur = completed.saturating_sub(started);
            println!(
                "sprint `{}` → Complete  duration={}s  completed_ts={}",
                s.id, dur, completed
            );
            Ok(0)
        }
        SprintCmd::Abandon { id } => {
            let s = crew::lifecycle::sprint_abandon(&id)?;
            println!(
                "sprint `{}` → Abandoned  abandoned_ts={}",
                s.id,
                s.abandoned_ts.unwrap_or(0)
            );
            Ok(0)
        }
    }
}

fn cmd_mission(sub: MissionCmd) -> Result<i32> {
    match sub {
        MissionCmd::Status { json } => mission_status::run(json),
        MissionCmd::Debrief { id, json } => mission_run::debrief(&id, json),
        MissionCmd::Start { id, reasoning } => {
            let m = crew::lifecycle::mission_start_with_reasoning(&id, reasoning.as_deref())?;
            println!(
                "mission `{}` → Active  started_ts={}",
                m.id,
                m.started_ts.unwrap_or(0)
            );
            Ok(0)
        }
        MissionCmd::Close { id, reasoning } => {
            let m = crew::lifecycle::mission_close_with_reasoning(&id, reasoning.as_deref())?;
            let started = m.started_ts.unwrap_or(0);
            let closed = m.closed_ts.unwrap_or(0);
            let dur = closed.saturating_sub(started);
            println!(
                "mission `{}` → Closed  duration={}s  closed_ts={}",
                m.id, dur, closed
            );
            // (#1000) The mission's done — prompt the debrief ceremony so its
            // transient signal (cautions + corrections) becomes durable lessons
            // for the next crew. Emits Stage::Debrief; never blocks the close.
            mission_run::nudge_mission_debrief(&m.id);
            Ok(0)
        }
        MissionCmd::Pause { id, reasoning } => {
            let m = crew::lifecycle::mission_pause_with_reasoning(&id, reasoning.as_deref())?;
            println!(
                "mission `{}` → Paused  paused_ts={}",
                m.id,
                m.paused_ts.unwrap_or(0)
            );
            Ok(0)
        }
        MissionCmd::Resume { id, reasoning } => {
            let m = crew::lifecycle::mission_resume_with_reasoning(&id, reasoning.as_deref())?;
            println!(
                "mission `{}` → Active  (paused_ts preserved: {})",
                m.id,
                m.paused_ts.unwrap_or(0)
            );
            Ok(0)
        }
        MissionCmd::Propose {
            from_stdin,
            from_file,
            yes,
            start,
            ticket,
        } => mission_propose::propose(from_stdin, from_file.as_deref(), yes, start, ticket.as_deref()),
        MissionCmd::AddSprint {
            mission_id,
            sprint_id,
            description,
            depends_on,
            after,
            reasoning,
        } => {
            let s = crew::lifecycle::add_sprint_to_mission_with_reasoning(
                &mission_id,
                &sprint_id,
                &description,
                depends_on,
                after.as_deref(),
                reasoning.as_deref(),
            )?;
            let position = match after.as_deref() {
                Some(a) => format!(" (after `{a}`)"),
                None => String::new(),
            };
            println!(
                "mission `{}` ← added sprint `{}`{}",
                mission_id, s.id, position
            );
            Ok(0)
        }
        MissionCmd::Migrate { apply } => {
            let plan = migrate::plan_migration()?;
            migrate::print_plan(&plan);
            if !apply {
                if !plan.is_empty() {
                    println!("\nRe-run with --apply to commit.");
                }
                return Ok(0);
            }
            migrate::apply_migration(&plan)?;
            if !plan.is_empty() {
                println!(
                    "\nmigrate: applied {} move(s).",
                    plan.mission_moves.len() + plan.sprint_moves.len()
                );
            }
            Ok(0)
        }
        MissionCmd::Dispatch {
            mission_id,
            role,
            machine,
            timeout,
            no_wait,
        } => cmd_mission_dispatch(&mission_id, &role, machine.as_deref(), timeout, !no_wait),
        MissionCmd::Run {
            mission_id,
            sprint,
            role,
            image,
            base,
            timeout,
        } => mission_run::run(
            &mission_id,
            sprint.as_deref(),
            &role,
            image.as_deref(),
            &base,
            timeout,
        ),
        MissionCmd::Abort {
            mission_id,
            sprint,
        } => mission_run::abort(&mission_id, sprint.as_deref()),
        MissionCmd::Ship {
            mission_id,
            sprint,
            base,
            wait_ci,
            merge,
        } => mission_run::ship(&mission_id, sprint.as_deref(), &base, wait_ci, merge),
    }
}

fn cmd_mission_dispatch(
    mission_id: &str,
    role_id: &str,
    machine: Option<&str>,
    timeout_seconds: u32,
    wait: bool,
) -> Result<i32> {
    use crew::loader::{load_missions, load_roles, load_sprints};

    // 0. CLI-boundary charset validation (Wave-E.5 #255 — security-
    //    auditor MEDIUM from PR-D.1 review). `mission_id` flows into
    //    the session_id format string + WorkJob payload + audit chain
    //    + future "look up by mission" filters; charset enforcement
    //    at the boundary protects all current AND future use of the
    //    value. Rejects path-traversal, special chars, over-long ids.
    fleet::validate_identifier("mission_id", mission_id)?;
    fleet::validate_identifier("role_id", role_id)?;
    if let Some(m) = machine {
        fleet::validate_identifier("--machine", m)?;
    }

    // 1. Validate the mission exists.
    let missions = load_missions()?;
    let mission = missions
        .iter()
        .find(|m| m.id == mission_id)
        .ok_or_else(|| {
            anyhow::anyhow!(
            "mission `{mission_id}` not found. Run `darkmux mission propose` first or check the id."
        )
        })?;
    if !matches!(mission.status, crew::types::MissionStatus::Active) {
        eprintln!(
            "darkmux mission dispatch: warning — mission `{mission_id}` status is {:?}, not Active. \
             Proceeding anyway (operator-explicit override).",
            mission.status
        );
    }

    // 2. Confirm the role exists before fanning out sprints. After #590
    //    there is no tier requirement — sprints fan onto the single
    //    global `darkmux:work` stream and the first available runner
    //    claims each one.
    let roles = load_roles()?;
    if !roles.iter().any(|r| r.id == role_id) {
        anyhow::bail!("role `{role_id}` not found");
    }

    // 3. Filter sprints: this mission + depends_on=[] + status=Planned.
    //    `Running` is NOT included — PR-D.1 filter-level guard. Wave-E.3
    //    adds the state-machine gate: each filtered sprint goes through
    //    `lifecycle::sprint_start` BEFORE publish, flipping Planned →
    //    Running. A second `mission dispatch` invocation finds 0
    //    dispatchable sprints (all Running now) and bails with exit 2.
    let sprints = load_sprints()?;
    let initial: Vec<_> = sprints
        .iter()
        .filter(|s| s.mission_id == mission_id && s.depends_on.is_empty())
        .filter(|s| matches!(s.status, crew::types::SprintStatus::Planned))
        .collect();

    if initial.is_empty() {
        eprintln!(
            "darkmux mission dispatch: no sprints with depends_on=[] in mission `{mission_id}` \
             in Planned status. Nothing to fan out. (Running sprints from a previous \
             dispatch must be `darkmux sprint complete` or `sprint abandon` before \
             they're eligible again.)"
        );
        return Ok(2);
    }

    // 3b. Flip each filtered sprint Planned → Running BEFORE publishing.
    //     If a sprint flipped between the filter and this call (unlikely
    //     in single-operator scenarios but possible under racing CLIs),
    //     `sprint_start` bails on already-Running; skip and warn.
    let mut started: Vec<&crew::types::Sprint> = Vec::with_capacity(initial.len());
    for sprint in &initial {
        match crew::lifecycle::sprint_start(&sprint.id) {
            Ok(_) => started.push(*sprint),
            Err(e) => {
                eprintln!(
                    "darkmux mission dispatch: skipping sprint `{}` — sprint_start failed: {e:#}",
                    sprint.id
                );
            }
        }
    }
    if started.is_empty() {
        eprintln!(
            "darkmux mission dispatch: no sprints survived sprint_start (all were \
             already Running/Complete). Nothing to fan out."
        );
        return Ok(2);
    }

    // 4. Redis required for cross-machine fan-out. env(DARKMUX_REDIS_URL) >
    //    config-assembled (#661 Slice 5).
    let raw_url = flow::redis_url().ok_or_else(|| anyhow::anyhow!(
        "mission dispatch requires Redis (DARKMUX_REDIS_URL or config.redis.enabled) — the fleet work queue lives on Redis."
    ))?;
    let client = redis::Client::open(raw_url.expose_for_probe())
        .with_context(|| format!("opening Redis client {raw_url} for mission dispatch"))?;

    // 5. Build + pre-validate all WorkJobs BEFORE publishing any
    //    (HIGH-2 from review). All-or-nothing semantics: if any sprint
    //    would trip validate() (oversize description, etc.), the
    //    operator finds out before ANY orphan job lands on Redis.
    //    Loop-index suffix on session_id defeats microsecond collisions
    //    under sub-microsecond loop iterations (review M-session-id).
    let local_machine = flow::resolve_machine_id();
    let local_orchestrator = flow::resolve_orchestrator();
    let dispatch_micros = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_micros())
        .unwrap_or(0);
    let mut jobs: Vec<(String, String, fleet::WorkJob)> = Vec::new(); // (sprint_id, session_id, job)
    for (idx, sprint) in started.iter().enumerate() {
        let session_id = format!(
            "mission-{}-sprint-{}-{}-{}",
            mission_id, sprint.id, dispatch_micros, idx
        );
        let job = fleet::build_work_job(
            machine.map(String::from),
            role_id.to_string(),
            sprint.description.clone(),
            session_id.clone(),
            None,
            None,
            Some(sprint.id.clone()),
            // Mission dispatch publishes work jobs to peers; the runner
            // on the receiving machine runs the role through the
            // internal Docker-bounded runtime (same default as local
            // dispatch — Beat 36 directional principle, openclaw
            // optional everywhere).
            crate::crew::dispatch::Runtime::Internal,
            None, // image (#703 Slice 4) — mission dispatch uses the runner's default
            timeout_seconds,
            local_machine.clone(),
            local_orchestrator.clone(),
        );
        // Pre-validate. Surfaces oversize/charset failures BEFORE any
        // partial publish lands on the queue.
        job.validate()
            .with_context(|| format!("pre-publish validation failed for sprint `{}`", sprint.id))?;
        jobs.push((sprint.id.clone(), session_id, job));
    }

    // 6. Publish. Capture sessions for wait-aggregation. If a mid-loop
    //    publish fails (Redis network blip), the operator gets the
    //    list of already-published (sprint_id, session_id, work_id)
    //    triples on stderr so they can dedup / clean up via flow tail.
    eprintln!(
        "darkmux mission dispatch: mission={mission_id} role={role_id} \
         sprints={} target_machine={}",
        jobs.len(),
        machine.unwrap_or("<any>")
    );
    let mut sessions: Vec<(String, String, String)> = Vec::new(); // (sprint_id, session_id, work_id)
    for (sprint_id, session_id, job) in &jobs {
        match fleet::publish_job(&client, job) {
            Ok(work_id) => {
                eprintln!("  sprint={sprint_id} work_id={work_id} session={session_id}");
                sessions.push((sprint_id.clone(), session_id.clone(), work_id));
            }
            Err(e) => {
                eprintln!(
                    "\ndarkmux mission dispatch: ERROR — publish failed for sprint `{sprint_id}` \
                     after {} successful publishes. Already-published jobs are in flight on runners:",
                    sessions.len()
                );
                for (sid, sess, wid) in &sessions {
                    eprintln!("  ORPHAN sprint={sid} session={sess} work_id={wid}");
                }
                eprintln!(
                    "Tail each via `darkmux flow tail --session <id>` OR \
                     XRANGE darkmux:flow against the published session_ids to track completion. \
                     Do NOT re-run mission dispatch without checking — re-publish would \
                     double-fire the orphans."
                );
                return Err(e)
                    .with_context(|| format!("publishing sprint `{sprint_id}` as WorkJob"));
            }
        }
    }

    if !wait {
        println!(
            "Published {} sprint job(s); operator polls for completion via flow stream.",
            sessions.len()
        );
        for (sprint_id, session_id, work_id) in &sessions {
            println!("  sprint={sprint_id} session_id={session_id} work_id={work_id}");
        }
        return Ok(0);
    }

    // 6. Wait for each completion. Sequential polling is correct (XRANGE
    //    full-scan returns ALL records; finding session A doesn't preclude
    //    finding session B in the same pass). Net wall-clock is bounded by
    //    the slowest sprint's completion.
    let wait_timeout = std::time::Duration::from_secs((timeout_seconds as u64).saturating_add(60));
    eprintln!(
        "\n{}",
        worst_case_wait_banner(sessions.len(), timeout_seconds, wait_timeout.as_secs())
    );
    let mut completed: usize = 0;
    let mut failures: usize = 0;
    let mission_start = std::time::Instant::now();
    let mut sum_sprint_wall_ms: u64 = 0;
    for (sprint_id, session_id, _work_id) in &sessions {
        match fleet::wait_for_completion(&raw_url, session_id, wait_timeout) {
            Ok(c) => {
                completed += 1;
                if c.result_class != "ok" {
                    failures += 1;
                }
                if let Some(ms) = c.wall_ms {
                    sum_sprint_wall_ms += ms;
                }
                eprintln!(
                    "  ✓ sprint={sprint_id} result={} wall_ms={:?}",
                    c.result_class, c.wall_ms
                );
            }
            Err(e) => {
                failures += 1;
                eprintln!("  ✗ sprint={sprint_id} wait error: {e:#}");
            }
        }
    }
    let mission_wall_ms = mission_start.elapsed().as_millis() as u64;

    // Empirical parallelism check (#246 Q3 risk #3): if total wall-clock
    // is meaningfully less than sum of sprint wall-clocks, dispatches
    // ran in parallel. Otherwise they were serial under the hood.
    println!(
        "\nmission dispatch: completed={completed}/{} failures={failures} \
         wall_ms={mission_wall_ms} sum_sprint_wall_ms={sum_sprint_wall_ms}",
        sessions.len()
    );
    match speedup_verdict(sum_sprint_wall_ms, mission_wall_ms, sessions.len()) {
        SpeedupVerdict::ParallelConfirmed { speedup } => println!(
            "  → wall-clock indicates parallel execution: {speedup:.2}× speedup vs the \
             sum of per-sprint wall_ms (runner self-reported; not authenticated)."
        ),
        SpeedupVerdict::SeriallySuspect { speedup } => println!(
            "  ⚠ wall_ms ≈ sum of sprint wall_ms ({speedup:.2}×) — sprints may have run \
             serially. Check fleet roster + runner reachability."
        ),
        SpeedupVerdict::Inconclusive => {}
    }

    if failures > 0 {
        Ok(1)
    } else {
        Ok(0)
    }
}

/// Render the operator-facing "waiting for N completion(s)" banner with
/// the worst-case wall-clock bound named up front (Wave-E.9 #255). The
/// wait loop is sequential-per-sprint, so worst case is
/// `N × (per_sprint_timeout + slack)`. Surfacing this lets the operator
/// decide whether to SIGINT before the second per-sprint timeout if the
/// first sprint hangs — closes the PR-D.1 review MEDIUM where the
/// unbounded total wait could quietly run hours.
pub fn worst_case_wait_banner(
    n_sessions: usize,
    per_sprint_timeout_seconds: u32,
    wait_timeout_seconds: u64,
) -> String {
    let worst_case_secs = (n_sessions as u64).saturating_mul(wait_timeout_seconds);
    format!(
        "darkmux mission dispatch: waiting for {n_sessions} completion(s) \
         (per-sprint timeout {per_sprint_timeout_seconds}s + 60s slack; \
         worst-case total wall ≈ {worst_case_secs}s = {worst_case_min}min). \
         SIGINT cleanly aborts.",
        worst_case_min = worst_case_secs / 60,
    )
}

/// Minimum speedup ratio (sum_sprint_wall_ms / mission_wall_ms) at which
/// the mission-dispatch summary asserts "parallel execution." Below this,
/// the metric is reported with a serially-suspect warning OR nothing
/// (n=1 case). 1.5 is a conservative threshold for 2-machine fleets —
/// noise and per-sprint setup overhead can push a truly-parallel run
/// below 2.0× speedup. Adjust upward if false-positives appear.
const PARALLELISM_CONFIRMED_THRESHOLD: f64 = 1.5;

/// Verdict from the empirical parallelism metric computed at the end of
/// `mission dispatch --wait`. Extracted as a pure function (#255 Wave-E.4)
/// so the math + thresholding are unit-testable independent of the rest
/// of the dispatch handler.
#[derive(Debug, PartialEq)]
pub enum SpeedupVerdict {
    /// Wall-clock indicates parallel execution: speedup ≥
    /// `PARALLELISM_CONFIRMED_THRESHOLD` AND more than one sprint
    /// completed. Caller renders an operator-facing
    /// "parallel execution: Nx speedup" line.
    ParallelConfirmed { speedup: f64 },
    /// Wall-clock ≈ sum-of-sprints (`speedup < threshold`) with multiple
    /// sprints — sprints may have run serially under the hood. Caller
    /// renders the operator-warning line pointing at fleet roster +
    /// runner reachability.
    SeriallySuspect { speedup: f64 },
    /// Insufficient data to assert parallel vs serial: either zero
    /// sprints completed (`sum_sprint_wall_ms == 0`) OR exactly one
    /// sprint (parallelism is undefined for n=1). Caller stays silent.
    Inconclusive,
}

/// Pure-function speedup verdict computation. Inputs are the metric
/// summaries collected during `mission dispatch --wait`:
///
/// - `sum_sprint_wall_ms` — sum of `wall_ms` from each
///   `dispatch.complete` flow record. Runner self-reported.
/// - `mission_wall_ms` — wall time from `mission dispatch` invocation
///   to the last completion seen, measured by the publisher.
/// - `n_sprints` — number of sprints dispatched (sessions.len()).
pub fn speedup_verdict(
    sum_sprint_wall_ms: u64,
    mission_wall_ms: u64,
    n_sprints: usize,
) -> SpeedupVerdict {
    if sum_sprint_wall_ms == 0 || n_sprints == 0 {
        return SpeedupVerdict::Inconclusive;
    }
    // Avoid divide-by-zero on instantaneous missions; the `.max(1.0)`
    // floor doesn't materially change any non-degenerate case.
    let speedup = (sum_sprint_wall_ms as f64) / (mission_wall_ms as f64).max(1.0);
    if n_sprints > 1 && speedup >= PARALLELISM_CONFIRMED_THRESHOLD {
        SpeedupVerdict::ParallelConfirmed { speedup }
    } else if n_sprints > 1 {
        SpeedupVerdict::SeriallySuspect { speedup }
    } else {
        // n == 1: parallelism is undefined for a single sprint. Stay
        // silent even if the math says speedup >= threshold (which can
        // only happen via clock skew or wall_ms misreporting).
        SpeedupVerdict::Inconclusive
    }
}

/// Print a per-tier recommendation entry in operator-readable form.
/// Wraps `recommendations::for_tier` / `for_active_hardware` so the
/// `/darkmux-bootstrap` skill (and ad-hoc operator use) has a single
/// way to ask "what does darkmux recommend for my hardware?".
fn cmd_recommendations(sub: RecommendationsCmd) -> Result<i32> {
    match sub {
        RecommendationsCmd::Show { tier, json } => {
            let rec = match tier.as_deref() {
                Some(t) => recommendations::for_tier(t)?,
                None => recommendations::for_active_hardware()?,
            };
            if json {
                // (#907) Serialize the Recommendation directly — the full
                // entry (tier, status, primary/compactor, rationale, etc.).
                println!("{}", serde_json::to_string_pretty(&rec)?);
                return Ok(0);
            }
            println!("Tier:     {}", rec.tier);
            println!("Status:   {:?}", rec.status);
            if let Some(name) = &rec.profile_name {
                println!("Profile:  {name}");
            }
            if let Some(p) = &rec.primary {
                println!(
                    "Primary:  {} (n_ctx={}, role={})",
                    p.model_id, p.n_ctx, p.role
                );
            }
            if let Some(c) = &rec.compactor {
                println!(
                    "Compactor: {} (n_ctx={}, role={})",
                    c.model_id, c.n_ctx, c.role
                );
            }
            if !rec.validated_against.is_empty() {
                println!("Validated against: {}", rec.validated_against.join(", "));
            }
            if !rec.not_validated_against.is_empty() {
                println!(
                    "NOT validated against: {}",
                    rec.not_validated_against.join(", ")
                );
            }
            if let Some(url) = &rec.bake_off_url {
                println!("Bake-off:  {url}");
            }
            println!();
            println!("Rationale:");
            for line in rec.rationale.lines() {
                println!("  {line}");
            }
            Ok(0)
        }
    }
}

fn cmd_fleet(sub: FleetCmd) -> Result<i32> {
    match sub {
        FleetCmd::Add {
            id,
            address,
            description,
        } => cmd_fleet_add(&id, &address, description.as_deref()),
        FleetCmd::Remove { id } => cmd_fleet_remove(&id),
        FleetCmd::Status { json, deep } => cmd_fleet_status(json, deep),
    }
}

fn cmd_fleet_add(id: &str, address: &str, description: Option<&str>) -> Result<i32> {
    let was_present = fleet::mutate_roster(|roster| {
        let was_present = roster.machines.contains_key(id);
        fleet::add_machine(roster, id, address, description)?;
        Ok(was_present)
    })?;
    let verb = if was_present { "updated" } else { "added" };
    println!("fleet: {verb} {id} (address={address})");
    if let Some(d) = description {
        println!("  description: {d}");
    }
    println!("  roster: {}", fleet::roster_path().display());
    Ok(0)
}

fn cmd_fleet_remove(id: &str) -> Result<i32> {
    let removed = fleet::mutate_roster(|roster| Ok(fleet::remove_machine(roster, id)))?;
    match removed {
        Some(entry) => {
            println!("fleet: removed {id} (address was {})", entry.address);
            println!("  roster: {}", fleet::roster_path().display());
            Ok(0)
        }
        None => {
            eprintln!("fleet: no machine `{id}` in roster — nothing to remove");
            Ok(2)
        }
    }
}

fn cmd_fleet_status(emit_json: bool, deep: bool) -> Result<i32> {
    let roster = fleet::load_roster()?;

    // Probe each machine's reachability (TCP connect to its daemon port).
    // Done sequentially — the roster is small and the budget per probe
    // is 300ms; total wall is bounded.
    let probes: Vec<(fleet::MachineEntry, fleet::ReachabilityResult)> = roster
        .machines
        .values()
        .map(|m| {
            let probe = fleet::probe_reachability(&m.address);
            (m.clone(), probe)
        })
        .collect();

    // When --deep, fetch /machine/specs from each reachable peer. One
    // HTTP GET per peer; ~1s budget each. Failures are surfaced per-row
    // (Some(None) in the resolved vector) — they MUST NOT fail the
    // whole command. (#275 PR-B)
    // (#881) Resolve THIS machine's serve token once and send it to peers — a
    // single shared fleet token. Track peers that answered 401/403 so a missing
    // token surfaces a real "auth?" signal instead of looking like a timeout.
    let token = darkmux_flow::serve_token();
    let token_str = token.as_ref().map(|t| t.expose_for_compare());
    let mut auth_required: Vec<String> = Vec::new();
    let specs_by_id: std::collections::BTreeMap<String, Option<serde_json::Value>> = if deep {
        probes
            .iter()
            .map(|(m, p)| {
                let value = if p.reachable {
                    match fetch_machine_specs(&m.address, token_str) {
                        SpecsProbe::Ok(v) => Some(v),
                        SpecsProbe::AuthRequired => {
                            auth_required.push(m.id.clone());
                            None
                        }
                        SpecsProbe::Unavailable => None,
                    }
                } else {
                    None
                };
                (m.id.clone(), value)
            })
            .collect()
    } else {
        std::collections::BTreeMap::new()
    };

    if emit_json {
        // (#776) Machine-readable output stays byte-clean: force color off so
        // any accidental downstream style call can't leak ANSI into the JSON.
        darkmux_types::style::set_colorize_override(Some(false));
        let local_id = flow::resolve_machine_id();
        let payload = serde_json::json!({
            "roster_path": fleet::roster_path().display().to_string(),
            "roster_version": roster.version,
            "local_machine_id": local_id,
            "machines": probes
                .iter()
                .map(|(m, p)| serde_json::json!({
                    "id": m.id,
                    "address": m.address,
                    "description": m.description,
                    "added_unix_ms": m.added_unix_ms,
                    "reachable": p.reachable,
                    "resolved_address": p.resolved_address,
                    "probe_ms": p.elapsed_ms,
                    "probe_error": p.error,
                    // Only present when --deep was passed; null when
                    // --deep was passed but the fetch failed.
                    "specs": specs_by_id.get(&m.id).cloned().flatten().unwrap_or(serde_json::Value::Null),
                    // (#881) Distinguish a null `specs` caused by a 401/403
                    // (this machine isn't sending the shared fleet token) from a
                    // timeout/other failure, so a consumer (viewer/script) gets
                    // the same signal the text table's `auth?` column carries.
                    "specs_auth_required": auth_required.contains(&m.id),
                }))
                .collect::<Vec<_>>(),
        });
        println!("{}", serde_json::to_string_pretty(&payload)?);
        return Ok(0);
    }

    // Human-readable table.
    use darkmux_types::style;
    println!("{}", style::header("darkmux fleet status"));
    println!(
        "  roster:           {}",
        style::dim(&fleet::roster_path().display().to_string())
    );
    println!(
        "  local machine_id: {}",
        style::dim(&flow::resolve_machine_id().unwrap_or_else(|| "<unknown>".into()))
    );
    println!();
    if probes.is_empty() {
        println!("(no peers in roster — single-machine fleet)");
        println!();
        println!("Add a peer: darkmux fleet add <id> --address <tailnet-addr>");
        return Ok(0);
    }
    // Column-header row dimmed as secondary structure. Styling wraps the
    // WHOLE line (color codes at the line edges), so column alignment — which
    // counts visible chars inside the format — is preserved.
    if deep {
        println!(
            "{}",
            style::dim(&format!(
                "{:<14} {:<22} {:<10} {:<11} {:<10} VERSION  MODELS",
                "MACHINE", "ADDRESS", "PROBE", "RAM-FREE", "OS"
            ))
        );
    } else {
        println!(
            "{}",
            style::dim(&format!(
                "{:<14} {:<26} {:<10} DESCRIPTION",
                "MACHINE", "ADDRESS", "PROBE"
            ))
        );
    }
    for (m, p) in &probes {
        let status = if p.reachable {
            format!("✓ {}ms", p.elapsed_ms)
        } else {
            format!("✗ {}ms", p.elapsed_ms)
        };
        if deep {
            let specs = specs_by_id.get(&m.id).cloned().unwrap_or(None);
            let (ram_free, os_str, version, models_summary) = match &specs {
                Some(s) => {
                    let ram = s
                        .get("ram_free_for_ai_bytes")
                        .and_then(|v| v.as_u64())
                        .map(human_gb)
                        .unwrap_or_else(|| "—".into());
                    let os = s
                        .get("os")
                        .and_then(|v| v.as_str())
                        .unwrap_or("—")
                        .to_string();
                    let v = s
                        .get("darkmux_version")
                        .and_then(|v| v.as_str())
                        .unwrap_or("—")
                        .to_string();
                    let models = s
                        .get("loaded_models")
                        .and_then(|v| v.as_array())
                        .map(|arr| {
                            arr.iter()
                                .filter_map(|m| m.get("identifier").and_then(|i| i.as_str()))
                                .collect::<Vec<_>>()
                                .join(", ")
                        })
                        .unwrap_or_else(|| "—".into());
                    (
                        ram,
                        os,
                        v,
                        if models.is_empty() {
                            "—".into()
                        } else {
                            models
                        },
                    )
                }
                // (#881) Distinguish a 401/403 (peer requires a token we didn't
                // send) from a generic specs failure, so it doesn't read as a
                // timeout.
                None if auth_required.contains(&m.id) => {
                    ("auth?".into(), "—".into(), "—".into(), "—".into())
                }
                None => ("specs?".into(), "—".into(), "—".into(), "—".into()),
            };
            let row = format!(
                "{:<14} {:<22} {:<10} {:<11} {:<10} {:<8} {}",
                m.id, m.address, status, ram_free, os_str, version, models_summary
            );
            // Fade unreachable peers (whole-line dim — alignment-safe).
            println!("{}", if p.reachable { row } else { style::dim(&row) });
        } else {
            let desc = m.description.as_deref().unwrap_or("");
            let row = format!(
                "{:<14} {:<26} {:<10} {}",
                m.id, m.address, status, desc
            );
            println!("{}", if p.reachable { row } else { style::dim(&row) });
        }
        if let Some(err) = &p.error {
            println!("{}", style::error(&format!("               error: {err}")));
        }
    }
    // (#881) If any peer returned 401/403, the local machine is missing the
    // shared fleet token — surface the fix rather than leaving a silent "auth?".
    if !auth_required.is_empty() {
        println!(
            "{}",
            style::warn(&format!(
                "  ! {} peer(s) require a bearer token this machine isn't sending ({}). \
Set DARKMUX_SERVE_TOKEN (or the darkmux-serve-token Keychain item) to the shared fleet token.",
                auth_required.len(),
                auth_required.join(", ")
            ))
        );
    }
    Ok(0)
}

/// Outcome of probing a peer's `/machine/specs` (#881). `AuthRequired`
/// (401/403) is distinguished from `Unavailable` (timeout, refused, other
/// non-2xx, bad JSON) so a missing shared fleet token reads as `auth?`, not a
/// silent `specs?`.
enum SpecsProbe {
    Ok(serde_json::Value),
    AuthRequired,
    Unavailable,
}

/// Fetch `/machine/specs` from a peer's daemon at `address`, sending the shared
/// fleet bearer `token` if one is configured (#881). Bounded at 1s total — the
/// operator gets a row per peer even when one is slow or wedged. (#275 PR-B)
fn fetch_machine_specs(address: &str, token: Option<&str>) -> SpecsProbe {
    let normalized = if address.contains("://") {
        address.to_string()
    } else if address.contains(':') {
        format!("http://{address}")
    } else {
        // (#907) Use the typed port const — string-splitting the addr is
        // wrong for IPv6 / port-less forms.
        format!("http://{address}:{}", crate::serve::DEFAULT_DAEMON_PORT)
    };
    let url = format!("{normalized}/machine/specs");
    let agent = ureq::AgentBuilder::new()
        .timeout(std::time::Duration::from_millis(1000))
        .build();
    let mut req = agent.get(&url);
    if let Some(tok) = token {
        req = req.set("Authorization", &format!("Bearer {tok}"));
    }
    match req.call() {
        Ok(resp) => match resp.into_string() {
            Ok(body) => match serde_json::from_str(&body) {
                Ok(v) => SpecsProbe::Ok(v),
                Err(_) => SpecsProbe::Unavailable,
            },
            Err(_) => SpecsProbe::Unavailable,
        },
        Err(ureq::Error::Status(401, _)) | Err(ureq::Error::Status(403, _)) => {
            SpecsProbe::AuthRequired
        }
        Err(_) => SpecsProbe::Unavailable,
    }
}

/// Format a byte count as a human-friendly "N GB" string for the
/// `fleet status --deep` table. Round to whole GB — the precision the
/// `RAM-FREE` column wants. (#275 PR-B)
fn human_gb(bytes: u64) -> String {
    let gb = bytes as f64 / (1024.0 * 1024.0 * 1024.0);
    format!("{:.0} GB", gb.round())
}

fn cmd_crew(sub: CrewCmd) -> Result<i32> {
    match sub {
        CrewCmd::List => crew::cli::crew_list(),
        CrewCmd::Show { id } => crew::cli::crew_show(&id),
        CrewCmd::Dispatch {
            role,
            message,
            deliver,
            session_id,
            timeout,
            watch,
            workdir,
            sprint_id,
            skip_preflight,
            json,
            runtime,
            runtime_cmd,
            machine,
            no_wait,
            image,
        } => {
            // CLI default: if the operator didn't supply --watch, watch the
            // role's openclaw workspace dir. Library callers (e.g.
            // sprint_cli) pass an empty Vec directly to opt out.
            let watch_paths = if watch.is_empty() {
                vec![crew::dispatch::default_workspace_for_role(&role)]
            } else {
                watch
            };
            // Parse --runtime <openclaw|internal>; default internal.
            // `internal` routes to the in-house container-bounded
            // runtime (see `runtime/`); `openclaw` is the opt-in
            // shell-out path.
            let runtime_flag = crew::dispatch::Runtime::parse(&runtime)?;
            // Sprint-E QA: bail loud when --runtime-cmd is set without
            // --runtime openclaw. The flag is only consulted by the
            // openclaw shell-out; silently ignoring it under Internal
            // would re-introduce the "implicit state surprising the
            // operator" pattern Sprint-E removed.
            if runtime_flag != crew::dispatch::Runtime::Openclaw && runtime_cmd != "openclaw" {
                anyhow::bail!(
                    "--runtime-cmd `{runtime_cmd}` is only valid with --runtime openclaw \
                     (got --runtime {runtime}). Either drop --runtime-cmd, or add --runtime openclaw."
                );
            }
            let opts = crew::dispatch::DispatchOpts {
                role_id: role,
                message,
                deliver,
                session_id,
                timeout_seconds: timeout,
                skip_preflight,
                json,
                watch_paths,
                workdir,
                sprint_id,
                runtime: runtime_flag,
                runtime_cmd,
                machine,
                wait: !no_wait,
                // Bare `crew dispatch` carries no profile-derived compaction
                // config here; the internal dispatch fills the runtime-
                // required context window from the resolved `default_profile`
                // (#632 — the runtime has no built-in context-window default),
                // so a `default()` is safe. Lab + sprint paths derive the
                // full compaction config from the profile up front.
                compaction: crew::dispatch::CompactionDispatchArgs::default(),
                // (#549) Bare `crew dispatch` has no `--profile` override;
                // model selection falls back to the registry's
                // `default_profile`. The lab path passes the resolved name.
                profile_name: None,
                // (#984) No --profiles-file here; dispatch resolves env > default.
                config_path: None,
                // (#703) operator-selected dispatch image; darkmux injects
                // its runtime binary into it when it's not the default.
                image,
            };
            let result = fleet::dispatch_routed(opts)?;
            // Announce the resolved session id on stderr so operators see
            // which session openclaw was pointed at — without polluting
            // the --json envelope on stdout that orchestrators parse.
            eprintln!("darkmux crew dispatch: session id `{}`", result.session_id);
            // Stream both stdout (openclaw's --json envelope) and stderr to
            // the caller — the orchestrator parses one or the other.
            print!("{}", result.stdout);
            if !result.stderr.is_empty() {
                eprint!("{}", result.stderr);
            }
            // Surface the post-dispatch filesystem state at watched paths
            // (#89). Ground-truth signal next to the SIGNOFF block's
            // "files written" claims; operator/orchestrator compares.
            print_watched_state(&result.watched_state);
            Ok(result.exit_code)
        }
        CrewCmd::Index { sub } => match sub {
            CrewIndexCmd::Rebuild => crew::index::rebuild().map(|_| 0),
            CrewIndexCmd::Status => crew::index::status().map(|_| 0),
        },
        CrewCmd::Sync { yes, dry_run } => {
            // (#893) `crew sync` mutates operator-owned ~/.openclaw/openclaw.json,
            // so it must not write without explicit confirmation. Default
            // (no flags) = preview the diff + bail with a re-run hint;
            // `--dry-run` = explicit preview (exit 0); `--yes` = apply.
            // Follows `mission migrate`'s preview-then-confirm pattern, with a
            // non-zero exit when changes are pending so the unwritten state is
            // unmissable in scripts.
            let write = yes && !dry_run;
            let opts = crew::dispatch::SyncOpts { dry_run: !write };
            let result = crew::dispatch::sync(opts)?;
            let pending = result.added.len() + result.updated.len();
            let (add_v, upd_v) = if write {
                ("added", "updated")
            } else {
                ("would add", "would update")
            };
            let trail = if write {
                ""
            } else if dry_run {
                " [DRY RUN]"
            } else {
                " [PREVIEW]"
            };
            println!(
                "crew sync{trail}: {add_v} {a}, {upd_v} {u}, unchanged {un}, skipped (no .md prompt) {sn}",
                a = result.added.len(),
                u = result.updated.len(),
                un = result.unchanged.len(),
                sn = result.skipped_no_prompt.len(),
            );
            for id in &result.added {
                println!("  + {id}");
            }
            for id in &result.updated {
                println!("  ~ {id}");
            }
            for id in &result.skipped_no_prompt {
                println!("  · {id} (no .md prompt)");
            }
            // Default (no --yes, no --dry-run) with pending changes: refuse to
            // write, point at --yes. A bare `--dry-run` is an explicit preview
            // and exits 0 even with pending changes.
            if !write && !dry_run && pending > 0 {
                anyhow::bail!(
                    "crew sync: {pending} pending change(s) NOT applied to openclaw.json — \
                     re-run `darkmux crew sync --yes` to write them (or `--dry-run` to just preview)"
                );
            }
            Ok(0)
        }
    }
}

/// Render the post-dispatch watched-state summary to stderr. The output's
/// purpose is to give the operator/orchestrator a ground-truth view of
/// the filesystem at the watched paths so they can compare against any
/// "files written" claims in the SIGNOFF block (#89).
fn print_watched_state(states: &[crew::dispatch::WatchedPathState]) {
    if states.is_empty() {
        return;
    }
    eprintln!("darkmux crew dispatch: post-dispatch state at watched paths:");
    for s in states {
        if s.unreachable {
            eprintln!("  {} (unreachable — path does not exist)", s.root.display());
            continue;
        }
        if s.files.is_empty() {
            eprintln!("  {}: (no files)", s.root.display());
            continue;
        }
        eprintln!("  {}:", s.root.display());
        for f in &s.files {
            let rel = f.path.strip_prefix(&s.root).unwrap_or(&f.path);
            eprintln!("    {:>10}  {}", f.size, rel.display());
        }
        eprintln!(
            "    ({} file{})",
            s.files.len(),
            if s.files.len() == 1 { "" } else { "s" }
        );
    }
}

fn cmd_model(sub: ModelCmd) -> Result<i32> {
    match sub {
        ModelCmd::Status { json } => cmd_model_status(json),
        ModelCmd::Eject { dry_run } => cmd_model_eject(dry_run),
        ModelCmd::PullRecommended => cmd_model_pull_recommended(),
    }
}

fn cmd_model_status(json: bool) -> Result<i32> {
    let loaded = lms::list_loaded()?;
    let (managed, user): (Vec<_>, Vec<_>) = loaded
        .iter()
        .partition(|m| swap::is_darkmux_owned(&m.identifier));
    if json {
        // (#907) machine-readable parity, grouped by ownership.
        let out = serde_json::json!({
            "managed": managed,
            "user_state": user,
        });
        println!("{}", serde_json::to_string_pretty(&out)?);
        return Ok(0);
    }
    println!("{}", darkmux_types::style::header(&format!("darkmux-managed ({}):", managed.len())));
    if managed.is_empty() {
        println!("  (none — `darkmux swap <profile>` to load)");
    } else {
        for m in &managed {
            println!("  {} ctx={:<8} {}", darkmux_types::style::accent(&format!("{:<46}", m.identifier)), m.context, m.size);
        }
    }
    println!();
    println!("{}", darkmux_types::style::header(&format!("user state ({}):", user.len())));
    if user.is_empty() {
        println!("  (none — LMStudio is exclusively darkmux's right now)");
    } else {
        for m in &user {
            println!("  {} ctx={:<8} {}", darkmux_types::style::dim(&format!("{:<46}", m.identifier)), m.context, m.size);
        }
        println!();
        println!("note: darkmux will never unload entries under `user state` — they're");
        println!("      yours. Use `lms unload <identifier>` to remove them manually.");
    }
    Ok(0)
}

fn cmd_model_eject(dry_run: bool) -> Result<i32> {
    let loaded = lms::list_loaded()?;
    let managed: Vec<_> = loaded
        .iter()
        .filter(|m| swap::is_darkmux_owned(&m.identifier))
        .collect();
    let user_count = loaded.len() - managed.len();
    if managed.is_empty() {
        println!("no darkmux-managed loads to eject");
        if user_count > 0 {
            println!(
                "({} user-loaded model(s) untouched — use `lms unload <identifier>` for those)",
                user_count
            );
        }
        return Ok(0);
    }
    for m in &managed {
        if dry_run {
            println!("would eject {} (ctx={})", m.identifier, m.context);
        } else {
            println!("eject {} (ctx={})", m.identifier, m.context);
            lms::unload(&m.identifier)?;
        }
    }
    let verb = if dry_run { "would eject" } else { "ejected" };
    let mut summary = format!("{verb} {} model(s)", managed.len());
    if user_count > 0 {
        summary.push_str(&format!(", respected {user_count} user-loaded model(s)"));
    }
    if dry_run {
        summary.push_str(" [DRY RUN]");
    }
    println!("{summary}");
    Ok(0)
}

/// `darkmux model pull-recommended` — batch-download the bake-off-validated
/// models for the active hardware tier. Skips already-downloaded models;
/// reports per-model progress; errors with the tier's rationale when the
/// recommendation isn't validated. (#159)
fn cmd_model_pull_recommended() -> Result<i32> {
    let rec = recommendations::for_active_hardware()?;
    if rec.status != recommendations::RecommendationStatus::Validated {
        eprintln!(
            "darkmux: no validated recommendation for tier `{}` (status: {:?}).\n\nRationale:\n  {}",
            rec.tier, rec.status, rec.rationale
        );
        return Ok(2);
    }
    let required = rec.required_model_ids();
    if required.is_empty() {
        eprintln!(
            "darkmux: recommendation for tier `{}` is validated but lists no required models — registry bug.",
            rec.tier
        );
        return Ok(2);
    }

    let available = lms::list_available()?;
    let downloaded_keys: std::collections::HashSet<&str> =
        available.iter().map(|m| m.model_key.as_str()).collect();

    let mut downloaded_now = 0u32;
    let mut already_present = 0u32;
    for model_id in &required {
        if downloaded_keys.contains(model_id.as_str()) {
            println!("✓ {model_id} (already downloaded)");
            already_present += 1;
            continue;
        }
        println!("⤓ {model_id} (downloading via `lms get`)");
        lms::get(model_id)
            .with_context(|| format!("downloading recommended model `{model_id}`"))?;
        downloaded_now += 1;
    }

    println!(
        "darkmux: tier `{}` — {downloaded_now} downloaded, {already_present} already present, {} total required",
        rec.tier,
        required.len()
    );
    Ok(0)
}

/// `darkmux swap --recommended` — resolve the active hardware tier to its
/// bake-off-validated profile and swap to it. Errors loudly when the
/// recommendation status isn't `Validated`, or when the prescribed
/// models aren't downloaded (with a one-command-fix pointer to
/// `darkmux model pull-recommended`). (#159)
fn cmd_swap_recommended(
    config: Option<&str>,
    dry_run: bool,
    quiet: bool,
    runtime: &str,
) -> Result<i32> {
    let rec = recommendations::for_active_hardware()?;
    if !quiet {
        println!(
            "darkmux: matching tier `{}` → status `{:?}`",
            rec.tier, rec.status
        );
    }

    if rec.status != recommendations::RecommendationStatus::Validated {
        eprintln!(
            "darkmux: no validated recommendation for tier `{}`.\n\nRationale:\n  {}\n\nOptions:\n  - Pick a profile manually: `darkmux profiles` then `darkmux swap <name>`\n  - Contribute a bake-off for this tier — see kstrat2001/darkmux#117",
            rec.tier, rec.rationale
        );
        return Ok(2);
    }

    let profile_name = rec.profile_name.as_deref().ok_or_else(|| {
        anyhow::anyhow!(
            "validated recommendation for tier `{}` lacks `profile_name` — registry bug",
            rec.tier
        )
    })?;

    // Check the prescribed models are actually downloaded before kicking
    // off the swap. The swap itself would also fail if models are missing,
    // but a pre-flight check gives the operator a cleaner error + fix-it
    // pointer than discovering it mid-swap.
    let required = rec.required_model_ids();
    let available = lms::list_available()?;
    let downloaded_keys: std::collections::HashSet<&str> =
        available.iter().map(|m| m.model_key.as_str()).collect();
    let missing: Vec<&String> = required
        .iter()
        .filter(|id| !downloaded_keys.contains(id.as_str()))
        .collect();
    if !missing.is_empty() {
        eprintln!("darkmux: required model(s) not downloaded for recommended swap:");
        for id in &missing {
            eprintln!("  - {id}");
        }
        eprintln!("\nFix: `darkmux model pull-recommended`, then re-try.");
        return Ok(2);
    }

    if !quiet {
        println!(
            "darkmux: tier `{}` → profile `{profile_name}` (bake-off: {})",
            rec.tier,
            rec.bake_off_url.as_deref().unwrap_or("no url"),
        );
    }
    cmd_swap(Some(profile_name), config, dry_run, quiet, runtime, false)
}

fn cmd_profile(sub: ProfileCmd) -> Result<i32> {
    match sub {
        ProfileCmd::Draft {
            name,
            model,
            task_class,
            params,
            max_ctx,
        } => {
            let task = heuristics::TaskClass::parse(&task_class).ok_or_else(|| {
                anyhow::anyhow!("unknown task-class '{task_class}'. Try: fast | mid | long")
            })?;
            // Try to find the model in `lms ls`. If not found, the user MUST
            // supply --params (otherwise we'd silently bucket the unknown
            // model as Tiny, producing a 32K no-compactor profile regardless
            // of its real size — a documented footgun).
            let available = lms::list_available().unwrap_or_default();
            let meta = match available.iter().find(|m| m.model_key == model).cloned() {
                Some(found) => {
                    if params.is_some() || max_ctx.is_some() {
                        eprintln!(
                            "note: model `{model}` is in `lms ls`; --params/--max-ctx overrides ignored."
                        );
                    }
                    found
                }
                None => {
                    let Some(params) = params.as_deref() else {
                        anyhow::bail!(
                            "model `{model}` not found in `lms ls` (not downloaded yet?). \
                             Re-run with `--params <NB>` (e.g. `--params 70B`) to draft a \
                             profile from explicit metadata, or download the model first \
                             so heuristics can read its size + max context length."
                        );
                    };
                    eprintln!(
                        "note: model `{model}` not found in `lms ls`; using --params={params}. \
                         Heuristics are tighter when the model is downloaded — re-run after \
                         download for the canonical draft."
                    );
                    lms::ModelMeta {
                        model_key: model.clone(),
                        display_name: model.clone(),
                        publisher: "".into(),
                        size_bytes: 0,
                        params_string: Some(params.to_string()),
                        architecture: None,
                        max_context_length: max_ctx,
                        trained_for_tool_use: true,
                        model_type: "llm".into(),
                    }
                }
            };

            let suggestion = heuristics::suggest_profile(&meta, task);
            let json = heuristics::suggestion_to_profile_json(&name, &model, &suggestion, None);
            // Pretty-print
            println!("{}", serde_json::to_string_pretty(&json)?);
            eprintln!();
            eprintln!("// Copy the above into the `profiles` block of ~/.darkmux/profiles.json,");
            eprintln!("// then run `darkmux doctor` to verify the result.");
            Ok(0)
        }
    }
}

fn cmd_init(
    with_hook: bool,
    with_claude_md: Option<std::path::PathBuf>,
    with_agents_md: Option<std::path::PathBuf>,
    force: bool,
    dry_run: bool,
) -> Result<i32> {
    let report = init::init(&init::InitOptions {
        with_hook,
        with_claude_md,
        with_agents_md,
        force,
        dry_run,
    })?;
    if let Some(p) = report.profile_registry_path.as_ref() {
        if report.profile_registry_already_present {
            println!("profile registry: already present at {}", p.display());
        } else if report.profile_registry_created {
            println!(
                "profile registry: created at {} (edit it to point at your downloaded models — `lms ls`)",
                p.display()
            );
        }
    }
    if let Some(p) = report.config_path.as_ref() {
        if report.config_already_present {
            println!("config: already present at {}", p.display());
        } else if report.config_created {
            println!(
                "config: created at {} (machine_id seeded — edit to set Redis, dirs, runtime knobs)",
                p.display()
            );
        }
    }
    println!(
        "skills targets: {}",
        report
            .skills_targets
            .iter()
            .map(|p| p.display().to_string())
            .collect::<Vec<_>>()
            .join(", ")
    );
    if !report.skills_installed.is_empty() {
        println!(
            "  installed ({}): {}",
            report.skills_installed.len(),
            report.skills_installed.join(", ")
        );
    }
    if !report.skills_overwritten.is_empty() {
        println!(
            "  overwritten ({}): {}",
            report.skills_overwritten.len(),
            report.skills_overwritten.join(", ")
        );
    }
    if !report.skills_skipped.is_empty() {
        println!(
            "  skipped ({}): {}",
            report.skills_skipped.len(),
            report.skills_skipped.join(", ")
        );
    }
    if let Some(p) = report.hook_added {
        if report.hook_already_present {
            println!("hook: already present in {}", p.display());
        } else {
            println!("hook: added to {}", p.display());
        }
    }
    if let Some(p) = report.claude_md_path {
        if report.claude_md_already_present {
            println!("CLAUDE.md: already integrated at {}", p.display());
        } else if report.claude_md_appended {
            println!("CLAUDE.md: integration section appended to {}", p.display());
        }
    }
    if let Some(p) = report.agents_md_path {
        if report.agents_md_already_present {
            println!("AGENTS.md: already integrated at {}", p.display());
        } else if report.agents_md_appended {
            println!("AGENTS.md: integration section appended to {}", p.display());
        }
    }
    if dry_run {
        println!("[DRY RUN — nothing was written]");
    } else {
        println!();
        println!("Next steps:");
        if report.profile_registry_created {
            println!("  1. Edit ~/.darkmux/profiles.json to point at your downloaded models");
            println!("     (run `lms ls` to see what's available)");
            println!(
                "  2. Build the internal-runtime image (one-time, ~50 MB, from the darkmux repo root):"
            );
            println!("       docker build -t darkmux-runtime:latest runtime/");
            println!("  3. Run `darkmux doctor` to verify your setup");
            println!("  4. Run `darkmux lab characterize` to smoke-test your machine");
        } else {
            println!(
                "  1. Build the internal-runtime image if you haven't (one-time, ~50 MB, from the darkmux repo root):"
            );
            println!("       docker build -t darkmux-runtime:latest runtime/");
            println!("  2. Run `darkmux doctor` to verify your setup");
            println!("  3. Run `darkmux lab characterize` to smoke-test your machine");
        }
    }
    Ok(0)
}

fn cmd_swap(
    profile_name: Option<&str>,
    config: Option<&str>,
    dry_run: bool,
    quiet: bool,
    runtime: &str,
    recommended: bool,
) -> Result<i32> {
    // `swap --recommended` — short-circuit to the recommendation-
    // registry-driven dispatcher rather than looking up a profile.
    if recommended {
        return cmd_swap_recommended(config, dry_run, quiet, runtime);
    }
    let Some(profile_name) = profile_name else {
        eprintln!("darkmux: specify a profile name to swap to, or pass --recommended");
        return Ok(2);
    };
    let loaded = profiles::load_registry(config)?;
    let profile = profiles::get_profile(&loaded.registry, profile_name)?;
    if !quiet {
        println!(
            "darkmux: swapping to \"{profile_name}\" (registry: {})",
            loaded.path.display()
        );
    }
    // (#590) Openclaw config is patched only when the operator explicitly
    // names openclaw (`--runtime openclaw`). The default (`internal`) leaves
    // ~/.openclaw/openclaw.json untouched — file-presence is not opt-in.
    let patch_openclaw =
        crew::dispatch::Runtime::parse(runtime)? == crew::dispatch::Runtime::Openclaw;
    let result = swap::swap(
        profile,
        &loaded.registry,
        swap::SwapOpts {
            quiet,
            dry_run,
            patch_openclaw,
        },
    )?;
    if !quiet {
        let mut bits = vec![
            format!("done in {}ms", result.walltime_ms),
            format!("unloaded {}", result.unloaded.len()),
            format!("loaded {}", result.loaded.len()),
        ];
        if result.runtime_modified {
            bits.push("runtime patched".to_string());
        }
        if result.hooks_ran > 0 {
            bits.push(format!("{} hook(s)", result.hooks_ran));
        }
        if dry_run {
            bits.push("[DRY RUN]".to_string());
        }
        println!("{}", bits.join(" — "));
    }
    Ok(0)
}

fn cmd_status(config: Option<&str>, json: bool) -> Result<i32> {
    let loaded = profiles::load_registry(config)?;
    let models = lms::list_loaded()?;
    let matches: Vec<&String> = loaded
        .registry
        .profiles
        .iter()
        .filter(|(_, p)| profile_matches(p, &models))
        .map(|(k, _)| k)
        .collect();
    if json {
        // (#907) machine-readable parity for the frontier orchestrator.
        let out = serde_json::json!({
            "registry": loaded.path.display().to_string(),
            "loaded_models": models,
            "matching_profiles": matches,
        });
        println!("{}", serde_json::to_string_pretty(&out)?);
        return Ok(0);
    }
    println!("registry: {}", loaded.path.display());
    println!("loaded models ({}):", models.len());
    for m in &models {
        println!("  {:<40} ctx={:<8} {}", m.identifier, m.context, m.status);
    }
    if matches.is_empty() {
        println!("matches no registered profile");
    } else {
        let listed: Vec<&str> = matches.iter().map(|s| s.as_str()).collect();
        println!("matches profile(s): {}", listed.join(", "));
    }
    Ok(0)
}

fn profile_matches(profile: &types::Profile, loaded: &[types::LoadedModel]) -> bool {
    if profile.models.len() != loaded.len() {
        return false;
    }
    for m in &profile.models {
        let ident = m.identifier.clone().unwrap_or_else(|| m.id.clone());
        let Some(cur) = loaded.iter().find(|x| x.identifier == ident) else {
            return false;
        };
        if cur.context as u32 != m.n_ctx {
            return false;
        }
    }
    true
}

fn cmd_profiles(config: Option<&str>, json: bool) -> Result<i32> {
    let loaded = profiles::load_registry(config)?;
    if json {
        // (#907) Serialize the registry directly — `default_profile` + the
        // full profile map, the lowest-surprise machine-readable shape.
        let out = serde_json::json!({
            "registry_path": loaded.path.display().to_string(),
            "registry": loaded.registry,
        });
        println!("{}", serde_json::to_string_pretty(&out)?);
        return Ok(0);
    }
    println!("{}", darkmux_types::style::header(&format!("registry: {}", loaded.path.display())));
    for (name, profile) in &loaded.registry.profiles {
        let default_marker = if loaded.registry.default_profile.as_deref() == Some(name) {
            &format!(" {}", darkmux_types::style::success("(default)"))
        } else {
            ""
        };
        println!("\n{}{}", darkmux_types::style::accent(name), default_marker);
        if let Some(desc) = profile.description.as_deref() {
            println!("  {}", darkmux_types::style::dim(desc));
        }
        // (#590) Models no longer carry a role; mark the default model
        // (default_model, or first model) instead.
        let default_id = profile.default_model_id();
        for m in &profile.models {
            let marker = if Some(m.id.as_str()) == default_id {
                "default"
            } else {
                ""
            };
            println!("  - {} {} @ ctx {}", darkmux_types::style::dim(&format!("{:<10}", marker)), m.id, m.n_ctx);
        }
    }
    Ok(0)
}

fn cmd_skills(sub: SkillsCmd) -> Result<i32> {
    match sub {
        SkillsCmd::Install {
            target,
            force,
            dry_run,
        } => {
            let report = skills::install_skills(&skills::InstallOptions {
                target,
                force,
                dry_run,
            })?;
            println!("source: {}", report.source.display());
            println!(
                "targets: {}",
                report
                    .targets
                    .iter()
                    .map(|p| p.display().to_string())
                    .collect::<Vec<_>>()
                    .join(", ")
            );
            if !report.installed.is_empty() {
                println!(
                    "installed ({}): {}",
                    report.installed.len(),
                    report.installed.join(", ")
                );
            }
            if !report.overwritten.is_empty() {
                println!(
                    "overwritten ({}): {}",
                    report.overwritten.len(),
                    report.overwritten.join(", ")
                );
            }
            if !report.skipped.is_empty() {
                println!(
                    "skipped (already exists, use --force to overwrite) ({}): {}",
                    report.skipped.len(),
                    report.skipped.join(", ")
                );
            }
            if dry_run {
                println!("[DRY RUN — nothing was written]");
            }
            Ok(0)
        }
        SkillsCmd::List { target } => {
            let listed = skills::list_installed_skills(target.as_deref())?;
            if listed.is_empty() {
                println!("(no skills installed)");
            } else {
                for n in listed {
                    println!("{n}");
                }
            }
            Ok(0)
        }
    }
}

fn cmd_lab(sub: LabCmd) -> Result<i32> {
    match sub {
        LabCmd::Workloads => {
            let ids = lab::run::lab_workloads();
            if ids.is_empty() {
                println!("(no workloads found — check templates/builtin/workloads/ or .darkmux/workloads/)");
            } else {
                for id in ids {
                    println!("{id}");
                }
            }
            Ok(0)
        }
        LabCmd::Run {
            workload,
            profile,
            runs,
            profiles,
            quiet,
            runtime,
            runtime_cmd,
        } => {
            let runtime_flag = crew::dispatch::Runtime::parse(&runtime)?;
            // Sprint-E QA: bail loud when --runtime-cmd is set without
            // --runtime openclaw — see CrewCmd::Dispatch for the same
            // gate; doctrine is "no implicit state, operator-explicit
            // intent only" (Beat 36).
            if runtime_flag != crew::dispatch::Runtime::Openclaw && runtime_cmd != "openclaw" {
                anyhow::bail!(
                    "--runtime-cmd `{runtime_cmd}` is only valid with --runtime openclaw \
                     (got --runtime {runtime}). Either drop --runtime-cmd, or add --runtime openclaw."
                );
            }
            let outcomes = lab::run::lab_run(lab::run::RunOpts {
                workload_id: workload,
                profile_name: profile,
                runs,
                config_path: profiles,
                quiet,
                runtime: runtime_flag,
                runtime_cmd,
                loop_override: None,
                inject_context: None,
            })?;
            if !quiet {
                println!("\n{} run(s) complete:", outcomes.len());
                for o in &outcomes {
                    println!("  {} — {}", o.run_id, o.notes.join(" | "));
                }
            }
            Ok(if outcomes.iter().all(|o| o.ok) { 0 } else { 1 })
        }
        LabCmd::Loop {
            workload,
            profile,
            profiles,
            max_turns,
            max_tokens,
            timeout,
            compact_threshold_tokens,
            compact_threshold_ratio,
            compact_strategy,
            bail_after_compactions,
            context_window,
            ab,
            inject_from_mission,
            json,
        } => cmd_lab_loop(LabLoopArgs {
            workload,
            profile,
            profiles,
            max_turns,
            max_tokens,
            timeout,
            compact_threshold_tokens,
            compact_threshold_ratio,
            compact_strategy,
            bail_after_compactions,
            context_window,
            ab,
            inject_from_mission,
            json,
        }),
        LabCmd::Runs { limit, all } => {
            let lim = if all { None } else { Some(limit) };
            let summaries = lab::list::list_runs(lim)?;
            print!("{}", lab::list::format_table(&summaries));
            Ok(0)
        }
        LabCmd::Inspect { run, summary } => {
            let report = lab::inspect::lab_inspect(&run)?;
            println!("run:         {}", report.run_id);
            println!("workload:    {}", report.workload_id);
            println!("wall:        {}s", report.walltime_ms / 1000);
            println!("turns:       {}", report.turns);
            println!("compactions: {}", report.compactions);
            if !report.tokens_before.is_empty() {
                let listed: Vec<String> =
                    report.tokens_before.iter().map(|n| n.to_string()).collect();
                println!("tokensBefore: {}", listed.join(", "));
            }
            if let Some(m) = report.mode {
                println!(
                    "mode:        {}",
                    match m {
                        workloads::types::RunMode::Fast => "fast",
                        workloads::types::RunMode::Slow => "slow",
                    }
                );
            }
            println!("notes:");
            for n in &report.notes {
                println!("  - {n}");
            }
            if summary {
                let run_dir = lab::inspect::resolve_run_path(&run);
                let summaries = lab::inspect::read_compaction_summaries(&run_dir)?;
                println!();
                if summaries.is_empty() {
                    println!("compaction summaries: (none — no trajectory.jsonl recorded)");
                } else {
                    println!("compaction summaries: {}", summaries.len());
                    for (i, s) in summaries.iter().enumerate() {
                        println!();
                        println!(
                            "─── summary {} of {} (turn {}, tokensBefore={}, {} chars) ───",
                            i + 1,
                            summaries.len(),
                            s.turn_index,
                            s.tokens_before,
                            s.summary_chars
                        );
                        println!("{}", s.summary_text);
                    }
                }
            }
            Ok(0)
        }
        LabCmd::Compare { run_a, run_b } => {
            let result = lab::compare::lab_compare(&run_a, &run_b)?;
            for n in &result.notes {
                println!("{n}");
            }
            Ok(0)
        }
        LabCmd::Characterize {
            workload,
            profile,
            profiles,
        } => {
            let report = lab::characterize::characterize(&lab::characterize::CharacterizeOpts {
                workload,
                profile,
                config: profiles,
            })?;
            lab::characterize::print_report(&report);
            Ok(if report.outcomes.iter().all(|o| o.ok) {
                0
            } else {
                1
            })
        }
        LabCmd::Tune {
            workload,
            profile,
            runs,
            profiles,
        } => {
            let report = lab::tune::tune(&lab::tune::TuneOpts {
                workload,
                profile,
                runs,
                config: profiles,
            })?;
            lab::tune::print_report(&report);
            Ok(if report.outcomes.iter().all(|o| o.ok) {
                0
            } else {
                1
            })
        }
        LabCmd::Register {
            path,
            name,
            force,
            if_absent,
        } => {
            let msg = lab::fixture_cli::cmd_register(&path, name, force, if_absent)?;
            println!("{msg}");
            Ok(0)
        }
        LabCmd::Unregister { name } => {
            let msg = lab::fixture_cli::cmd_unregister(&name)?;
            println!("{msg}");
            Ok(0)
        }
        LabCmd::Fixtures => {
            let msg = lab::fixture_cli::cmd_list()?;
            println!("{msg}");
            Ok(0)
        }
        LabCmd::Doctor => {
            let report = lab::doctor::lab_doctor()?;
            // Warnings first so actionable items don't get buried
            // behind a long list of passes when many fixtures are
            // registered. Reviewer suggestion (#498 QA).
            for w in &report.warnings {
                println!("[warn] {w}");
            }
            for p in &report.passes {
                println!("[ok]  {p}");
            }
            println!();
            println!(
                "{} pass, {} warn ({} fixture{} checked)",
                report.passes.len(),
                report.warnings.len(),
                report.fixture_count,
                if report.fixture_count == 1 { "" } else { "s" }
            );
            Ok(if report.has_warnings() { 1 } else { 0 })
        }
    }
}

/// Flattened args for `darkmux lab loop` (#986). Kept as a struct so the
/// handler signature stays one parameter rather than a dozen.
struct LabLoopArgs {
    workload: String,
    profile: Option<String>,
    profiles: Option<String>,
    max_turns: Option<u32>,
    max_tokens: Option<u32>,
    timeout: Option<u64>,
    compact_threshold_tokens: Option<u32>,
    compact_threshold_ratio: Option<f32>,
    compact_strategy: Option<String>,
    bail_after_compactions: Option<u32>,
    context_window: Option<u32>,
    ab: bool,
    inject_from_mission: Option<String>,
    json: bool,
}

/// Parse the `--compact-strategy` flag into the typed enum. Accepts the two
/// strategies the runtime supports, kebab- or snake-cased.
fn parse_compact_strategy(raw: &str) -> Result<darkmux_types::CompactionStrategy> {
    match raw.trim().to_lowercase().replace('_', "-").as_str() {
        "narrative" => Ok(darkmux_types::CompactionStrategy::Narrative),
        "structured-slot" => Ok(darkmux_types::CompactionStrategy::StructuredSlot),
        other => anyhow::bail!(
            "unknown --compact-strategy `{other}` (expected `narrative` or `structured-slot`)"
        ),
    }
}

/// `darkmux lab loop` (#986) — single-run loop-engineering bench. Runs ONE
/// dispatch under the chosen harness config, then classifies how the loop
/// behaved via `lab::loop_report`.
///
/// Two loop-variation axes:
///   - **Caps** (`--max-turns` / `--max-tokens` / `--timeout`) resolve through
///     `config_access`'s live env tier, so we set the documented env override
///     for this dispatch. This process is single-shot (it exits right after),
///     so a process-wide `set_var` is the simplest honest mechanism — it's the
///     same tier an operator would `export` for one run.
///   - **Compaction** (`--compact-*` / `--bail-after-compactions` /
///     `--context-window`) overlays onto the profile-derived
///     `CompactionDispatchArgs` via the provider (`loop_override`).
fn cmd_lab_loop(args: LabLoopArgs) -> Result<i32> {
    use darkmux_lab::lab::loop_report::{analyze_run, LoopCompactionOverride};

    // ── build the compaction overlay (axis 2) ───────────────────────
    let strategy = match args.compact_strategy.as_deref() {
        Some(s) => Some(parse_compact_strategy(s)?),
        None => None,
    };
    // Validate the adaptive-trigger ratio upfront (parity with
    // --compact-strategy) — this is a trust-the-bench surface, so reject an
    // out-of-range value loudly rather than letting it flow into the runtime.
    if let Some(r) = args.compact_threshold_ratio {
        if !(0.1..=0.9).contains(&r) {
            anyhow::bail!(
                "--compact-threshold-ratio {r} is out of range (expected 0.1–0.9)"
            );
        }
    }
    let loop_override = LoopCompactionOverride {
        threshold_tokens: args.compact_threshold_tokens,
        threshold_ratio: args.compact_threshold_ratio,
        context_window: args.context_window,
        strategy,
        bail_after_compactions: args.bail_after_compactions,
    };

    // ── self-describing loop-config summary for the report ───────────
    let mut loop_config: Vec<String> = Vec::new();
    if let Some(p) = args.profile.as_deref() {
        loop_config.push(format!("profile={p}"));
    }
    if let Some(p) = args.profiles.as_deref() {
        loop_config.push(format!("profiles-file={p}"));
    }
    if let Some(n) = args.max_turns {
        loop_config.push(format!("max-turns={n}"));
    }
    if let Some(n) = args.max_tokens {
        loop_config.push(format!("max-tokens={n}"));
    }
    if let Some(n) = args.timeout {
        loop_config.push(format!("timeout={n}s"));
    }
    if let Some(n) = args.compact_threshold_tokens {
        loop_config.push(format!("compact-threshold-tokens={n}"));
    }
    if let Some(r) = args.compact_threshold_ratio {
        loop_config.push(format!("compact-threshold-ratio={r}"));
    }
    if let Some(s) = args.compact_strategy.as_deref() {
        loop_config.push(format!("compact-strategy={s}"));
    }
    if let Some(n) = args.bail_after_compactions {
        loop_config.push(format!("bail-after-compactions={n}"));
    }
    if let Some(n) = args.context_window {
        loop_config.push(format!("context-window={n}"));
    }
    if loop_config.is_empty() {
        loop_config.push("profile defaults (no overrides)".to_string());
    }

    // ── apply caps via the live env-override tier (axis 1) ───────────
    if let Some(n) = args.max_turns {
        std::env::set_var("DARKMUX_RUNTIME_MAX_TURNS", n.to_string());
    }
    if let Some(n) = args.max_tokens {
        std::env::set_var("DARKMUX_RUNTIME_MAX_TOKENS", n.to_string());
    }
    if let Some(n) = args.timeout {
        std::env::set_var("DARKMUX_INACTIVITY_TIMEOUT_SECONDS", n.to_string());
    }

    // ── run one arm of the bench (single dispatch + classify) ────────
    // Cloned inputs so the A/B path (#1004) can call it twice with only the
    // injected context varying. `inject` carries the engagement-context for the
    // "with" arm; `None` is the baseline. quiet in --json mode so stdout stays
    // pure JSON.
    use darkmux_lab::lab::loop_report::Verdict;
    let run_arm = |inject: Option<String>| -> Result<darkmux_lab::lab::loop_report::LoopReport> {
        let outcomes = darkmux_lab::lab::run::lab_run(darkmux_lab::lab::run::RunOpts {
            workload_id: args.workload.clone(),
            profile_name: args.profile.clone(),
            runs: 1,
            config_path: args.profiles.clone(),
            quiet: args.json,
            runtime: crew::dispatch::Runtime::Internal,
            runtime_cmd: "openclaw".to_string(),
            // When no compaction flag was set, pass `None` so the dispatch takes
            // the exact `lab run` compaction path (caps still apply via env).
            loop_override: if loop_override.is_empty() {
                None
            } else {
                Some(loop_override.clone())
            },
            inject_context: inject,
        })?;
        let outcome = outcomes
            .into_iter()
            .next()
            .ok_or_else(|| anyhow::anyhow!("loop lab produced no run outcome"))?;
        analyze_run(
            &outcome.run_dir,
            &outcome.run_id,
            outcome.ok,
            outcome.verify_passed,
            outcome.duration_ms,
            loop_config.clone(),
        )
    };

    // Exit 0 when the loop achieved the task (productive or struggled-through);
    // non-zero when it failed or — critically — falsely passed while inert.
    let verdict_exit = |v: Verdict| match v {
        Verdict::Productive | Verdict::Struggled => 0,
        Verdict::InertFalsePass | Verdict::Failed => 1,
    };

    // ── (#1004) engagement-context A/B ───────────────────────────────
    if args.ab {
        let ws = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
        let ctx = crate::mission_run::injected_context_for_lab(
            args.inject_from_mission.as_deref(),
            &ws,
            args.profile.as_deref(),
            args.profiles.as_deref(),
        );
        if ctx.trim().is_empty() {
            anyhow::bail!(
                "--ab: nothing to inject — no authored lessons for this repo{}. \
                 Record a lesson (`darkmux lessons add`){}, then retry.",
                args.inject_from_mission
                    .as_deref()
                    .map(|m| format!(" and no detected cautions for mission `{m}`"))
                    .unwrap_or_default(),
                if args.inject_from_mission.is_some() {
                    ""
                } else {
                    " or pass --inject-from-mission <id> to add a mission's cautions"
                }
            );
        }
        let ctx_chars = ctx.len();
        if !args.json {
            eprintln!("… A/B: baseline run (WITHOUT engagement-context)");
        }
        let without = run_arm(None)?;
        if !args.json {
            eprintln!("… A/B: treatment run (WITH {ctx_chars} chars of engagement-context)");
        }
        let with = run_arm(Some(ctx))?;

        // Run ids are second-stamped; the two arms are sequential with a
        // multi-second dispatch between, so they normally differ. Guard the
        // (improbable) same-second collision — a shared run dir would merge
        // trajectories and confound the very comparison this feature makes —
        // so a confounded A/B is surfaced, never silently trusted (#44).
        if with.run_id == without.run_id {
            anyhow::bail!(
                "A/B run-id collision (`{}`): both arms wrote the same run dir, so the \
                 comparison would mix trajectories. Re-run `--ab` (the arms are sequential; \
                 a second-boundary collision won't recur).",
                with.run_id
            );
        }

        // Rank the verdicts so the shift is a single signed comparison.
        let rank = |v: Verdict| match v {
            Verdict::Productive => 3,
            Verdict::Struggled => 2,
            Verdict::InertFalsePass => 1,
            Verdict::Failed => 0,
        };
        let shift = match rank(with.verdict).cmp(&rank(without.verdict)) {
            std::cmp::Ordering::Greater => "improved",
            std::cmp::Ordering::Less => "regressed",
            std::cmp::Ordering::Equal => "no-change",
        };

        if args.json {
            println!(
                "{}",
                serde_json::to_string_pretty(&serde_json::json!({
                    "ab": true,
                    "injected_context_chars": ctx_chars,
                    "verdict_shift": shift,
                    "without": without,
                    "with": with,
                }))?
            );
        } else {
            println!("\n── engagement-context A/B (#1004) ──");
            println!("  without context: {}", without.verdict.as_str());
            println!("  with context:    {} ({ctx_chars} chars injected)", with.verdict.as_str());
            println!("  verdict shift:   {shift}");
        }
        // The "with" arm is the configuration the operator would ship.
        return Ok(verdict_exit(with.verdict));
    }

    // ── single run (default) ─────────────────────────────────────────
    let report = run_arm(None)?;
    if args.json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        darkmux_lab::lab::loop_report::print_report(&report);
    }
    Ok(verdict_exit(report.verdict))
}

#[cfg(test)]
mod tests {
    use super::*;

    // ─── worst_case_wait_banner (Wave-E.9 #255) ──────────────────────

    #[test]
    fn worst_case_wait_banner_names_total_bound() {
        let s = worst_case_wait_banner(3, 600, 660);
        assert!(s.contains("3 completion(s)"));
        assert!(s.contains("worst-case total wall ≈ 1980s"));
        assert!(s.contains("33min")); // 1980 / 60
        assert!(s.contains("SIGINT"));
    }

    #[test]
    fn worst_case_wait_banner_handles_single_sprint() {
        let s = worst_case_wait_banner(1, 60, 120);
        assert!(s.contains("1 completion(s)"));
        assert!(s.contains("worst-case total wall ≈ 120s"));
        assert!(s.contains("2min"));
    }

    #[test]
    fn worst_case_wait_banner_handles_zero_sessions_gracefully() {
        // Defensive: should NOT panic on degenerate inputs even if the
        // caller's flow normally guards against this.
        let s = worst_case_wait_banner(0, 600, 660);
        assert!(s.contains("0 completion(s)"));
        assert!(s.contains("worst-case total wall ≈ 0s"));
    }

    #[test]
    fn worst_case_wait_banner_uses_saturating_arithmetic() {
        // u64 overflow check: large N × large wait_timeout shouldn't panic.
        let s = worst_case_wait_banner(u64::MAX as usize, 3600, 3660);
        assert!(s.contains("worst-case"));
    }

    // ─── speedup_verdict (Wave-E.4 #255) ──────────────────────────────

    #[test]
    fn speedup_verdict_confirms_parallel_when_speedup_above_threshold() {
        // 2 sprints, each 5000ms, mission wall 5000ms → speedup = 2.0
        let v = speedup_verdict(10_000, 5_000, 2);
        match v {
            SpeedupVerdict::ParallelConfirmed { speedup } => {
                assert!((speedup - 2.0).abs() < 0.01, "speedup ≈ 2.0; got {speedup}");
            }
            other => panic!("expected ParallelConfirmed; got {other:?}"),
        }
    }

    #[test]
    fn speedup_verdict_warns_serial_when_speedup_below_threshold() {
        // 2 sprints, sum 10000ms, mission wall 8500ms → speedup ≈ 1.18 (< 1.5)
        let v = speedup_verdict(10_000, 8_500, 2);
        match v {
            SpeedupVerdict::SeriallySuspect { speedup } => {
                assert!(
                    (speedup - 1.18).abs() < 0.05,
                    "speedup ≈ 1.18; got {speedup}"
                );
            }
            other => panic!("expected SeriallySuspect; got {other:?}"),
        }
    }

    #[test]
    fn speedup_verdict_inconclusive_when_no_sprints_completed() {
        assert_eq!(
            speedup_verdict(0, 100, 2),
            SpeedupVerdict::Inconclusive,
            "zero sum (e.g. all dispatch errors) → Inconclusive"
        );
    }

    #[test]
    fn speedup_verdict_inconclusive_when_single_sprint() {
        // Parallelism is undefined for a single sprint — stay silent
        // even if the math would otherwise say "confirmed".
        let v = speedup_verdict(10_000, 5_000, 1);
        assert_eq!(v, SpeedupVerdict::Inconclusive);
    }

    #[test]
    fn speedup_verdict_inconclusive_when_zero_sessions() {
        let v = speedup_verdict(10_000, 5_000, 0);
        assert_eq!(v, SpeedupVerdict::Inconclusive);
    }

    #[test]
    fn speedup_verdict_handles_zero_wall_ms_safely() {
        // Instantaneous mission (clock granularity). Math floor at 1ms
        // prevents divide-by-zero; verdict is still computed.
        let v = speedup_verdict(5_000, 0, 2);
        match v {
            SpeedupVerdict::ParallelConfirmed { speedup } => {
                assert!(speedup >= PARALLELISM_CONFIRMED_THRESHOLD);
            }
            other => panic!("expected ParallelConfirmed (degenerate); got {other:?}"),
        }
    }

    #[test]
    fn speedup_verdict_threshold_boundary_exact_match_confirms() {
        // 1.5× exactly → ParallelConfirmed (boundary inclusive). Sum=1500, wall=1000.
        let v = speedup_verdict(1_500, 1_000, 2);
        assert!(matches!(v, SpeedupVerdict::ParallelConfirmed { .. }));
    }

    #[test]
    fn speedup_verdict_threshold_boundary_just_below_warns() {
        // 1.49× → SeriallySuspect (just below the inclusive threshold).
        let v = speedup_verdict(1_490, 1_000, 2);
        assert!(matches!(v, SpeedupVerdict::SeriallySuspect { .. }));
    }

    #[test]
    fn derive_profile_name_strips_publisher_and_lowercases() {
        let n = derive_profile_name("nousresearch/hermes-4-70b", heuristics::TaskClass::Mid);
        assert_eq!(n, "hermes-4-70b-mid");
    }

    #[test]
    fn derive_profile_name_preserves_dot_in_version() {
        let n = derive_profile_name(
            "mlx-community/Qwen3-1.7B-MLX-MXFP4",
            heuristics::TaskClass::Fast,
        );
        assert_eq!(n, "qwen3-1.7b-mlx-mxfp4-fast");
    }

    #[test]
    fn derive_profile_name_collision_when_publishers_differ() {
        // Two different publishers, same base — derived names match (the
        // documented collision case warned about in cmd_scan).
        let a = derive_profile_name("unsloth/Qwen-7B", heuristics::TaskClass::Fast);
        let b = derive_profile_name("lmstudio-community/Qwen-7B", heuristics::TaskClass::Fast);
        assert_eq!(a, b);
    }

    #[test]
    fn derive_profile_name_handles_empty_id() {
        let n = derive_profile_name("", heuristics::TaskClass::Fast);
        assert!(
            n.starts_with("model-")
                || n.chars()
                    .next()
                    .map(|c| c.is_ascii_alphanumeric())
                    .unwrap_or(false),
            "expected name to start with alphanumeric or 'model-', got: {n}"
        );
    }

    #[test]
    fn derive_profile_name_strips_garbage_chars() {
        let n = derive_profile_name("publisher/some@weird*name!", heuristics::TaskClass::Mid);
        assert_eq!(n, "someweirdname-mid");
    }

    #[test]
    fn has_stripped_publisher_true_for_pubprefixed() {
        assert!(has_stripped_publisher("nousresearch/hermes"));
        assert!(!has_stripped_publisher("hermes"));
    }
}
