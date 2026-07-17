//! Dispatch a crew member (role) for a single turn.
//!
//! This is the operator-facing entry point that ties the crew schema
//! (`templates/builtin/roles/<id>.{json,md}`) to the in-house container-
//! bounded runtime (`dispatch_internal`). This module owns the pieces that
//! are runtime-neutral: the licensed-adjacent acknowledgment gate, session
//! id generation, cross-phase message/output threading, flow-record
//! builders, and fleet routing decisions. The actual dispatch execution
//! (Docker container spawn, agent loop, trajectory) lives in
//! `dispatch_internal.rs`.
//!
//! (2.0: the `openclaw` shell-out runtime and `darkmux crew sync` were
//! removed — see #1405. The in-house runtime is the only dispatch path.)

use anyhow::{anyhow, bail, Context, Result};
use std::fs;
use std::io::{BufRead, IsTerminal, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

/// Roles whose prompts operate in domains regulated by professional
/// licensure (health, law, athletics-as-RD-adjacent). Each prompt opens
/// with a "You are NOT a physician / attorney / trainer" framing, but the
/// operator never sees that text unless they go read the .md file. The
/// CLI-side acknowledgment gate (`require_licensed_adjacent_ack`) makes
/// the same disclaimer visible to the operator before first dispatch,
/// and records the timestamped ack at
/// `~/.darkmux/acks/<role>.ack`. The ack is operator-sovereign:
/// the operator can pre-create the file (`touch ~/.darkmux/acks/<role>.ack`)
/// to skip the prompt in scripted contexts, or delete it to re-trigger.
const LICENSED_ADJACENT_ROLES: &[&str] = &["health-research", "legal-research", "fitness-coach"];

/// Resolve the directory where licensed-adjacent acknowledgment files
/// live. Defaults to `~/.darkmux/acks/`. The `DARKMUX_ACK_DIR` env var
/// overrides — used by tests, also available for operators who want to
/// keep the acks in a different location.
fn ack_dir() -> Result<PathBuf> {
    // env(DARKMUX_ACK_DIR) > config.dirs.ack > ~/.darkmux/acks (#661 Slice 3).
    if let Some(p) = darkmux_types::config_access::ack_dir_override() {
        return Ok(p);
    }
    let home = dirs::home_dir().ok_or_else(|| anyhow!("no home directory found"))?;
    Ok(home.join(".darkmux").join("acks"))
}

fn ack_file_for(role_id: &str) -> Result<PathBuf> {
    Ok(ack_dir()?.join(format!("{role_id}.ack")))
}

/// Print the licensed-adjacent disclosure banner to stderr. Separated so
/// tests can verify the gate's behavior without coupling to terminal IO.
fn print_licensed_adjacent_banner(role_id: &str) {
    eprintln!();
    eprintln!("=== licensed-adjacent role: {role_id} ===");
    eprintln!("This role operates in a domain regulated by professional licensure.");
    eprintln!("It is a research / organization assistant — NOT a substitute for a");
    eprintln!("licensed professional. The role's full doctrine is in the .md prompt");
    eprintln!("at templates/builtin/roles/{role_id}.md in the darkmux source");
    eprintln!("(or your override at ~/.darkmux/roles/{role_id}.md if set).");
    eprintln!();
    eprintln!("By acknowledging, you confirm you understand:");
    eprintln!("  - The local LLM may deviate from its system prompt under adversarial");
    eprintln!("    or persistent prompting. The prompt IS the only runtime boundary.");
    eprintln!("  - You are solely responsible for following jurisdiction-specific");
    eprintln!("    licensure rules (UPL / UPM / scope-of-practice).");
    eprintln!("  - Time-sensitive situations (medical emergency, served lawsuit,");
    eprintln!("    acute pain) go to professionals, not this tool.");
    eprintln!();
}

/// Licensed-adjacent ACK gate. For roles whose prompts operate in
/// regulated domains, require an operator acknowledgment on first
/// dispatch. The ack persists at `~/.darkmux/acks/<role>.ack` (or
/// `$DARKMUX_ACK_DIR/<role>.ack` if set).
///
/// **Operator-sovereign escape hatches:**
/// - Pre-create the file (`mkdir -p ~/.darkmux/acks && touch
///   ~/.darkmux/acks/<role>.ack`) to skip the prompt in scripted use.
/// - Delete the file to re-trigger the prompt on next dispatch.
///
/// **Non-interactive without prior ack:** bails with a clear error and
/// the operator-facing instruction for how to pre-acknowledge.
///
/// **No-op for non-licensed-adjacent roles.**
pub(crate) fn require_licensed_adjacent_ack(role_id: &str) -> Result<()> {
    if !LICENSED_ADJACENT_ROLES.contains(&role_id) {
        return Ok(());
    }
    let ack_path = ack_file_for(role_id)?;
    if ack_path.exists() {
        return Ok(());
    }

    print_licensed_adjacent_banner(role_id);

    // Non-interactive (stdin not a TTY) → bail with operator-facing
    // remediation. The contract is that the ack is operator-explicit;
    // we don't auto-acknowledge for scripted callers.
    if !std::io::stdin().is_terminal() {
        bail!(
            "licensed-adjacent role `{role_id}` requires operator acknowledgment, \
             but stdin is not a TTY. To pre-acknowledge in scripted contexts, run:\n\
             \n  mkdir -p {} && touch {}\n\
             \nThen re-run the dispatch.",
            ack_dir()?.display(),
            ack_path.display()
        );
    }

    // Interactive: prompt for the ACKNOWLEDGE token. Anything else aborts.
    eprint!("Type ACKNOWLEDGE to continue (or Ctrl-C to abort): ");
    std::io::stderr().flush().ok();
    let mut input = String::new();
    let stdin = std::io::stdin();
    stdin
        .lock()
        .read_line(&mut input)
        .context("reading acknowledgment from stdin")?;
    if input.trim() != "ACKNOWLEDGE" {
        bail!(
            "acknowledgment not given (got `{}`); dispatch aborted",
            input.trim()
        );
    }

    let dir = ack_dir()?;
    fs::create_dir_all(&dir)
        .with_context(|| format!("creating ack directory at {}", dir.display()))?;
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let stamp = format!("acknowledged_at_unix_seconds={now}\n");
    fs::write(&ack_path, stamp)
        .with_context(|| format!("writing ack file at {}", ack_path.display()))?;
    eprintln!();
    eprintln!("Acknowledged. Recorded at {}.", ack_path.display());
    eprintln!();
    Ok(())
}

/// Which agent runtime services a dispatch. As of 2.0 (#1405) the
/// in-house container-bounded Rust runtime (see `runtime/` and
/// `dispatch_internal.rs`) is the ONLY dispatch path — the legacy
/// `openclaw` shell-out runtime and its `--runtime` opt-in flag were
/// removed. The enum survives as a single-variant type because
/// `DispatchOpts.runtime` and the fleet `WorkJob.runtime` field are
/// serialized/round-tripped across the queue boundary; collapsing it
/// further would touch that schema for no behavioral gain.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Runtime {
    #[default]
    Internal,
}

#[derive(Debug)]
pub struct DispatchOpts {
    pub role_id: String,
    pub message: String,
    /// Optional delivery target in `<channel>:<target>` form
    /// (e.g. `discord:1500166601909993503`). Not consumed by the
    /// internal runtime today — reserved for a future delivery
    /// integration.
    pub deliver: Option<String>,
    pub session_id: Option<String>,
    pub timeout_seconds: u32,
    /// Skip the pre-flight checks. Use only when explicitly debugging.
    pub skip_preflight: bool,
    /// When `true`, request the runtime emit a machine-parseable JSON
    /// envelope on stdout instead of the human-readable format —
    /// plumbed through to `--json` on the container's CLI.
    pub json: bool,
    /// Explicit working-directory override for the dispatch (#143).
    /// When `Some(path)`, the internal runtime mounts the given path
    /// into the container as the workspace. When `None`, a fresh
    /// tempdir is allocated. Per the operator-sovereignty contract,
    /// darkmux never auto-creates or auto-removes an operator-named
    /// `--workdir` — see `dispatch_internal`'s workspace setup.
    pub workdir: Option<PathBuf>,
    /// Optional phase id binding this dispatch to a phase in a
    /// mission. When set, the dispatch's flow records are stamped with
    /// `phase_id` (and the owning `mission_id`, resolved via
    /// [`resolve_mission_for_phase`]) so the viewer groups the dispatch
    /// under its mission. Provenance stamping ONLY — no message
    /// rewriting, no output persistence (the #146 Stage 1 cross-phase
    /// context injection was removed in #1405; `mission run`'s
    /// `coder_brief()` is the mechanism that carries context between
    /// phases now). When `None`, records carry no mission/phase fields.
    pub phase_id: Option<String>,
    /// Which agent runtime to dispatch through. See [`Runtime`].
    /// The in-house container-bounded runtime is the only value.
    pub runtime: Runtime,
    /// Target machine for the dispatch (#246 PR-C.3). When `Some(<id>)`
    /// and `<id>` differs from the local `DARKMUX_MACHINE_ID`, the
    /// dispatch is published to the single global `darkmux:work` stream
    /// via `fleet::publish_job` instead of running locally; the first
    /// available runner picks it up. The id is an **advisory hint**
    /// (#590): any runner may claim the job; a non-target runner logs a
    /// soft warning and proceeds (no NACK/requeue). When `None`, the
    /// dispatch runs locally — there is no implicit tier auto-route
    /// (retired in #590; capability-based auto-routing is the #590
    /// successor work).
    pub machine: Option<String>,
    /// Whether to block on completion when the dispatch routes to a
    /// remote machine (#246 PR-C.3). `true` — the default for
    /// `dispatch` — tails the local flow stream for the matching
    /// `session_id`'s `dispatch.complete` record and returns the
    /// outcome (preserves today's "spawn, block, see result" CLI
    /// ergonomics). `false` returns immediately with a synthetic
    /// success result; the operator polls via `darkmux flow tail`
    /// (or PR-D's `mission dispatch --no-wait` path).
    ///
    /// Ignored when the dispatch runs locally — local dispatches are
    /// always synchronous.
    pub wait: bool,
    /// Compaction config to pass to the internal runtime (#368). Each
    /// field is operator-derived from the active
    /// `profile.runtime.compaction.*` and translated to a runtime CLI
    /// flag. When `None`, the runtime falls back to its default. Env
    /// vars are NOT consulted by the runtime — the operator's tuning
    /// surface is the profile JSON, with these struct fields as the
    /// in-process plumbing layer between profile-read and CLI-emit.
    pub compaction: CompactionDispatchArgs,
    /// (#549) The resolved profile name the dispatch should use for
    /// model selection — the CLI `--profile` override when set, else
    /// `None` to fall back to the registry's `default_profile`. Set by
    /// the lab provider (which knows the resolved profile); `None`
    /// everywhere else preserves the default-profile behavior.
    pub profile_name: Option<String>,
    /// (#984) The profiles-registry path (`lab run --profiles-file`) the
    /// dispatch's model + context-window + utility-model resolution must load
    /// from. Without this, those resolvers call `load_registry(None)` —
    /// `env(DARKMUX_PROFILES) > default` — so a `--profiles-file` reached lab
    /// run's own lookup but NOT the dispatch, silently selecting the default
    /// registry's model. Set by the lab providers; `None` everywhere else
    /// preserves today's behavior (`env > default`).
    pub config_path: Option<String>,
    /// (#1199) Force the container/agentic path even for a tool-less role
    /// whose profile model is remote. The single-shot hosted path is a
    /// host-side curl — no container, no trajectory, no per-turn telemetry —
    /// so a bench comparing a local CONTAINERIZED run against a remote curl
    /// compares different substrates. Benches set this for the consistency
    /// guarantee; `false` everywhere else preserves the cheap path.
    pub force_container: bool,
    /// (#1199) Cap on the single-shot hosted path's completion tokens.
    /// `None` → the historical 4096 default. Length-axis bench tasks would
    /// silently truncate on remote but not local without this knob — a
    /// fairness bug masquerading as a capability gap.
    pub max_completion_tokens: Option<u32>,
    /// (#703) Override the Docker image the internal runtime dispatches
    /// into. `None` → the default `darkmux-runtime:latest` (slim base, the
    /// binary baked in). Set to ANY Linux image (e.g. `rust:slim`, the
    /// operator's own CI image) and darkmux **injects** its static runtime
    /// binary into it (bind-mount + entrypoint override) so the coder runs
    /// in that environment and can compile/test in-sandbox — the inner
    /// verify loop. No per-language darkmux images. The image needs `bash`
    /// and coreutils `timeout` (debian/ubuntu-family ship them; bare-alpine
    /// images need them added — Slice 2).
    pub image: Option<String>,
    /// Mock-model harness: override the container's `--base-url` — the
    /// LMStudio-compatible chat-completions host the runtime dials for a
    /// LOCAL-brain dispatch. `None` (the default) leaves the runtime's
    /// baked-in `http://host.docker.internal:1234/v1` default in place
    /// (real LMStudio on the host). Point this at a mock chat-completions
    /// server (e.g. `http://host.docker.internal:<port>/v1`) to run the
    /// real container-based dispatch machinery — real `docker run`, real
    /// agent loop, real flow records — against a scripted/deterministic
    /// fake response instead of a real model, with zero LMStudio/GPU
    /// involvement. The mock server itself is the standalone
    /// `tools/darkmux-mock-model` binary — a genuinely separate process
    /// reached over a real socket, not a function-call fake — see its
    /// crate doc and `crates/darkmux-crew/tests/mock_dispatch_proof.rs`.
    pub model_base_url_override: Option<String>,
}

/// Host-side compaction config passthrough to the internal runtime
/// (#368). Each field maps 1:1 to a runtime CLI flag. The host
/// constructs from a `Profile`; `crew::dispatch_internal::dispatch`
/// translates to `--compact-threshold-tokens N`, `--compactor-model
/// id`, `--compact-threshold-ratio f`, `--context-window N`,
/// `--compact-strategy <kebab>`, `--bail-after-compactions N`, and
/// `--compactor-custom-instructions <text>` flags. Flag names must match
/// the runtime's parser verbatim — an unknown flag exits the container 2.
///
/// All optional: `None` ⇒ don't pass the flag ⇒ runtime uses its
/// hardcoded default for that knob.
#[derive(Debug, Clone, Default)]
pub struct CompactionDispatchArgs {
    /// Absolute trigger. Set from `profile.runtime.compaction.threshold_tokens`
    /// (typed v0.1 field, #357).
    pub threshold_tokens: Option<u32>,
    /// Compactor model override. `None` by default — the runtime falls
    /// back to its hardcoded default compactor model (or the machine's
    /// bound `internal.utility` model via `apply_utility_model` below).
    pub compactor_model: Option<String>,
    /// Adaptive-trigger fraction (0.1-0.9). Set from typed
    /// `profile.runtime.compaction.threshold_ratio` (#368 T2-A).
    pub threshold_ratio: Option<f32>,
    /// Primary model's loaded context window. Set from
    /// `profile.models[primary].n_ctx`. Required for the formula
    /// trigger to compute; absent ⇒ formula trigger is disabled
    /// even when `threshold_ratio` is set.
    pub context_window: Option<u32>,
    /// Compaction strategy. Set from typed
    /// `profile.runtime.compaction.strategy` (#372 T2-A). When
    /// `None`, runtime uses default Narrative. Setting
    /// `Some(StructuredSlot)` opts the dispatch into tier-2.
    pub strategy: Option<darkmux_types::CompactionStrategy>,
    /// (#377) Escalation bound — after this many compactions, the
    /// runtime emits `TerminalReason::EscalationTriggered` and exits
    /// instead of continuing the agent loop. Set from
    /// `profile.runtime.compaction.reserve.bail_after_compactions`
    /// (typed field that landed in #357). The KISS-doubled answer
    /// from Beat 44 closure: *bound the cost and escalate past the
    /// bound*. `None` disables (back-compat / unbounded).
    pub bail_after_compactions: Option<u32>,
    /// (#383) Operator-tunable text appended to the compactor's
    /// system prompt at compaction time. Set from typed
    /// `profile.runtime.compaction.custom_instructions`. Schema
    /// isolation: reads ONLY the typed field.
    pub custom_instructions: Option<String>,
}

impl CompactionDispatchArgs {
    /// Derive from a profile (operator's tuning source-of-truth).
    /// Reads the typed fields under `profile.runtime.compaction.*`.
    /// Picks the primary model's `n_ctx` as the context_window (needed
    /// for formula trigger).
    pub fn from_profile(profile: &darkmux_types::Profile) -> Self {
        let comp = profile.runtime.as_ref().and_then(|r| r.compaction.as_ref());
        let threshold_tokens = comp
            .and_then(|c| c.threshold_tokens)
            .and_then(|v| u32::try_from(v).ok());
        // (#368 clean break) Compactor model is a typed field only —
        // no legacy-shape `extras["model"]` fallback. Until a typed
        // `compaction.compactor_model` field exists, the runtime uses
        // its hardcoded default `darkmux:qwen3-4b-instruct-2507`,
        // which matches what a dispatch loads for the standard
        // gestalt-residency workflow.
        let compactor_model: Option<String> = None;
        // (#368 clean break) Read from the typed schema field.
        // Operators wanting the adaptive trigger set
        // `profile.runtime.compaction.threshold_ratio` directly.
        let threshold_ratio = comp.and_then(|c| c.threshold_ratio).map(|f| f as f32);
        // (#590) Context window for the compaction trigger comes from the
        // profile's default model (default_model, or first model). (#1282)
        // A model with no declared `n_ctx` (endpoint-bearing) yields `None` —
        // the formula trigger is disabled, same as any window-less profile.
        let context_window = profile
            .default_model_id()
            .and_then(|id| profile.models.iter().find(|m| m.id == id))
            .and_then(|m| m.n_ctx);
        // (#372 T2-A/T2-C) Strategy is a typed field on the schema;
        // read directly. When operator hasn't set it, runtime falls
        // back to Narrative default.
        let strategy = comp.and_then(|c| c.strategy);
        // (#377) Escalation bound — read from typed
        // `compaction.reserve.bail_after_compactions` field that
        // landed in #357. The profile-level value here is the
        // FALLBACK; `apply_role_override` (called by dispatchers
        // that know the role) overlays the per-role pin from the
        // role manifest's `bail_after_compactions` field.
        let bail_after_compactions = comp
            .and_then(|c| c.reserve.as_ref())
            .and_then(|r| r.bail_after_compactions);
        // (#383) Custom instructions — read from typed field only.
        let custom_instructions = comp.and_then(|c| c.custom_instructions.clone());
        Self {
            threshold_tokens,
            compactor_model,
            threshold_ratio,
            context_window,
            strategy,
            bail_after_compactions,
            custom_instructions,
        }
    }

    /// (#377) Apply per-role overrides on top of profile defaults.
    /// Lookup chain: role override > profile default > None
    /// (runtime default ⇒ unbounded). Call after `from_profile` from
    /// any dispatcher that knows which role is about to run; sites
    /// that don't have a role (phase_cli adhoc) can skip the call
    /// and the profile-level fallback applies.
    ///
    /// The role's `escalation_posture` field is parsed here too but
    /// is currently informational only — the host/skill layer in
    /// chunk 5 will branch on it when frontier handoff lands.
    pub fn apply_role_override(&mut self, role: &crate::types::Role) {
        if let Some(role_bail) = role.bail_after_compactions {
            self.bail_after_compactions = Some(role_bail);
        }
    }

    /// (#590) Overlay the machine-level utility model (`internal.utility`) as
    /// the compactor, UNLESS the caller already pinned one. Call after
    /// `from_profile` / `apply_role_override` from any dispatcher that has the
    /// registry. The utility model is the machine's standing support model —
    /// one global model for compaction (and future estimation / mission-
    /// compile), decoupled from the profile. `None` (no binding) ⇒
    /// untouched, so the runtime keeps its built-in default compactor.
    pub fn apply_utility_model(&mut self, utility_model_id: Option<&str>) {
        if self.compactor_model.is_none() {
            self.compactor_model = utility_model_id.map(str::to_string);
        }
    }
}

#[derive(Debug)]
pub struct DispatchResult {
    pub exit_code: i32,
    pub stdout: String,
    pub stderr: String,
    /// The session id actually used for this dispatch. Echoes back the
    /// caller-supplied `opts.session_id` when set, or the fresh one this
    /// dispatch generated when `opts.session_id` was `None` (closes #88 —
    /// without an explicit `--session-id`, per-agent session reuse can
    /// cause cross-task context pollution).
    pub session_id: String,
    /// Host path where the internal runtime's `.darkmux-runtime/`
    /// bookkeeping landed (the dir mounted into the container at
    /// `/darkmux-out`). `None` when the dispatch path doesn't produce
    /// out-of-band bookkeeping (e.g. the remote single-shot path).
    pub out_dir: Option<PathBuf>,
}

/// Process-local monotonic counter — guarantees uniqueness for rapid
/// successive `fresh_session_id` calls in the same process even when the
/// wall-clock micros component collides (loops faster than the system clock).
static SESSION_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Generate a fresh, unique session id for an unscoped `dispatch` call.
/// Shape: `crew-dispatch-<role>-<unix_micros>-<process_counter>`.
///
/// The `crew-dispatch-` prefix is a FROZEN data-contract identifier —
/// presence tests key on it. It predates the #1426 verb rename (`crew
/// dispatch` -> `dispatch`); do NOT rename it in a spelling-cleanup sweep.
///
/// The micros component distinguishes calls across processes (different
/// invocations of `darkmux dispatch` from a shell each get their own
/// process start time). The counter component distinguishes calls within
/// the same process (scripted callers or future server backends could call
/// faster than microsecond resolution allows). Together they guarantee no
/// two `fresh_session_id` calls return the same string, closing the
/// per-agent session reuse this helper is meant to prevent (#88).
pub fn fresh_session_id(role_id: &str) -> String {
    let micros = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_micros())
        .unwrap_or(0);
    let counter = SESSION_COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("crew-dispatch-{role_id}-{micros}-{counter}")
}

/// Resolve the path to the optional operator-identity file (#147).
/// Defaults to `~/.darkmux/identity.md`. The `DARKMUX_IDENTITY_PATH`
/// env var overrides — used by tests, also available for operators
/// with multi-user / multi-identity setups.
fn identity_path() -> Option<PathBuf> {
    // env(DARKMUX_IDENTITY_PATH) > config.dirs.identity > ~/.darkmux/identity.md
    // (None if no HOME and no override) (#661 Slice 3).
    darkmux_types::config_access::identity_path_override()
        .or_else(|| dirs::home_dir().map(|h| h.join(".darkmux").join("identity.md")))
}

/// Load the operator-identity content from `~/.darkmux/identity.md` if
/// present. Returns `Some(content)` when the file exists and is
/// non-empty, `None` otherwise. The file is optional — when absent, the
/// bootstrap-chatter pain class observed in the experiment surfaces
/// naturally and the operator can decide whether to author the file.
///
/// **Bounded scope** (#147): the identity file is intended for stable
/// operator-identity primitives — name, pronouns, timezone, work-mode
/// preference, language preference. Explicit non-goals: engagement
/// context (lives in dispatch messages per the engagement-not-CLI
/// doctrine), project-specific knowledge (lives in CLAUDE.md per
/// project), vision-bearing content (lives in the frontier orchestrator,
/// not in static files).
fn load_operator_identity() -> Option<String> {
    let path = identity_path()?;
    let content = fs::read_to_string(&path).ok()?;
    let trimmed = content.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(content)
    }
}

/// Compute the "effective" system prompt for a role — the role's
/// authored .md prompt, optionally augmented with operator-identity
/// content from `~/.darkmux/identity.md` (#147).
///
/// When the identity file is absent or empty: returns the role prompt
/// unchanged. The bootstrap chatter the operator sees is the agent's
/// honest surfacing of the missing context.
///
/// When the identity file is present: appends an `## About the operator`
/// section to the role prompt. Called from `dispatch_internal::dispatch`
/// before the system prompt is sent to the runtime.
pub(crate) fn augment_prompt_with_identity(role_prompt: &str) -> String {
    match load_operator_identity() {
        Some(identity) => format!(
            "{role_prompt}\n\n---\n\n## About the operator\n\n{}\n",
            identity.trim_end()
        ),
        None => role_prompt.to_string(),
    }
}

/// Max chars of stderr carried in the dispatch-error flow record (#1042).
/// `pub(crate)` so the internal-runtime path reuses the same bound.
pub(crate) const STDERR_EXCERPT_MAX: usize = 4000;

/// TAIL excerpt of `content` to `max` chars (char-safe, never mid-UTF-8), with a
/// leading marker when truncated. Unlike [`cap_parent_output`] (a head), this
/// keeps the END — a failing process's actual error almost always lands at the
/// tail of stderr. `max == 0` means "no cap". Pure, for testability. (#1042)
/// `pub(crate)` so the internal-runtime path emits the same bounded excerpt.
pub(crate) fn tail_excerpt(content: &str, max: usize) -> String {
    let trimmed = content.trim_end();
    let n = trimmed.chars().count();
    if max == 0 || n <= max {
        return trimmed.to_string();
    }
    let tail: String = trimmed.chars().skip(n - max).collect();
    format!("[… stderr truncated, showing last {max} of {n} chars]\n{tail}")
}

/// (#714) Resolve a phase's mission so every dispatch flow record can be
/// stamped with `mission_id` and group under its mission in the observability
/// view. `None` when there's no `--phase-id` or the phase manifest can't be
/// loaded — best-effort metadata, never a reason to fail the dispatch.
pub(crate) fn resolve_mission_for_phase(phase_id: Option<&str>) -> Option<String> {
    let phase_id = phase_id?;
    match crate::lifecycle::load_phase_by_id(phase_id) {
        Ok(s) => Some(s.mission_id),
        Err(_) => {
            eprintln!(
                "darkmux dispatch: phase `{phase_id}` not found; \
                 flow records won't carry a mission_id."
            );
            None
        }
    }
}

/// Run a single dispatch end-to-end.
///
/// Local dispatch entry point. Runs the role through the in-house
/// container-bounded runtime on THIS machine. Never routes across the
/// fleet — the local-vs-remote routing decision lives in
/// `fleet::dispatch_routed` (#463 cycle-break: moved up so `crew` doesn't
/// depend on `fleet`). User-facing callers go through
/// `fleet::dispatch_routed`; the fleet runner (already on the chosen
/// machine) calls this directly.
pub fn dispatch(opts: DispatchOpts) -> Result<DispatchResult> {
    crate::dispatch_internal::dispatch(opts)
}

/// Outcome of the `dispatch()` routing-decision branch. Extracted as a
/// pure shape so the (Some(machine), local_machine_id) matrix is
/// unit-testable without filesystem / env-var setup. (Wave-E.7 #255)
#[derive(Debug, PartialEq)]
pub enum RoutingDecision {
    /// Run locally. `matches_was_explicit=true` when the operator
    /// passed `--machine` matching the local id (vs. the no-`--machine`
    /// case where local is the implicit default).
    Local { matches_was_explicit: bool },
    /// Route via the work queue to `target`. `local_unknown=true`
    /// signals the publisher couldn't resolve its own machine_id —
    /// caller should emit an operator-visible warning before routing.
    Remote { target: String, local_unknown: bool },
}

/// Emit a `dispatch route` flow record at the moment the routing
/// decision is made and return the resolved session_id so the caller
/// can re-attach it to `opts.session_id`. This ensures the route
/// record's session_id matches the runner's subsequent `dispatch
/// start` / `dispatch complete` records — the topology UI's pair-
/// rendering depends on session_id continuity.
///
/// After #590 the only routed path is explicit `--machine`
/// (`target_machine: Some(id)`, `decision: "pinned"`); the tier
/// auto-route arm was retired, so `decision: "auto"` no longer occurs.
pub fn emit_route_record_and_resolve_session(
    opts: &DispatchOpts,
    target_machine: Option<&str>,
) -> String {
    let session_id = opts
        .session_id
        .clone()
        .unwrap_or_else(|| fresh_session_id(&opts.role_id));
    let payload = build_route_payload(target_machine);
    let mission_id = resolve_mission_for_phase(opts.phase_id.as_deref());
    let _ = darkmux_flow::record(build_dispatch_record_with_payload(
        darkmux_flow::Level::Info,
        "dispatch route",
        &opts.role_id,
        &session_id,
        None,
        mission_id.as_deref(),
        opts.phase_id.as_deref(),
        Some(payload),
    ));
    session_id
}

/// Construct the payload for a `dispatch route` flow record (#247
/// PR-C). Pure; testable in isolation. After #590 the payload carries
/// the advisory `target_machine` hint + the `decision` verdict only —
/// the former `role_tier` / `local_tier` fields are gone with tier
/// routing. `target_machine: Some(id)` signals an operator-pinned
/// explicit-machine dispatch; `None` is the local-fallthrough case (so
/// `decision` reduces to {`pinned`, `local`}). The #556 topology UI
/// previously colored edges by `role_tier`/`local_tier`; keeping
/// `target_machine` + `decision` is the agreed minimum the route record
/// must still carry.
fn build_route_payload(target_machine: Option<&str>) -> serde_json::Value {
    serde_json::json!({
        "target_machine": target_machine,
        // `decision` makes the operator-visible verdict explicit in
        // the audit trail without re-deriving it from the other
        // fields (the topology UI uses this to color routing edges).
        "decision": if target_machine.is_some() { "pinned" } else { "local" },
    })
}

/// Pure-function routing decision. `machine` is the operator's
/// `--machine` flag (None when omitted); `local_machine_id` is what
/// `flow::resolve_machine_id()` returned for the current process.
///
/// Decision matrix:
/// - `(None, _)` → Local (no override; existing local-path behavior)
/// - `(Some(t), Some(l))` where `t == l` → Local (matches_was_explicit=true)
/// - `(Some(t), Some(l))` where `t != l` → Remote (normal cross-machine)
/// - `(Some(t), None)` → Remote with `local_unknown=true` (operator-
///   visible warning; PR-C.3 review MEDIUM)
pub fn routing_decision(machine: Option<&str>, local_machine_id: Option<&str>) -> RoutingDecision {
    match (machine, local_machine_id) {
        (None, _) => RoutingDecision::Local {
            matches_was_explicit: false,
        },
        (Some(t), Some(l)) if t == l => RoutingDecision::Local {
            matches_was_explicit: true,
        },
        (Some(t), Some(_)) => RoutingDecision::Remote {
            target: t.to_string(),
            local_unknown: false,
        },
        (Some(t), None) => RoutingDecision::Remote {
            target: t.to_string(),
            local_unknown: true,
        },
    }
}

/// Build a flow record for a dispatch lifecycle event (`dispatch start`,
/// `dispatch complete`, `dispatch error`). All three share the same
/// session_id so the viewer pairs start↔end into a single wall-clock
/// arc per dispatch. `handle` is the role id (operator-readable label);
/// `model` is the resolved LMStudio model id (best-effort — `None` on
/// resolution failure).
///
/// Legacy wrapper around `build_dispatch_record_with_payload` for the
/// pre-#204 call shape. Emit sites now go through `_with_payload`
/// directly to carry runtime metadata; this wrapper survives for tests
/// + future callers that don't need payload.
#[allow(dead_code)]
pub fn build_dispatch_record(
    level: darkmux_flow::Level,
    action: &str,
    role_id: &str,
    session_id: &str,
    model: Option<&str>,
) -> darkmux_flow::FlowRecord {
    build_dispatch_record_with_payload(level, action, role_id, session_id, model, None, None, None)
}

/// (#1127) Max prompt-text length stamped on a `dispatch.start` record. The
/// full prompt is operator-useful run context (the viewer renders it as a
/// collapsed block), but an unbounded paste would bloat the per-day JSONL +
/// the Redis stream — so cap the EMITTED text. The cost is one capped string
/// per dispatch (start is one record per dispatch, not per turn). `prompt_chars`
/// always carries the FULL length, so the viewer detects truncation by
/// comparing it against the stored text's length.
pub(crate) const MAX_PROMPT_PAYLOAD_CHARS: usize = 16_000;

/// Char-safe truncation of a prompt for the `dispatch.start` payload.
pub(crate) fn capped_prompt(s: &str) -> String {
    s.chars().take(MAX_PROMPT_PAYLOAD_CHARS).collect()
}

/// Same as `build_dispatch_record` but with an explicit `payload` for
/// event-specific fields (#204). The richer dispatch events (turn,
/// tool, compaction, reasoning) use this directly; the bare
/// `build_dispatch_record` wrapper preserves the legacy call shape.
#[allow(clippy::too_many_arguments)]
pub fn build_dispatch_record_with_payload(
    level: darkmux_flow::Level,
    action: &str,
    role_id: &str,
    session_id: &str,
    model: Option<&str>,
    mission_id: Option<&str>,
    phase_id: Option<&str>,
    payload: Option<serde_json::Value>,
) -> darkmux_flow::FlowRecord {
    darkmux_flow::FlowRecord {
        ts: darkmux_flow::ts_utc_now(),
        level,
        category: darkmux_flow::Category::Work,
        tier: darkmux_flow::Tier::Local,
        stage: darkmux_flow::Stage::Dispatch,
        action: action.to_string(),
        handle: role_id.to_string(),
        phase_id: phase_id.map(String::from),
        session_id: Some(session_id.to_string()),
        // FROZEN data-contract value: consumed by the viewer's source join and
        // test-asserted. Predates the #1426 verb rename (`crew dispatch` ->
        // `dispatch`); do NOT rename in a spelling-cleanup sweep.
        source: Some("crew_dispatch".to_string()),
        model: model.map(String::from),
        reasoning: None,
        mission_id: mission_id.map(String::from),
        machine_id: None,
        machine_uid: None,
        orchestrator: None,
        prev_hash: None,
        hash: None,
        payload,
        work_id: None,
        attempt: None,
    }
}

/// Build a telemetry flow record (#557 slice 2). Same shape as
/// `build_dispatch_record_with_payload` but `category = Telemetry` and
/// the `source` is caller-supplied (`"detector"`, `"runtime"`, …) so the
/// observability viewer can discriminate telemetry sub-streams. The
/// `payload` carries the instrument-specific fields (the viewer aliases
/// the wire `payload` to `fields` client-side).
#[allow(clippy::too_many_arguments)]
pub fn build_telemetry_record(
    level: darkmux_flow::Level,
    action: &str,
    source: &str,
    role_id: &str,
    session_id: &str,
    model: Option<&str>,
    mission_id: Option<&str>,
    phase_id: Option<&str>,
    payload: serde_json::Value,
) -> darkmux_flow::FlowRecord {
    darkmux_flow::FlowRecord {
        ts: darkmux_flow::ts_utc_now(),
        level,
        category: darkmux_flow::Category::Telemetry,
        tier: darkmux_flow::Tier::Local,
        stage: darkmux_flow::Stage::Dispatch,
        action: action.to_string(),
        handle: role_id.to_string(),
        phase_id: phase_id.map(String::from),
        session_id: Some(session_id.to_string()),
        source: Some(source.to_string()),
        model: model.map(String::from),
        reasoning: None,
        mission_id: mission_id.map(String::from),
        machine_id: None,
        machine_uid: None,
        orchestrator: None,
        prev_hash: None,
        hash: None,
        payload: Some(payload),
        work_id: None,
        attempt: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    // ─── #1042 stderr tail excerpt for the dispatch-error record ───────
    #[test]
    fn tail_excerpt_keeps_the_tail_with_marker_when_truncated() {
        let s: String = ('a'..='z').cycle().take(100).collect(); // 100 ASCII chars
        let out = tail_excerpt(&s, 10);
        assert!(
            out.starts_with("[… stderr truncated, showing last 10 of 100 chars]\n"),
            "expected truncation marker, got: {out}"
        );
        assert!(out.ends_with(&s[s.len() - 10..]), "expected the last 10 chars");
    }

    #[test]
    fn tail_excerpt_returns_full_trimmed_when_under_cap() {
        // trailing whitespace trimmed; no marker when within the cap.
        assert_eq!(tail_excerpt("boom: exit 2\n\n", 4000), "boom: exit 2");
    }

    #[test]
    fn tail_excerpt_zero_means_no_cap() {
        assert_eq!(tail_excerpt("x\n", 0), "x");
    }

    #[test]
    fn tail_excerpt_is_char_safe_on_multibyte() {
        // The tail must fall on a char boundary — never panic / split a UTF-8
        // scalar. "héllo wörld 日本語" is 15 chars; last 3 = 日本語.
        let out = tail_excerpt("héllo wörld 日本語", 3);
        assert!(out.starts_with("[… stderr truncated, showing last 3 of 15 chars]\n"));
        assert!(out.ends_with("日本語"));
    }

    #[test]
    fn tail_excerpt_no_marker_at_exact_cap() {
        assert_eq!(tail_excerpt("abcde", 5), "abcde"); // n == max → no truncation
    }

    // ─── #590 apply_utility_model overlay ─────────────────────────────

    #[test]
    fn apply_utility_model_sets_compactor_when_unset() {
        let mut c = CompactionDispatchArgs::default();
        assert!(c.compactor_model.is_none());
        c.apply_utility_model(Some("darkmux:util-4b"));
        assert_eq!(c.compactor_model.as_deref(), Some("darkmux:util-4b"));
    }

    #[test]
    fn apply_utility_model_does_not_override_a_pinned_compactor() {
        let mut c = CompactionDispatchArgs {
            compactor_model: Some("operator-pinned".into()),
            ..Default::default()
        };
        c.apply_utility_model(Some("darkmux:util-4b"));
        assert_eq!(c.compactor_model.as_deref(), Some("operator-pinned"));
    }

    #[test]
    fn apply_utility_model_none_leaves_compactor_unset() {
        let mut c = CompactionDispatchArgs::default();
        c.apply_utility_model(None);
        assert!(c.compactor_model.is_none());
    }

    // ─── #557 slice 2 build_telemetry_record ──────────────────────────

    /// `build_telemetry_record` differs from the work-category dispatch
    /// builder in exactly two fields: `category = Telemetry` and a
    /// caller-supplied `source`. Everything else (tier=Local,
    /// stage=Dispatch, handle=role_id, session_id, model, payload) is
    /// copied verbatim. This asserts the discriminating fields plus the
    /// payload round-trip.
    #[test]
    fn build_telemetry_record_has_telemetry_category_and_caller_source() {
        let payload = serde_json::json!({
            "kind": "cycle",
            "severity": "warn",
            "detail": "`read` called 3× in the last 10 tool calls",
        });
        let rec = build_telemetry_record(
            darkmux_flow::Level::Info,
            "telemetry.detector",
            "detector",
            "coder",
            "sess-1",
            Some("darkmux:qwen3.6"),
            None,
            None,
            payload.clone(),
        );

        assert!(matches!(rec.category, darkmux_flow::Category::Telemetry));
        assert_eq!(rec.source.as_deref(), Some("detector"));
        assert!(matches!(rec.tier, darkmux_flow::Tier::Local));
        assert!(matches!(rec.stage, darkmux_flow::Stage::Dispatch));
        assert_eq!(rec.handle, "coder");
        assert_eq!(rec.session_id.as_deref(), Some("sess-1"));
        assert_eq!(rec.model.as_deref(), Some("darkmux:qwen3.6"));
        assert_eq!(rec.payload, Some(payload));
    }

    /// The observability viewer discriminates telemetry sub-streams on
    /// the *serialized* `category` + `source` strings. Confirm a
    /// telemetry record serializes to `"category":"telemetry"` and
    /// `"source":"detector"` (the wire-level contract the demo viewer
    /// keys on; it then aliases `payload` → `fields` client-side).
    #[test]
    fn telemetry_record_serializes_with_telemetry_category_and_detector_source() {
        let rec = build_telemetry_record(
            darkmux_flow::Level::Info,
            "telemetry.detector",
            "detector",
            "coder",
            "sess-1",
            None,
            None,
            None,
            serde_json::json!({ "kind": "cycle", "severity": "warn", "detail": "x" }),
        );
        let v = serde_json::to_value(&rec).unwrap();
        assert_eq!(v["category"], "telemetry");
        assert_eq!(v["source"], "detector");
        // payload (aliased to `fields` viewer-side) round-trips on the wire.
        assert_eq!(v["payload"]["kind"], "cycle");
    }

    // ─── #247 PR-C build_route_payload (post-#590 single-stream shape) ─

    /// Local-fallthrough payload — no target_machine; decision="local".
    /// After #590 the tier auto-route arm is gone, so the no-target case
    /// is the local-dispatch verdict (not "auto-route"). The payload no
    /// longer carries role_tier / local_tier.
    #[test]
    fn build_route_payload_no_target_has_local_decision() {
        let p = build_route_payload(None);
        assert_eq!(p["target_machine"], serde_json::Value::Null);
        assert_eq!(p["decision"], "local");
        assert!(
            p.get("role_tier").is_none(),
            "role_tier dropped with tier routing (#590)"
        );
        assert!(
            p.get("local_tier").is_none(),
            "local_tier dropped with tier routing (#590)"
        );
    }

    /// Pinned payload — operator-supplied advisory target_machine;
    /// decision="pinned". The explicit-machine path still emits a
    /// dispatch route record so the audit trail (and the #556 topology
    /// UI) capture that the operator made the decision (not the
    /// substrate).
    #[test]
    fn build_route_payload_pinned_has_target_and_pinned_decision() {
        let p = build_route_payload(Some("laptop"));
        assert_eq!(p["target_machine"], "laptop");
        assert_eq!(p["decision"], "pinned");
    }

    /// Minimum #556-coordination shape: every route record carries
    /// exactly `target_machine` + `decision` and nothing tier-shaped.
    #[test]
    fn build_route_payload_minimum_shape_is_target_and_decision() {
        let p = build_route_payload(Some("studio"));
        let obj = p.as_object().expect("route payload is a JSON object");
        let mut keys: Vec<&str> = obj.keys().map(String::as_str).collect();
        keys.sort_unstable();
        assert_eq!(keys, vec!["decision", "target_machine"]);
    }

    // ─── routing_decision (Wave-E.7 #255) ─────────────────────────────

    #[test]
    fn routing_decision_no_machine_is_local() {
        assert_eq!(
            routing_decision(None, Some("laptop")),
            RoutingDecision::Local {
                matches_was_explicit: false
            }
        );
        assert_eq!(
            routing_decision(None, None),
            RoutingDecision::Local {
                matches_was_explicit: false
            }
        );
    }

    #[test]
    fn routing_decision_machine_matches_local_is_local_explicit() {
        assert_eq!(
            routing_decision(Some("laptop"), Some("laptop")),
            RoutingDecision::Local {
                matches_was_explicit: true
            }
        );
    }

    #[test]
    fn routing_decision_machine_differs_is_remote_known_local() {
        assert_eq!(
            routing_decision(Some("studio"), Some("laptop")),
            RoutingDecision::Remote {
                target: "studio".to_string(),
                local_unknown: false,
            }
        );
    }

    #[test]
    fn routing_decision_machine_set_but_local_unknown_warns() {
        // The case PR-C.3 review M flagged: DARKMUX_MACHINE_ID unset +
        // hostname failure means we can't tell if --machine matches
        // local. Route via queue + signal the warning condition.
        assert_eq!(
            routing_decision(Some("studio"), None),
            RoutingDecision::Remote {
                target: "studio".to_string(),
                local_unknown: true,
            }
        );
    }

    #[test]
    #[serial_test::serial]
    fn licensed_adjacent_ack_passes_when_ack_file_exists() {
        let tmp = TempDir::new().unwrap();
        let prev = std::env::var("DARKMUX_ACK_DIR").ok();
        // Safety: tests mutate process env; the serial attribute keeps them
        // from racing each other.
        unsafe {
            std::env::set_var("DARKMUX_ACK_DIR", tmp.path());
        }
        std::fs::create_dir_all(tmp.path()).unwrap();
        std::fs::write(tmp.path().join("health-research.ack"), "test").unwrap();

        // ACK file present → returns Ok without prompting.
        require_licensed_adjacent_ack("health-research").unwrap();

        // Restore env.
        unsafe {
            match prev {
                Some(v) => std::env::set_var("DARKMUX_ACK_DIR", v),
                None => std::env::remove_var("DARKMUX_ACK_DIR"),
            }
        }
    }

    #[test]
    #[serial_test::serial]
    fn licensed_adjacent_ack_is_noop_for_other_roles() {
        // No DARKMUX_ACK_DIR set, no ack file, no TTY input — but for
        // a non-licensed-adjacent role, the gate is a no-op and returns Ok.
        // `serial_test::serial` is defensive: the function's current
        // implementation short-circuits before reading any env, but if a
        // future refactor moves env reads earlier this test must not race
        // the other two serialized tests that mutate DARKMUX_ACK_DIR.
        require_licensed_adjacent_ack("coder").unwrap();
        require_licensed_adjacent_ack("analyst").unwrap();
        require_licensed_adjacent_ack("scribe").unwrap();
    }

    #[test]
    #[serial_test::serial]
    fn augment_prompt_with_identity_passes_through_when_file_absent() {
        let tmp = TempDir::new().unwrap();
        let prev = std::env::var("DARKMUX_IDENTITY_PATH").ok();
        // Point at a non-existent file so the lookup misses cleanly.
        unsafe {
            std::env::set_var(
                "DARKMUX_IDENTITY_PATH",
                tmp.path().join("does-not-exist.md"),
            );
        }

        let augmented = augment_prompt_with_identity("# Role\n\nyou are X");
        assert_eq!(augmented, "# Role\n\nyou are X");

        unsafe {
            match prev {
                Some(v) => std::env::set_var("DARKMUX_IDENTITY_PATH", v),
                None => std::env::remove_var("DARKMUX_IDENTITY_PATH"),
            }
        }
    }

    #[test]
    #[serial_test::serial]
    fn augment_prompt_with_identity_appends_section_when_file_present() {
        let tmp = TempDir::new().unwrap();
        let identity_path = tmp.path().join("identity.md");
        fs::write(
            &identity_path,
            "Name: Kain.\nPronouns: He/Him.\nTimezone: Asia/Kuala_Lumpur.\n",
        )
        .unwrap();
        let prev = std::env::var("DARKMUX_IDENTITY_PATH").ok();
        unsafe {
            std::env::set_var("DARKMUX_IDENTITY_PATH", &identity_path);
        }

        let augmented = augment_prompt_with_identity("# Role\n\nyou are X");
        // Role prompt preserved verbatim at the start.
        assert!(
            augmented.starts_with("# Role\n\nyou are X"),
            "got: {augmented}"
        );
        // About-the-operator section appended.
        assert!(
            augmented.contains("## About the operator"),
            "got: {augmented}"
        );
        // Identity content present.
        assert!(augmented.contains("Name: Kain"), "got: {augmented}");
        assert!(augmented.contains("Asia/Kuala_Lumpur"), "got: {augmented}");
        // Separator between role prompt and identity.
        assert!(augmented.contains("\n\n---\n\n"), "got: {augmented}");

        unsafe {
            match prev {
                Some(v) => std::env::set_var("DARKMUX_IDENTITY_PATH", v),
                None => std::env::remove_var("DARKMUX_IDENTITY_PATH"),
            }
        }
    }

    #[test]
    #[serial_test::serial]
    fn augment_prompt_with_identity_treats_empty_file_as_absent() {
        let tmp = TempDir::new().unwrap();
        let identity_path = tmp.path().join("identity.md");
        fs::write(&identity_path, "   \n  \n").unwrap();
        let prev = std::env::var("DARKMUX_IDENTITY_PATH").ok();
        unsafe {
            std::env::set_var("DARKMUX_IDENTITY_PATH", &identity_path);
        }

        let augmented = augment_prompt_with_identity("# Role\n\nyou are X");
        // Empty/whitespace identity file = no augmentation. Operator
        // hasn't actually authored content, so we don't fabricate a section.
        assert_eq!(augmented, "# Role\n\nyou are X");

        unsafe {
            match prev {
                Some(v) => std::env::set_var("DARKMUX_IDENTITY_PATH", v),
                None => std::env::remove_var("DARKMUX_IDENTITY_PATH"),
            }
        }
    }

    #[test]
    #[serial_test::serial]
    fn resolve_mission_for_phase_returns_mission_for_known_phase() {
        // (#714) The resolution heart of the fix: a known phase id maps to
        // its mission so dispatch records can group under it.
        let tmp = TempDir::new().unwrap();
        let prev = std::env::var("DARKMUX_CREW_DIR").ok();
        unsafe {
            std::env::set_var("DARKMUX_CREW_DIR", tmp.path());
        }
        let phases_dir = tmp.path().join("missions").join("sweep").join("phases");
        fs::create_dir_all(&phases_dir).unwrap();
        fs::write(
            phases_dir.join("s694.json"),
            r#"{"id":"s694","mission_id":"sweep","description":"d","status":"planned","depends_on":[],"created_ts":0}"#,
        ).unwrap();

        assert_eq!(
            resolve_mission_for_phase(Some("s694")).as_deref(),
            Some("sweep")
        );
        // No phase id → no mission (one-off dispatch).
        assert!(resolve_mission_for_phase(None).is_none());

        unsafe {
            match prev {
                Some(v) => std::env::set_var("DARKMUX_CREW_DIR", v),
                None => std::env::remove_var("DARKMUX_CREW_DIR"),
            }
        }
    }

    #[test]
    #[serial_test::serial]
    fn resolve_mission_for_phase_returns_none_for_unknown_phase() {
        // An unresolvable phase warns (stderr) and degrades to None rather
        // than failing the dispatch — flow records just go ungrouped.
        let tmp = TempDir::new().unwrap();
        let prev = std::env::var("DARKMUX_CREW_DIR").ok();
        unsafe {
            std::env::set_var("DARKMUX_CREW_DIR", tmp.path());
        }
        // No manifests written under the crew dir.
        assert!(resolve_mission_for_phase(Some("does-not-exist")).is_none());

        unsafe {
            match prev {
                Some(v) => std::env::set_var("DARKMUX_CREW_DIR", v),
                None => std::env::remove_var("DARKMUX_CREW_DIR"),
            }
        }
    }

    #[test]
    #[serial_test::serial]
    fn licensed_adjacent_ack_bails_when_no_tty_and_no_ack_file() {
        let tmp = TempDir::new().unwrap();
        let prev = std::env::var("DARKMUX_ACK_DIR").ok();
        // Safety: serialized.
        unsafe {
            std::env::set_var("DARKMUX_ACK_DIR", tmp.path());
        }

        // Stdin in tests is not a TTY → the gate should bail with a
        // clear remediation message rather than block on read.
        let err = require_licensed_adjacent_ack("legal-research").unwrap_err();
        let s = format!("{err:#}");
        assert!(s.contains("requires operator acknowledgment"), "got: {s}");
        assert!(s.contains("mkdir -p"), "got: {s}");

        unsafe {
            match prev {
                Some(v) => std::env::set_var("DARKMUX_ACK_DIR", v),
                None => std::env::remove_var("DARKMUX_ACK_DIR"),
            }
        }
    }

    // ─── #88: fresh session id per dispatch ────────────────────────────────

    #[test]
    fn fresh_session_id_includes_role_micros_and_counter() {
        let id = fresh_session_id("code-reviewer");
        // Shape: `crew-dispatch-<role>-<micros>-<counter>`
        assert!(id.starts_with("crew-dispatch-code-reviewer-"), "got {id:?}");
        let suffix = id.trim_start_matches("crew-dispatch-code-reviewer-");
        // Suffix splits into <micros>-<counter>; both digit-only.
        let parts: Vec<&str> = suffix.split('-').collect();
        assert_eq!(
            parts.len(),
            2,
            "expected <micros>-<counter>, got {suffix:?}"
        );
        let micros: u128 = parts[0].parse().expect("micros should parse as u128");
        let _counter: u64 = parts[1].parse().expect("counter should parse as u64");
        // Plausibly-recent timestamp (post-2020-01-01 in micros).
        assert!(
            micros > 1_577_836_800_000_000,
            "suffix should be after 2020-01-01 (micros), got {micros}",
        );
    }

    #[test]
    fn fresh_session_id_uniqueness_under_rapid_calls() {
        // Two back-to-back calls must not collide. Microsecond resolution
        // guards against the same-second collision the prior implementation
        // had (would have re-introduced the per-agent session reuse #88
        // tried to fix). Generate a batch and assert all-unique.
        let ids: Vec<String> = (0..50).map(|_| fresh_session_id("coder")).collect();
        let unique: std::collections::HashSet<_> = ids.iter().cloned().collect();
        assert_eq!(
            unique.len(),
            ids.len(),
            "50 rapid calls produced {} unique ids (expected 50)",
            unique.len(),
        );
    }

    #[test]
    fn fresh_session_id_differs_across_roles() {
        // Same call instant, different roles → different ids.
        let a = fresh_session_id("coder");
        let b = fresh_session_id("scribe");
        assert_ne!(a, b);
        assert!(a.contains("-coder-"));
        assert!(b.contains("-scribe-"));
    }

    #[test]
    fn fresh_session_id_handles_roles_with_hyphens() {
        // `code-reviewer` is one of the production roles and contains a
        // hyphen; the format must preserve it cleanly (no escape, no split).
        let id = fresh_session_id("code-reviewer");
        assert!(id.starts_with("crew-dispatch-code-reviewer-"));
        // No double-hyphen artifact.
        assert!(!id.contains("crew-dispatch--"));
    }

    // ─── build_dispatch_record (Phase 2 of #104) ──────────────────────────

    #[test]
    fn dispatch_record_carries_role_id_session_and_local_tier() {
        let rec = build_dispatch_record(
            darkmux_flow::Level::Info,
            "dispatch start",
            "coder",
            "crew-dispatch-coder-12345-1",
            Some("darkmux:qwen3.6-35b-a3b"),
        );
        assert_eq!(rec.action, "dispatch start");
        assert_eq!(rec.handle, "coder");
        assert_eq!(
            rec.session_id.as_deref(),
            Some("crew-dispatch-coder-12345-1")
        );
        assert_eq!(rec.source.as_deref(), Some("crew_dispatch"));
        assert_eq!(rec.model.as_deref(), Some("darkmux:qwen3.6-35b-a3b"));
        assert!(matches!(rec.tier, darkmux_flow::Tier::Local));
        assert!(matches!(rec.stage, darkmux_flow::Stage::Dispatch));
        assert!(matches!(rec.category, darkmux_flow::Category::Work));
        // The bare `build_dispatch_record` wrapper carries no mission/phase
        // (it's the legacy/test call shape). A real phase-bound dispatch goes
        // through `_with_payload` with the resolved mission/phase (#714); the
        // viewer joins via session_id either way.
        assert!(rec.phase_id.is_none());
        // ts is set to a non-empty UTC datetime string.
        assert!(!rec.ts.is_empty());
        assert!(rec.ts.ends_with('Z'), "ts should be UTC: {}", rec.ts);
    }

    #[test]
    fn dispatch_record_omits_model_when_none() {
        // None model => field is absent from serialized JSON entirely
        // (per `skip_serializing_if = "Option::is_none"`). Old viewers
        // tolerate the absent field; new viewers render "model: unknown"
        // or similar.
        let rec = build_dispatch_record(
            darkmux_flow::Level::Info,
            "dispatch start",
            "coder",
            "session-no-model",
            None,
        );
        assert!(rec.model.is_none());
        let json = serde_json::to_string(&rec).unwrap();
        assert!(
            !json.contains("\"model\""),
            "absent field should serialize away: {json}"
        );
    }

    #[test]
    fn dispatch_record_with_payload_stamps_mission_and_phase() {
        // (#714) A phase-bound dispatch threads its mission/phase onto
        // every flow record so the observability view groups the dispatch
        // under its mission. The viewer keys the mission crumb/panel on
        // `mission_id`; without this the records were ungrouped.
        let rec = build_dispatch_record_with_payload(
            darkmux_flow::Level::Info,
            "dispatch start",
            "coder",
            "crew-dispatch-coder-99-internal",
            Some("darkmux:qwen3.6"),
            Some("pre-1.0-compat-sweep"),
            Some("s694-profiles-schema"),
            None,
        );
        assert_eq!(rec.mission_id.as_deref(), Some("pre-1.0-compat-sweep"));
        assert_eq!(rec.phase_id.as_deref(), Some("s694-profiles-schema"));
    }

    #[test]
    fn dispatch_record_with_payload_omits_mission_when_not_phase_bound() {
        // A one-off dispatch (no --phase-id) carries neither field — they
        // serialize away (skip_serializing_if), so old viewers and the
        // ungrouped-session rendering are untouched.
        let rec = build_dispatch_record_with_payload(
            darkmux_flow::Level::Info,
            "dispatch start",
            "coder",
            "crew-dispatch-coder-99-internal",
            Some("darkmux:qwen3.6"),
            None,
            None,
            None,
        );
        assert!(rec.mission_id.is_none());
        assert!(rec.phase_id.is_none());
        let json = serde_json::to_string(&rec).unwrap();
        assert!(
            !json.contains("mission_id") && !json.contains("phase_id"),
            "absent mission/phase should serialize away: {json}"
        );
    }

    #[test]
    fn telemetry_record_with_payload_stamps_mission_and_phase() {
        // Telemetry siblings (runtime turns, CPU samples, detector events)
        // group under the mission too — same wire as the work records.
        let rec = build_telemetry_record(
            darkmux_flow::Level::Info,
            "telemetry.runtime",
            "runtime",
            "coder",
            "sess-1",
            Some("darkmux:qwen3.6"),
            Some("pre-1.0-compat-sweep"),
            Some("s694-profiles-schema"),
            serde_json::json!({ "turns": 9 }),
        );
        assert_eq!(rec.mission_id.as_deref(), Some("pre-1.0-compat-sweep"));
        assert_eq!(rec.phase_id.as_deref(), Some("s694-profiles-schema"));
    }

    #[test]
    fn dispatch_record_error_level_serializes_distinctly() {
        // Error-level records render differently in the viewer (red tag,
        // not green). Lock the error level on dispatch_error so the
        // failure path is visually distinct from completion.
        let ok = build_dispatch_record(
            darkmux_flow::Level::Info,
            "dispatch complete",
            "coder",
            "session-abc",
            Some("darkmux:foo"),
        );
        let err = build_dispatch_record(
            darkmux_flow::Level::Error,
            "dispatch error",
            "coder",
            "session-abc",
            Some("darkmux:foo"),
        );
        assert!(matches!(ok.level, darkmux_flow::Level::Info));
        assert!(matches!(err.level, darkmux_flow::Level::Error));
        // Same session_id so the viewer pairs them — this is the contract
        // that makes computeDispatchDurations() work for the failure path
        // too (an erroring dispatch still has a wall-clock arc).
        assert_eq!(ok.session_id, err.session_id);
    }
}
