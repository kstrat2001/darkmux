//! The `darkmux` clap command tree — the `Cli`/`Cmd` derive types and every
//! subcommand's arg struct. Pure declarative surface: no handler logic lives
//! here (handlers stay in `main.rs`, or in their own module for the larger
//! subsystems — `lab_cli.rs`, `fleet_cli.rs`, `flow_cli.rs`, `config_cmd.rs`).
//!
//! Extracted from `main.rs` (mechanical, zero behavior change) to keep the
//! binary's entry point readable — this file is arg-surface-only, matching
//! the pattern the smaller command modules (`flow_cli`, `config_cmd`,
//! `phase_cli`) already established.

use clap::{Parser, Subcommand};

/// Shared `--profiles-file` flag (#661, renamed from `--config`). Collapses
/// the identical declaration that was duplicated across `ProfileCmd::List`/
/// `ProfileCmd::Scan`/`MachineCmd::Status`/`LabCmd::Run`/
/// `LabCmd::Characterize`/`LabCmd::Tune` into
/// one `#[command(flatten)]`-able struct — mechanical dedup only, the doc
/// string + `--profiles-file` flag name are unchanged. Two other subcommands
/// (`LabCmd::Eval`, `LabCmd::Loop`) declare their own doc text for
/// this same flag (a shorter variant and a `#984`-specific one respectively)
/// and are deliberately left un-flattened — collapsing them would change
/// their help text.
#[derive(clap::Args)]
pub(crate) struct ProfilesFileArg {
    /// Profiles-registry path (profiles.json). Overrides DARKMUX_PROFILES
    /// and the default search locations. (renamed from --config, #661)
    #[arg(long = "profiles-file")]
    pub(crate) profiles: Option<String>,
}

/// Shared `--json` flag ("Emit machine-readable JSON instead of styled text
/// (#907)." doc variant). Collapses the identical declaration duplicated
/// across `ProfileCmd::List`/`RoleCmd::List`/`RoleCmd::Show`/
/// `MachineCmd::Status`. Other `--json` flags with distinct doc text (schema
/// descriptions, "instead of the table", the #907-less short form, etc.) are
/// deliberately left un-flattened.
#[derive(clap::Args)]
pub(crate) struct JsonFlag {
    /// Emit machine-readable JSON instead of styled text (#907).
    #[arg(long)]
    pub(crate) json: bool,
}

/// Shared `--json` flag ("Emit machine-readable JSON instead of styled
/// text." doc variant, no `#907` reference). Collapses the identical
/// declaration duplicated across `LessonCmd::List`/`LessonCmd::Recall`.
#[derive(clap::Args)]
pub(crate) struct JsonFlagPlain {
    /// Emit machine-readable JSON instead of styled text.
    #[arg(long)]
    pub(crate) json: bool,
}

/// (#1129) `darkmux --version` shows the full build identifier (version + git
/// SHA, or `release`) — the same string the viewer header + `darkmux doctor`
/// render, so the first place anyone checks a version agrees with the rest.
/// A `OnceLock` hands clap the `&'static str` its `version =` needs from the
/// runtime `build_version()`.
fn build_version_static() -> &'static str {
    use std::sync::OnceLock;
    static V: OnceLock<String> = OnceLock::new();
    V.get_or_init(darkmux_types::build_version)
}

#[derive(Parser)]
#[command(name = "darkmux", version = build_version_static(), about = "Mission orchestrator and lab for local AI")]
pub(crate) struct Cli {
    #[command(subcommand)]
    pub(crate) command: Cmd,
}

#[derive(Subcommand)]
pub(crate) enum Cmd {
    /// Lab subcommands.
    Lab {
        #[command(subcommand)]
        sub: LabCmd,
    },
    /// Dispatch a single turn to the named role — the task-grain execution
    /// entry point (#1426). Loads the role manifest + `.md` system prompt and
    /// runs the role through the in-house container-bounded runtime (a
    /// per-dispatch `darkmux-runtime` Docker container) with the assembled
    /// message.
    ///
    /// The MESSAGE is positional. When it is omitted, darkmux reads the
    /// message from stdin, so a diff pipes straight in:
    /// `git diff | darkmux dispatch pr-reviewer`. For a message that begins
    /// with `-`, use the standard `--` separator:
    /// `darkmux dispatch coder -- --version bump`.
    Dispatch {
        /// Role id (e.g. `code-reviewer`). Must have a manifest at
        /// `templates/builtin/roles/<id>.json` (or under
        /// `~/.darkmux/roles/`) AND a sibling `.md` prompt file.
        role: String,
        /// Message body for the dispatch (positional). When omitted, the
        /// message is read from stdin (`git diff | darkmux dispatch
        /// pr-reviewer`); darkmux refuses to run if stdin is a terminal and
        /// no message was given, rather than hang waiting for input, and an
        /// empty or whitespace-only pipe (e.g. an empty `git diff`) is
        /// refused loudly rather than dispatched as a blank brief. A
        /// message that begins with `-` needs the standard `--` separator:
        /// `darkmux dispatch coder -- -starts-with-dash`.
        message: Option<String>,
        /// (#386) Read the message body from a file instead of the positional
        /// argument or stdin — for substantial briefs that would exceed the
        /// shell's ARG_MAX or clutter `ps`/shell history. The brief is passed
        /// to the runtime via a bind-mounted file, so it never lands on the
        /// `docker run` argv either. Conflicts with the positional MESSAGE.
        #[arg(long = "message-from-file", value_name = "PATH", conflicts_with = "message")]
        message_from_file: Option<std::path::PathBuf>,
        /// (#1054) Select a named profile from the machine's registry for this
        /// dispatch's model + context-window resolution, instead of the
        /// registry's `default_profile`. When the named profile isn't defined
        /// on this machine, the dispatch falls back to `default_profile` (with
        /// a note). Lets a machine-agnostic caller (e.g. the self-review CI
        /// workflow) NAME the profile it wants while each machine owns which
        /// lab-validated model that profile maps to.
        #[arg(long)]
        profile: Option<String>,
        /// Override the dispatch session id. Default: a fresh
        /// `crew-dispatch-<role>-<unix-micros>-<process-counter>` is
        /// generated per call, so consecutive dispatches don't share
        /// session state (which would otherwise pollute one task with
        /// another's context).
        #[arg(long)]
        session_id: Option<String>,
        /// Timeout in seconds (default: 600).
        #[arg(long, default_value = "600")]
        timeout: u32,
        /// Explicit working directory override (#143). When set, the
        /// internal runtime mounts this path into the container as the
        /// workspace, so the agent operates against the operator-named
        /// scope. When omitted, a fresh ephemeral tempdir is used.
        #[arg(long = "workdir", value_name = "PATH")]
        workdir: Option<std::path::PathBuf>,
        /// Phase id binding this dispatch to a phase in a mission (#714).
        /// When set, every flow record this dispatch emits carries
        /// `mission_id`/`phase_id` so the observability view groups it
        /// under its mission.
        #[arg(long = "phase-id", value_name = "ID")]
        phase_id: Option<String>,
        /// Skip the pre-flight checks. Use only for debugging.
        #[arg(long, hide = true)]
        skip_preflight: bool,
        /// Emit the runtime's response as a machine-parseable JSON
        /// envelope on stdout, with status lines routed to stderr.
        /// Schema: `{ result, final_assistant, metrics, trajectory_path }`.
        #[arg(long)]
        json: bool,
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
        /// cross-machine `--machine` dispatch.
        #[arg(long, value_name = "TAG")]
        image: Option<String>,
        /// (#1199) Cap the completion tokens of a single-shot hosted dispatch
        /// (a tool-less role on a remote endpoint). Default 4096. Raise it
        /// when a long output (e.g. a many-finding review) would truncate.
        /// No effect on container-path dispatches (local or agentic-remote).
        #[arg(long, value_name = "N")]
        max_completion_tokens: Option<u32>,
    },
    /// Run pre-flight diagnostic checks. Verifies the local setup (profile
    /// registry, LMStudio, models, runtime, RAM, power) and reports
    /// pass/warn/fail with actionable hints. Exit 0 if no failures, else 1.
    Doctor {
        /// (#1130) Print every check. Default output is issues-only — the
        /// build identity line + any warnings/failures, with the passing
        /// checks collapsed to a count. Use `-v` to see the full list.
        #[arg(long, short = 'v')]
        verbose: bool,
        /// (#1177) Live-probe each profile model's remote endpoint with ONE
        /// minimal chat completion through the same URL/auth path a real
        /// dispatch uses — verifies the credential actually WORKS (the
        /// default doctor only checks the Keychain item exists). Opt-in
        /// because each probe is a real API call: a paid endpoint bills a
        /// few tokens per probe (the probe's own token cost is shown in
        /// its result line).
        #[arg(long)]
        probe: bool,
    },
    /// Profile registry — the declaration surface for named model stacks.
    /// `profile list` shows the configured profiles; `profile scan` finds
    /// downloaded LMStudio models not yet in any profile; `profile draft`
    /// emits a starter profile JSON (#1426 — top-level `profiles` and `scan`
    /// merged into this family).
    Profile {
        #[command(subcommand)]
        sub: ProfileCmd,
    },
    /// This host's AI state — residents, live resources, roster (#1426).
    /// `machine` = is my host HEALTHY RIGHT NOW (live state, RAM truth);
    /// `doctor` = is my setup CORRECT (preflight, config). Bare `machine`
    /// routes to `machine status` (no separate overview render). Reads may
    /// target a roster peer over its serve daemon; MUTATIONS STAY LOCAL —
    /// `machine eject` only ever releases THIS host's `darkmux:` namespace.
    /// (#1426 folded the retired top-level `model`, `status`, and `fleet`
    /// families into this one.)
    Machine {
        #[command(subcommand)]
        sub: Option<MachineCmd>,
    },
    // (#1426 ship-2) The `crew` family retired ENTIRELY: phase 2 promoted
    // single-role dispatch to the top-level `darkmux dispatch` verb, and the
    // crew REGISTRY dissolved with the crews map — a crew is now a DERIVED
    // VIEW of a mission's resourcing (`darkmux_crew::resourcing`), never a
    // declared entity, so the registry-read verbs (list/show/index) go too.
    /// What darkmux knows — the durable memory that briefs future dispatches,
    /// one sub-noun per KIND. `memory lesson` is what the user authored
    /// (conventions, constraints, decisions + the reasoning behind them);
    /// `memory correction` is what their reviewer recorded when adjudicating a
    /// dispatch. Both surface to coder dispatches as injected brief blocks; new
    /// kinds slot in here rather than growing a new top-level verb. (#1426 —
    /// the `lessons` top-level verb retired into this family.)
    Memory {
        #[command(subcommand)]
        sub: MemoryCmd,
    },
    /// Role management — list and show role details from the SQLite index.
    Role {
        #[command(subcommand)]
        sub: RoleCmd,
    },
    /// Mission lifecycle — transition missions through their state machine.
    /// Mission status flows: Active ↔ Paused → Finalized. All transitions are
    /// operator-explicit; nothing auto-decides a mission is paused or done.
    /// Wall-clock UI consumes mission timestamps via `darkmux serve`.
    Mission {
        #[command(subcommand)]
        sub: MissionCmd,
    },
    /// Flow observability — record operator-facing flow events.
    Flow {
        #[command(subcommand)]
        sub: crate::flow_cli::FlowCmd,
    },
    /// Read/write `~/.darkmux/config.json` settings (#937). `set` validates the
    /// key + coerces the value; secrets stay in the Keychain. Distinct from
    /// `profile` (the profiles registry).
    Config {
        #[command(subcommand)]
        sub: crate::config_cmd::ConfigCmd,
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
        /// (#1247 Part 3) Root directory the lab observer lens scans for run
        /// clusters (any dir containing funnels.json / funnel-events.jsonl /
        /// scores.json). Falls back to `DARKMUX_LAB_DIR` when unset; unset
        /// entirely by default — no default scanning of arbitrary paths, the
        /// lab lens stays "not configured" until named. Machine-local by
        /// design: this daemon only ever reads ITS OWN machine's runs, never
        /// a remote path.
        #[arg(long = "lab-dir")]
        lab_dir: Option<std::path::PathBuf>,
    },
    /// One-command setup: install skills, optionally add session-start hook
    /// and CLAUDE.md integration so Claude Code knows about darkmux. Safe to
    /// re-run; refreshes the bundled skills after a darkmux upgrade (#1426 —
    /// `darkmux doctor` flags stale darkmux-* skills and points here).
    Init {
        /// Add a SessionStart hook to ~/.claude/settings.json that runs
        /// `darkmux machine status` so Claude sees the current stack at
        /// session start.
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
}

#[derive(Subcommand)]
pub(crate) enum RoleCmd {
    /// List every role in the index.
    List {
        #[command(flatten)]
        json: JsonFlag,
    },
    /// Show full details for a single role.
    Show {
        /// Role id to show.
        id: String,
        #[command(flatten)]
        json: JsonFlag,
    },
}

#[derive(Subcommand)]
pub(crate) enum MissionCmd {
    /// Global mission-control read (#829): the whole board — every mission
    /// grouped by status with phase progress, the inconsistencies that need
    /// attention (a Finalized mission with a non-terminal phase; an open mission
    /// whose phases are all done), and copy-pasteable reconcile commands.
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
    /// recorded (#849), and the mission's phases + how each ended. READ-ONLY.
    /// Run it (or let the finalize nudge prompt it) at mission completion; the
    /// `darkmux-mission-debrief` skill consumes `--json` to distill durable
    /// `memory lesson`s (with the why) for the next dispatch. NASA vocabulary:
    /// Mission · Debrief · Lessons (`Crew` was a derived view — the crew
    /// registry retired in #1426; staffing now resolves per dispatch). (#1465)
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
    /// Finalize a mission — the SUCCESS terminal (#1463). Drives every
    /// non-terminal phase to `Complete`, tears down each phase's worktree +
    /// branch, and transitions the mission to `Finalized` (stamps
    /// `finalized_ts=now()`). The frontier orchestrator does the git/gh work by
    /// hand (commit/push/PR/merge — its native job), then calls this to close
    /// out the darkmux-side state. The clear opposite of `abort` (which records
    /// `Abandoned` instead of `Complete`); both clean up whatever exists. Named
    /// to match the internal `finalize_mission` fn that graph/review runs call
    /// to auto-close. (Renamed from `close` in #1463; the `ship` verb it
    /// absorbs retired.)
    Finalize {
        id: String,
        /// Optional operator-supplied reasoning for finalizing the mission.
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
    /// Propose a Mission + Phases from unstructured input (#113 Phase 3).
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
    ///
    /// The input is any text on stdin — the pipe IS the interface, so the
    /// tools that already exist (gh, curl, cat) are the source adapters
    /// (#1426 — this retired the bespoke `darkmux external pull` wrapper):
    ///
    ///   gh issue view 42 | darkmux mission propose --from-stdin
    ///   curl -s <url>    | darkmux mission propose --from-stdin
    ///   cat notes.md     | darkmux mission propose --from-stdin
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
        /// After approval, immediately invoke `darkmux mission launch <id>`
        /// on the newly-persisted mission config. Skips the manual
        /// two-step. Defaults to false — operators who want to inspect the
        /// persisted config before launching can omit this flag.
        #[arg(long)]
        start: bool,
        /// Work-item / ticket id this mission realizes (e.g. `SYS-2598`).
        /// Stamped into the config draft and, at `mission launch`, onto the
        /// launched mission record; referenced as `{ticket}` by the repo's
        /// `.darkmux/conventions.json` templates (#816) for branch names,
        /// commit subjects, and PR titles.
        #[arg(long, value_name = "ID")]
        ticket: Option<String>,
    },
    /// Launch a named mission CONFIG into a brand-new mission RUN (#1284
    /// Packet 4a; run-identity fixed in #1503). Resolves `<config-id>`
    /// through the mission-config registry (user → on-disk → embedded — see
    /// `darkmux doctor`'s mission-config-registry check), validates it
    /// loud, collects its declared runtime-only `inputs` from `--input` /
    /// `--param` (bailing with a copy-pasteable example if any required
    /// input is missing), then mints `mission.json` + one phase per
    /// declared phase + a `config-snapshot.json` freezing the resolved
    /// config alongside the run. A graph with no tasks anywhere (a
    /// freeform/manual config) mints the run and starts the mission but
    /// leaves every phase transition operator-driven. A coder-phase graph
    /// executes worktree → coder → QA and then STOPS at an operator
    /// sign-off gate — the phase stays Running. The frontier orchestrator
    /// ships the git work by hand (commit/push/PR/merge), then `mission
    /// finalize` closes it out; `mission abort` tears it down (#1463). Launch
    /// never auto-closes past the gate. `review` (#1284 Packet 4b — the
    /// retired `pr-review run`) is dispatched through its OWN dedicated
    /// launcher instead: bundle → probe → dedup → judge → verify →
    /// synthesis, with no operator sign-off gate — its mission/phase
    /// envelope finalizes generically once the run completes, and the old
    /// CLI flags map one-to-one onto `--param key=value` (see
    /// `templates/builtin/mission-configs/review.json`'s own `inputs` doc
    /// for the mapping table). The run id is ALWAYS minted fresh — never
    /// derived from config+inputs (#1503): two launches of the same config
    /// with the same inputs are two DIFFERENT runs (AI work is
    /// non-deterministic), so relaunching with identical values mints a
    /// brand-new run rather than reusing or reopening a prior one. The
    /// config+inputs pairing is still recorded — as `Mission.spec`, a
    /// grouping key for corpus analysis, never identity.
    ///
    /// Exit codes (coder-phase / gate-less generic graphs): `0` freeform
    /// mint, or coder ran with QA clean/flags-only (gate banner, phase
    /// Running); `1` coder dispatch error; `2` QA found blocker(s) —
    /// resolve before shipping; `3` QA could not run — manual review
    /// required; `4` instance minted but the graph references step
    /// kind(s) this launcher can't construct yet. `review` exits `0` on
    /// any produced output (Clean/Degraded/Degenerate alike — CI-facing
    /// pass/fail comes from the rendered payload's `mode` field, not this
    /// code), propagating a hard failure for anything that fails before an
    /// envelope was ever produced.
    Launch {
        /// Mission config id to launch — a built-in (e.g. `coder-phase`)
        /// or a `darkmux mission propose`-drafted user-tier config.
        config_id: String,
        /// JSON file supplying the config's declared inputs (a flat
        /// object: input name → value).
        #[arg(long, value_name = "FILE")]
        input: Option<std::path::PathBuf>,
        /// An individual input override in `key=value` form. Repeatable;
        /// always wins over the same key in `--input`'s file.
        #[arg(long = "param", value_name = "KEY=VALUE")]
        params: Vec<String>,
        /// Per-dispatch timeout (seconds), for a config whose graph
        /// executes a dispatch. The default when omitted is PER CONFIG:
        /// coder-phase (and gate-less generic graphs) default 600;
        /// `review` defaults 3600 — the retired `pr-review run`'s own
        /// per-call default, preserved so a long judge pass doesn't newly
        /// time out (#1284 Packet 4b review gate, must-fix 1).
        #[arg(long)]
        timeout: Option<u32>,
    },
    /// Add a new Phase to an existing Mission mid-flight (#107).
    /// Operator-sovereign scope growth — alternative to either hand-
    /// editing JSON or filing a separate Mission for work that
    /// composes with the in-flight arc. Idempotent on exact-match
    /// (same id + mission + description); errors on collision. Phases
    /// are strictly linear (#1341) — `--after` places the new phase in
    /// `Mission.phase_ids` order; there is no separate dependency
    /// declaration.
    AddPhase {
        /// Mission id to extend (must exist).
        mission_id: String,
        /// Id for the new Phase (must not collide with any existing
        /// phase under a different mission; idempotent if same).
        #[arg(long = "phase-id")]
        phase_id: String,
        /// Description of the new Phase's scope.
        #[arg(long)]
        description: String,
        /// Insert the new phase immediately after this existing
        /// phase id (insert-in-middle). When omitted, the new
        /// phase is appended to the end of the mission's phase
        /// list (queue-on-end). The named id must already be in
        /// the mission's phase_ids — errors otherwise to surface
        /// typos and stale references.
        #[arg(long)]
        after: Option<String>,
        /// Optional operator-supplied reasoning for the mid-flight
        /// scope growth. Lands on the emitted flow record so the
        /// audit substrate captures *why* the mission grew here.
        #[arg(long)]
        reasoning: Option<String>,
    },
    /// Migrate mission + phase storage from the pre-#148 flat layout
    /// (`<crew>/missions/<id>.json`, `<crew>/phases/<id>.json`) into the
    /// per-mission nested layout (`<crew>/missions/<id>/mission.json`,
    /// `<crew>/missions/<id>/phases/<phase-id>.json`).
    ///
    /// ALSO synthesizes `config-snapshot.json` for every nested-layout
    /// instance that doesn't have one yet (#1284 Packet 4a) — a
    /// hand-authored mission minted before `mission launch` existed. Each
    /// gets a trivial, task-less config built from its own mission/phase
    /// JSONs, so it reads (in `mission status`, a future graph lens) as the
    /// freeform/manual instance it always was, without hand-editing.
    ///
    /// Dry-run by default — prints the proposed moves + synthesis without
    /// touching any files. Pass `--apply` to commit. Idempotent: re-running
    /// after a successful apply is a no-op. Orphan phases (whose
    /// `mission_id` has no matching mission on disk) are reported but never
    /// auto-moved; operator resolves them manually. A mission whose
    /// `phase_ids` reference a missing phase JSON skips ONLY that mission's
    /// snapshot synthesis (warned, not fatal) — existing flat→nested
    /// migration behavior is otherwise unchanged.
    Migrate {
        /// Apply the migration. Without this flag, only the proposed
        /// moves are printed (dry-run).
        #[arg(long)]
        apply: bool,
    },
    /// Fan-out dispatch all initial-depends phases (depends_on=[]) of a
    /// mission across the fleet in parallel (#247, PR-D.1). One role
    /// applies to every dispatched phase — operator-explicit per the
    /// CLAUDE.md doctrine that mission planning is judgment-bearing
    /// work the operator owns.
    ///
    /// Each phase becomes a WorkJob published to the single global
    /// `darkmux:work` stream (#590); the first available runner claims
    /// and runs each one. Default `--wait` blocks until all phases emit
    /// `dispatch.complete` (or timeout). `--no-wait` returns immediately
    /// with the session_ids for later polling.
    ///
    /// This is the keystone for Article 4's "operator hands off a
    /// mission and the fleet runs it" narrative.
    Dispatch {
        /// Mission id to dispatch.
        mission_id: String,
        /// Role to dispatch each phase under (e.g. `coder`,
        /// `code-reviewer`). One role applies to every dispatched phase.
        #[arg(long)]
        role: String,
        /// Optional advisory target machine for every phase. When
        /// omitted, jobs publish with no `target_machine` hint — the
        /// first available runner claims each (pull semantics). The hint
        /// is advisory (#590): any runner may claim regardless.
        #[arg(long, value_name = "ID")]
        machine: Option<String>,
        /// Per-phase dispatch timeout (seconds). Default 600.
        #[arg(long, default_value = "600")]
        timeout: u32,
        /// Return immediately after publishing all phase jobs instead
        /// of blocking on each `dispatch.complete`. Default is `--wait`.
        #[arg(long)]
        no_wait: bool,
    },
    /// Abort a mission — the KILL terminal (#1463). By default the WHOLE
    /// mission: removes every phase's worktree + branch, flips all non-terminal
    /// phases to `Abandoned`, and closes the mission. The clear opposite of
    /// `finalize` (which records `Complete`); both clean up whatever exists.
    /// Ends a stuck mission in one command (the `doom-loop-m4` case that used to
    /// need `phase abandon`×N + `close`). Pass `--phase <id>` to scope the
    /// teardown to a SINGLE gate-held coder-phase run instead of the whole
    /// mission. (Widened from single-phase in #1463; #782, #1426 ship-4.)
    Abort {
        /// Mission id.
        mission_id: String,
        /// Scope the teardown to one phase (the narrow single-run abort).
        /// When omitted, the WHOLE mission is aborted: every non-terminal
        /// phase torn down + Abandoned, the mission closed.
        #[arg(long, value_name = "ID")]
        phase: Option<String>,
    },
}

#[derive(Subcommand)]
pub(crate) enum MachineCmd {
    /// Show models currently loaded in LMStudio, grouped by ownership:
    /// darkmux-managed (under the `darkmux:` namespace) vs user state
    /// (everything else), plus which registered profile(s) the loaded set
    /// matches. Read-only. (#1426 — absorbs the retired top-level `status`
    /// verb's profile-match dimension; the default when `machine` is run
    /// with no sub-verb.)
    ///
    /// With a roster `[id]`, fetches THAT peer's residents over its serve
    /// daemon (same shared-token mechanism as `machine list --deep`); the
    /// profile-match column is local-only (it reads THIS host's registry).
    /// No id = this host.
    Status {
        /// Optional roster machine id to read remotely; omit for this host.
        id: Option<String>,
        #[command(flatten)]
        profiles: ProfilesFileArg,
        #[command(flatten)]
        json: JsonFlag,
    },
    /// Live machine resources (#1286, renamed from `model ledger` in #1426
    /// for vocabulary alignment — gestalt's port is `ResourceProbe`/`pools`,
    /// and this panel shows what that arbiter sees): per resident model,
    /// POTENTIAL (the commitment — weights + KV cache at the loaded ctx +
    /// transient margin) vs CURRENT (observed inference-worker footprint),
    /// color-stated green / amber ("made it by luck" — under the limit only
    /// because lazy allocation hasn't materialized; names the config shrink
    /// to reach green) / red (over the limit or memory pressure active),
    /// plus machine pressure rows (swap, compressor, memory-pressure free%).
    /// Read-only: kernel counters + lms metadata calls only — zero model
    /// dispatches; the output stamps the gather's own cost. The same data
    /// serves live at the daemon's GET /machine/resources (the viewer's
    /// machine lens).
    ///
    /// With a roster `[id]`, reads THAT peer's resources over its serve
    /// daemon; no id = this host.
    Resources {
        /// Optional roster machine id to read remotely; omit for this host.
        id: Option<String>,
        /// Emit machine-readable JSON instead of the table.
        #[arg(long)]
        json: bool,
    },
    /// Eject all darkmux-managed model loads (anything in the `darkmux:`
    /// namespace) on THIS host. User-loaded models are never touched. Use
    /// this when you want to release darkmux's RAM footprint without
    /// affecting other tools using LMStudio. MUTATION — local-only by
    /// design: never takes a roster id, never touches a peer (#1426).
    Eject {
        /// Show what would be ejected without actually unloading.
        #[arg(long, short = 'n')]
        dry_run: bool,
    },
    /// List the fleet roster + per-machine reachability (#1426 — absorbs the
    /// retired `fleet status`). Each machine gets a TCP-probe to its daemon
    /// port (300ms budget per probe). `--deep` additionally fetches each
    /// reachable peer's spec sheet (RAM, CPU, loaded models, darkmux
    /// version) via the daemon's `/machine/specs` endpoint (#275). `--json`
    /// for scripting; default is a table for operator eyes.
    List {
        /// Emit JSON instead of the human-readable table.
        #[arg(long)]
        json: bool,
        /// Aggregate `/machine/specs` from each reachable peer in
        /// addition to the reachability probe. Adds one HTTP GET per
        /// peer (~hundreds of ms over a tailnet).
        #[arg(long)]
        deep: bool,
    },
    /// Register a machine in the fleet roster (#1426 — absorbs the retired
    /// `fleet add`). Idempotent — calling again with the same `<id>` updates
    /// fields but preserves the original `added_unix_ms` so the fleet-age
    /// signal stays honest.
    Add {
        /// Logical machine id (what flow records carry as `machine_id`).
        /// Example: `studio`, `laptop`, `mini-1`.
        id: String,
        /// Tailnet address or DNS name to reach the daemon on. Example:
        /// `100.74.208.36`, `100.74.208.36:8765`, `studio.tailnet`. If
        /// no `:port` suffix, port 8765 is assumed.
        #[arg(long)]
        address: String,
        /// Optional one-line description for `machine list` + topology
        /// tooltips.
        #[arg(long)]
        description: Option<String>,
    },
    /// Remove a machine from the fleet roster (#1426 — absorbs the retired
    /// `fleet remove`). Doesn't touch the actual remote machine — just
    /// removes the local routing reference. Historical flow records from
    /// that machine remain in the audit chain and are still visible in the
    /// topology view.
    Remove {
        /// Logical machine id to remove.
        id: String,
    },
}

#[derive(Subcommand)]
pub(crate) enum ProfileCmd {
    /// List profiles in the registry. (#1426 — the retired top-level
    /// `darkmux profiles` verb; now `darkmux profile list`.)
    List {
        #[command(flatten)]
        profiles: ProfilesFileArg,
        #[command(flatten)]
        json: JsonFlag,
    },
    /// Scan the LMStudio model catalog for downloaded models that aren't yet
    /// covered by any profile. For each uncovered model, suggests a task class
    /// and rough memory impact. Run after downloading a new model in LMStudio
    /// to see whether you'd want to define a profile for it. (#1426 — the
    /// retired top-level `darkmux scan` verb; now `darkmux profile scan`.)
    Scan {
        #[command(flatten)]
        profiles: ProfilesFileArg,
    },
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
pub(crate) enum NotebookCmd {
    /// Draft a notebook entry from a recorded run via the active role.
    Draft {
        run_id: String,
        /// DM role id to dispatch the drafting prompt through. Resolves
        /// through `templates/builtin/roles/<role>.{json,md}` under the
        /// in-house container-bounded runtime.
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

/// (#1426, decision 17) The memory KINDS. Singular sub-nouns, matching the
/// `profile`/`role`/`machine` singulars — a new kind is a new sub-noun here,
/// never a new top-level verb.
#[derive(Subcommand)]
pub(crate) enum MemoryCmd {
    /// Engagement-context lessons the user AUTHORED — conventions,
    /// constraints, and decisions (with the reasoning behind them) that surface
    /// to coder dispatches as a `<lessons>` block. Stored in a durable,
    /// concurrent-safe SQLite `lessons.db`. Per-repo by default
    /// (`<repo>/.darkmux/lessons.db`, engagement-scoped); `--global` targets
    /// the cross-engagement `~/.darkmux/lessons.db`. (#994)
    Lesson {
        #[command(subcommand)]
        sub: LessonCmd,
    },
    /// The adjudication corrections the user's reviewer RECORDED — the
    /// verdicts and overrides they logged against a dispatch (`darkmux flow
    /// note --source adjudication`), carried forward into every later coder
    /// brief in the same mission so a correction made once is never re-derived.
    /// Read-only: corrections are recorded by the review path, never authored
    /// as a memory entry, so there is no `add` here. (#849)
    Correction {
        #[command(subcommand)]
        sub: CorrectionCmd,
    },
}

/// (#1426) The first verb #849's persisted corrections have ever had. READ-ONLY
/// by construction — see [`MemoryCmd::Correction`].
#[derive(Subcommand)]
pub(crate) enum CorrectionCmd {
    /// List the adjudication corrections recorded in the flow trail's recent
    /// window, oldest→newest. With no scope flag, every session in the window;
    /// `--mission` scopes to one mission's dispatches (exactly as the coder
    /// brief does), `--session` to a single dispatch.
    List {
        /// Scope to one mission's dispatch sessions — the same exact-set match
        /// the coder brief uses, so this shows precisely what that mission's
        /// next brief would carry. Conflicts with `--session`.
        #[arg(long, conflicts_with = "session")]
        mission: Option<String>,
        /// Scope to a single dispatch session id.
        #[arg(long)]
        session: Option<String>,
        /// How many of the most-recent day-files to read. Defaults to the same
        /// window the coder-brief injection reads.
        #[arg(long, default_value_t = darkmux_crew::corrections::ADJUDICATION_LOOKBACK_DAYS)]
        days: usize,
        #[command(flatten)]
        json: JsonFlagPlain,
    },
}

/// (#1465) Singular `LessonCmd` to match `MemoryCmd::Lesson` (and the
/// `profile`/`role`/`machine` singular sub-nouns). Pure internal rename from
/// `LessonCmd` — no wire change.
#[derive(Subcommand)]
pub(crate) enum LessonCmd {
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
        #[command(flatten)]
        json: JsonFlagPlain,
    },
    /// Edit a recorded lesson in place by its id (from `memory lesson list
    /// --json`).
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
    /// Remove a recorded lesson by its id (from `memory lesson list --json`).
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
        #[command(flatten)]
        json: JsonFlagPlain,
    },
}

/// (#1465, #1426) The recorded-run sub-verbs, folded out of the flat
/// `lab runs`/`lab inspect`/`lab compare` leaves into the `lab run`
/// kind-family. `lab run <workload>` still dispatches (a positional workload);
/// these route when no workload positional is given.
#[derive(Subcommand)]
pub(crate) enum RunCmd {
    /// List recent runs (most recent first). (was: `lab runs`)
    List {
        /// Show at most N runs (default: 5).
        #[arg(long, short = 'l', default_value = "5")]
        limit: usize,
        /// Show all runs (overrides --limit).
        #[arg(long, short = 'a')]
        all: bool,
    },
    /// Inspect a previously-recorded run. (was: `lab inspect`)
    Inspect {
        run: String,
        /// Also dump the full compaction summary text(s) the compactor model
        /// wrote during this run (read from trajectory.jsonl). Useful for
        /// methodology validation — confirming the compactor is producing
        /// substantive summaries rather than degenerate / empty output.
        #[arg(long)]
        summary: bool,
    },
    /// Compare two runs. (was: `lab compare`)
    Compare { run_a: String, run_b: String },
}

/// (#1465) The `lab workload` kind-family. `list` is the only member today —
/// spelled `list` (round-9 universal convention) instead of the retired flat
/// `lab workloads` plural-noun-as-verb leaf.
#[derive(Subcommand)]
pub(crate) enum WorkloadCmd {
    /// List available workloads. (was: `lab workloads`)
    List,
}

/// (#1465, #491) The `lab fixture` kind-family — the flat `lab fixtures`/
/// `lab register`/`lab unregister` leaves folded into one singular sub-noun.
#[derive(Subcommand)]
pub(crate) enum FixtureCmd {
    /// List registered fixtures + their paths + hashes (#491).
    /// (was: `lab fixtures`)
    List,
    /// Register a fixture directory in the lab registry by name (#491).
    /// Reads `.fixture.json` from `<path>`, computes a BLAKE3 content
    /// hash, records the pointer in `~/.darkmux/lab-registry.json`.
    /// The dir itself stays where it is — registry is just a lookup
    /// table. (was: `lab register`)
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
    /// preserved. (was: `lab unregister`)
    Unregister {
        /// Registry key (name from `.fixture.json` or `--name` at
        /// register time).
        name: String,
    },
}

#[derive(Subcommand)]
pub(crate) enum LabCmd {
    /// Dispatch a workload, or manage recorded runs (#1465, #1426).
    ///
    /// `lab run <workload>` dispatches a workload (one or more times — the
    /// unchanged run path). With NO workload positional, a sub-verb manages
    /// recorded runs: `lab run list`, `lab run inspect <id>`,
    /// `lab run compare <a> <b>` (the retired flat `lab runs`/`lab inspect`/
    /// `lab compare` leaves, folded into the `run` kind-family). `run` takes
    /// EITHER a workload positional OR a sub-verb — `args_conflicts_with_
    /// subcommands` keeps the two forms from mixing, and a token that is not a
    /// known sub-verb fills the workload positional. A user workload whose id
    /// collides with a sub-verb (`list`/`inspect`/`compare`) is still reachable
    /// as a workload via the `--` escape: `lab run -- <id>` (#1465).
    #[command(args_conflicts_with_subcommands = true)]
    Run {
        /// Workload id to dispatch (omit when using a run sub-verb).
        workload: Option<String>,
        #[arg(long, short = 'p')]
        profile: Option<String>,
        #[arg(long, short = 'n', default_value = "1")]
        runs: u32,
        #[command(flatten)]
        profiles: ProfilesFileArg,
        #[arg(long, short = 'q')]
        quiet: bool,
        #[command(subcommand)]
        sub: Option<RunCmd>,
    },
    /// Workload registry (`lab workload list`). (#1465)
    Workload {
        #[command(subcommand)]
        sub: WorkloadCmd,
    },
    /// Lab fixtures (`lab fixture list|register|unregister`). (#1465, #491)
    Fixture {
        #[command(subcommand)]
        sub: FixtureCmd,
    },
    /// Role eval (#1119, generalized in #1465) — run a role over a labeled
    /// corpus and score precision / recall / verdict / anchor against the
    /// ground-truth labels. `<role>` defaults to `pr-reviewer` (today's
    /// behavior); any role that emits the same `{verdict, findings}` JSON
    /// contract is a caller (a future coder-eval is free). Run across profiles
    /// (`--profile` / `--profiles-file`) to compare models reproducibly — the
    /// rows are the bake-off matrix. (Was `lab review-bench`; generalizing the
    /// snowflake dissolves the `lab review` vs `mission launch review`
    /// naming collision — `eval` names what it does.)
    Eval {
        /// The role to evaluate against the corpus. Defaults to `pr-reviewer`
        /// (the original `review-bench` behavior). The scorer is role-agnostic
        /// — it matches the role's emitted `{verdict, findings}` JSON against
        /// the ground-truth labels. The experimental condition flags below
        /// (`--freeform`/`--agentic`/`--dialectic`/`--funnel`) are
        /// `pr-reviewer`-specific and ignore this positional (they dispatch
        /// fixed reviewer variant roles / pipelines); a follow-up moves those
        /// behind per-role config (#1465).
        #[arg(default_value = "pr-reviewer")]
        role: String,
        /// Directory of labeled cases (`<id>.diff` + `<id>.label.json`).
        #[arg(
            long = "cases-dir",
            default_value = "templates/builtin/lab-fixtures/pr-review-bench/cases"
        )]
        cases_dir: String,
        /// Profile (the model axis) — defaults to the registry's default_profile.
        #[arg(long, short = 'p')]
        profile: Option<String>,
        /// Profiles-registry path (profiles.json). Overrides DARKMUX_PROFILES.
        #[arg(long = "profiles-file")]
        profiles: Option<String>,
        /// Per-case dispatch timeout in seconds.
        #[arg(long, default_value = "600")]
        timeout: u32,
        /// (#1198) Where to write the scores.json artifact (default: a
        /// `review-bench-<ts>/scores.json` under the runs dir).
        #[arg(long = "scores-out")]
        scores_out: Option<std::path::PathBuf>,
        /// Dispatch the free-form `pr-reviewer-freeform` role (ordinary prose,
        /// `MUST FIX:`/`CONSIDER:` marker lines, no JSON grammar lock) instead
        /// of the shipped grammar-constrained `pr-reviewer` — to measure
        /// whether the JSON contract itself suppresses recall.
        #[arg(long, conflicts_with = "agentic")]
        freeform: bool,
        /// Dispatch the `pr-reviewer-agentic` role with each case's repository
        /// tree (at the reviewed commit) mounted as the workdir — the
        /// production agentic condition (#1197). Requires --workdirs.
        #[arg(long)]
        agentic: bool,
        /// (#1222) Dispatch the dialectic (adversarial) pipeline instead of a
        /// single reviewer: prosecutor → defender → judge as three chained
        /// dispatches; the judge's sustained charges are the review, and each
        /// case's debate envelope lands beside scores.json. The advocates run
        /// agentic, so this requires --workdirs.
        #[arg(long, conflicts_with_all = ["freeform", "agentic"])]
        dialectic: bool,
        /// (#1222 Phase B packet 7) Dispatch the review funnel (bundles →
        /// probe roles → dedup → double-confirm judge) instead of a single
        /// reviewer or the dialectic pipeline — the release-guard validation
        /// mode: recall/precision scored EXACTLY like every other mode. Requires
        /// --workdirs (the probe/judge seats read the case's repo tree, like
        /// --agentic/--dialectic); every review seat is pinned to one profile
        /// (--roster-profile, else --profile, else the registry's
        /// default_profile) via the role→profile resolver (#1475).
        #[arg(long, conflicts_with_all = ["freeform", "agentic", "dialectic"])]
        funnel: bool,
        /// Evidence root for --agentic / --dialectic / --funnel: one
        /// subdirectory per case id holding that case's repo tree
        /// (`git archive <commit> | tar -x -C <root>/<id>`).
        #[arg(long)]
        workdirs: Option<std::path::PathBuf>,
        /// (#1222) Per-seat profile override (dialectic); falls back to
        /// --profile. Debug phase: leave unset — one profile, all seats.
        #[arg(long = "prosecutor-profile", requires = "dialectic")]
        prosecutor_profile: Option<String>,
        /// (#1222) Per-seat profile override (dialectic); falls back to --profile.
        #[arg(long = "defender-profile", requires = "dialectic")]
        defender_profile: Option<String>,
        /// (#1222) Per-seat profile override (dialectic); falls back to
        /// --profile. The later single-variable escalation: point this at a
        /// denser local or remote-endpoint profile while the advocates stay.
        #[arg(long = "judge-profile", requires = "dialectic")]
        judge_profile: Option<String>,
        /// (#1475, the `--roster-profile` flag; renamed from `--crew` in #1465)
        /// The one profile the bench pins EVERY review seat (probe / judge /
        /// verify) to for a controlled funnel run — via the per-run role→profile
        /// override. Falls back to --profile, else the registry's
        /// `default_profile`.
        #[arg(long = "roster-profile", requires = "funnel")]
        roster_profile: Option<String>,
        /// (#1222) Funnel model-cycling mode: "sequential" | "parallel" |
        /// "auto" (default: auto — resolved once per run against the local
        /// hardware tier).
        #[arg(long = "exec-mode", requires = "funnel")]
        exec_mode: Option<String>,
        /// (#1475) The probe draw BREADTH per probe role for this run, applied
        /// to every probe seat. Omitted ⇒ one draw per probe role (the funnel
        /// staffs three distinct probe roles, so the default already draws
        /// three). Must be at least 1 — a 0 draw count guarantees a degenerate
        /// run (zero probe flags).
        #[arg(long, requires = "funnel", value_parser = clap::value_parser!(u32).range(1..))]
        k: Option<u32>,
        /// (#1222) Run an external bundler
        /// (`<cmd> --worktree <dir> --diff <file>`) per case instead of the
        /// built-in Rust bundler.
        #[arg(long, requires = "funnel")]
        bundler: Option<String>,
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
        /// dispatches (its `mission-run-<id>-<phase>` sessions). Without it,
        /// only the repo's authored lessons inject. Requires `--ab` (clap
        /// errors otherwise, so the flag is never a silent no-op).
        #[arg(long = "inject-from-mission", requires = "ab")]
        inject_from_mission: Option<String>,
        /// Emit the loop report as JSON instead of the human table.
        #[arg(long)]
        json: bool,
    },
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
        #[command(flatten)]
        profiles: ProfilesFileArg,
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
        #[command(flatten)]
        profiles: ProfilesFileArg,
    },
    /// Lint the lab registry — schema check, path existence, content
    /// hash recompute, required-files presence (#491). Cheap + offline:
    /// no dispatches, no network. Doctor is the discoverability layer
    /// for the lab subsystem.
    Doctor,
    /// Notebook — agent-as-scribe for lab notebook entries. A lab HAS a
    /// notebook: `lab notebook draft <run-id>` authors an entry from a run's
    /// artifacts, `lab notebook list` enumerates recorded entries. (#1426 —
    /// the retired top-level `darkmux notebook` verb; now under `lab`.)
    Notebook {
        #[command(subcommand)]
        sub: NotebookCmd,
    },
}
