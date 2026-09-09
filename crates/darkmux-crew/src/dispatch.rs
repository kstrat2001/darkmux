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

use anyhow::{bail, Context, Result};
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
/// live. Defaults to `<darkmux root>/acks/`. The `DARKMUX_ACK_DIR` env var
/// overrides — used by tests, also available for operators who want to
/// keep the acks in a different location.
///
/// (#2450) The fallback default is derived from the SAME root resolution
/// every other darkmux directory resolves through —
/// `darkmux_types::paths::resolve(Auto)`, which honors `DARKMUX_HOME` and a
/// project-local `./.darkmux` before `~/.darkmux` — mirroring
/// `config_access::fleet_file_default`/`flows_dir_default`. Before this fix,
/// this went straight to `dirs::home_dir()`, so a `DARKMUX_HOME`-scoped
/// install with no `DARKMUX_ACK_DIR` override still wrote licensed-adjacent
/// acknowledgment files into the operator's REAL `~/.darkmux/acks`, the same
/// bug class fixed elsewhere for #1585, #2093, #2363, and `fleet_file`
/// (#2450) itself. Probed and confirmed broken (not assumed from shape)
/// before this fix.
fn ack_dir() -> Result<PathBuf> {
    // env(DARKMUX_ACK_DIR) > config.dirs.ack > <darkmux root>/acks (#661 Slice 3).
    if let Some(p) = darkmux_types::config_access::ack_dir_override() {
        return Ok(p);
    }
    Ok(ack_dir_default())
}

/// Test builds must never default onto the operator's real
/// `~/.darkmux/acks` — same isolation discipline as
/// `config_access::lab_dir_default`'s own test-build variant (#994). This
/// accessor genuinely WRITES operator-visible files (the ack marker), unlike
/// `cache_dir`/`runtime_cache_dir` (read-mostly internal caches deliberately
/// left without this guard, see their doc). A test that DID isolate itself
/// (a `DARKMUX_HOME` tempdir, or a project-local `./.darkmux`) is honored
/// verbatim, because a test that isolated itself means it.
#[cfg(any(test, feature = "test-support"))]
fn ack_dir_default() -> PathBuf {
    let resolved = darkmux_types::paths::resolve(darkmux_types::paths::ResolveScope::Auto);
    let real_user_root = dirs::home_dir().map(|h| h.join(".darkmux"));
    if real_user_root.as_ref() == Some(&resolved.root) {
        return PathBuf::from("/tmp/darkmux-test-isolated/acks");
    }
    resolved.root.join("acks")
}

#[cfg(not(any(test, feature = "test-support")))]
fn ack_dir_default() -> PathBuf {
    darkmux_types::paths::resolve(darkmux_types::paths::ResolveScope::Auto).root.join("acks")
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

/// The operator-facing refusal text shared by BOTH gate variants — the
/// prompting one's non-TTY bail and [`licensed_adjacent_ack_status`]'s
/// unconditional one. One string, so the remediation an operator reads is
/// the same wording whichever path refused, and so a future edit cannot
/// improve one and leave the other behind.
fn unacked_error(role_id: &str, dir: &std::path::Path, ack_path: &std::path::Path) -> String {
    format!(
        "licensed-adjacent role `{role_id}` requires operator acknowledgment, \
         and none has been recorded. To acknowledge, run `darkmux dispatch {role_id} \
         \"<your message>\"` from an interactive terminal and type ACKNOWLEDGE at the \
         prompt, or pre-acknowledge without the prompt:\n\
         \n  mkdir -p {} && touch {}\n\
         \nThen re-run.",
        dir.display(),
        ack_path.display()
    )
}

/// (#1511) The CHECK-ONLY licensed-adjacent gate: same decision as
/// [`require_licensed_adjacent_ack`], with the interactive prompt removed.
/// A role outside the list passes; a role with a recorded ack passes;
/// anything else prints the disclosure banner and refuses with the same
/// remediation text the prompting variant's non-TTY arm uses.
///
/// **Why a second variant exists.** `require_licensed_adjacent_ack` calls
/// `stdin.lock().read_line(…)` with no timeout whenever stdin is a TTY, and
/// the scheduler's consent filter (`scheduler::run_step_graph`) runs on the
/// scheduler's MAIN thread, sequentially, over EVERY ready step of EVERY
/// wave, before any of that wave's jobs are built.
///
/// Note what the argument is NOT: "blocking the main thread here would
/// stall the wave" does not by itself distinguish this path, because the
/// operator sign-off gate (`crate::gate::resolve_gate`) already blocks on
/// that same thread, in the loop immediately above this one, and its own
/// comment says so. The distinction that actually holds is WHO OPTS IN.
/// `resolve_gate` returns immediately unless the step declares
/// `gate: Some(…)` — a per-step opt-in the operator wrote into the mission
/// config, so a launch that blocks is a launch that asked to. A consent
/// prompt has no such opt-in: it would sit on the path of every ready step
/// of every wave, in every graph, including a mission launched detached
/// with an inherited TTY, which would then hang forever while eating
/// keystrokes from the parent shell. So the scheduler refuses instead of
/// asking.
///
/// The place to ACQUIRE an ack is unchanged and still interactive: the
/// `dispatch` CLI verb's pre-flight (`dispatch_as_crew_of_one_with`), which
/// runs before `run_step_graph` is ever entered.
pub(crate) fn licensed_adjacent_ack_status(role_id: &str) -> Result<()> {
    if !LICENSED_ADJACENT_ROLES.contains(&role_id) {
        return Ok(());
    }
    let ack_path = ack_file_for(role_id)?;
    if ack_path.exists() {
        return Ok(());
    }
    print_licensed_adjacent_banner(role_id);
    bail!(unacked_error(role_id, &ack_dir()?, &ack_path));
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
///
/// **This variant PROMPTS, and a prompt can block forever.** It reads
/// `stdin` with no timeout, so it belongs only on a path that owns the
/// terminal and has nothing waiting behind it — the `dispatch` CLI verb's
/// pre-flight (`dispatch_as_crew_of_one_with`, which runs before any graph
/// starts) and `dispatch_internal`'s own per-dispatch check (already on the
/// dispatching worker thread). Anything running on the scheduler's MAIN
/// thread must use [`licensed_adjacent_ack_status`] instead — see that
/// function's doc for why (#1511).
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
        bail!(unacked_error(role_id, &ack_dir()?, &ack_path));
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

#[derive(Debug)]
pub struct DispatchOpts {
    pub role_id: String,
    pub message: String,
    /// (#2295) The darkmux records whose stored content was appended to
    /// `message` by `dispatch --finding <key>` / `dispatch --mod <key>`.
    /// Recorded on the `dispatch start` flow record so a reader can tell WHICH
    /// records a dispatch was briefed on — the brief itself is capped in the
    /// record, and a key is the address its store answers to. A `mod` ref also
    /// decides which attachment directories the container gets mounted.
    /// Empty on every other path.
    pub brief_refs: Vec<crate::brief_refs::BriefRef>,
    pub session_id: Option<String>,
    /// (#2480) Despite the name, this bounds ONLY the tool-less single-call
    /// paths: `dispatch_remote`'s `curl -m <n>` on a hosted-endpoint call,
    /// and `dispatch_local_single_shot`'s equivalent (the RADIO answering
    /// seat). The container-agentic path (`dispatch_internal::dispatch`,
    /// what `darkmux dispatch <role>` runs by default) never reads this
    /// field — that path has no single blocking call to bound; it has a
    /// multi-turn agent loop, bounded instead by the inactivity budget
    /// (`config.runtime.inactivity_timeout_seconds` /
    /// `DARKMUX_INACTIVITY_TIMEOUT_SECONDS`). See `timeout_override_seconds`
    /// below for the container path's per-invocation knob.
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
    /// (#1483) The mission-graph STEP id this dispatch runs as, when it is
    /// one step of a Task/Step graph. When set, the live trajectory tailer
    /// stamps `step_id` into every per-turn / per-tool / per-token flow
    /// record's `payload`, so the mission-graph viewer can attribute the
    /// live turn+tool+token climb to the seat card even when the dispatch's
    /// `session_id` is NOT the `step-<id>` default. The coder-phase
    /// `mission.coder` seat dispatches under a shared `mission-run-<…>`
    /// session (see `session_id::mission_run`), so its live records were
    /// previously unattributable and the agentic seat never ticked turns.
    /// `None` for a one-off `dispatch` that isn't a graph step; such records
    /// attribute via `session_id` alone, exactly as before.
    pub step_id: Option<String>,
    /// (#1698 Packet B2) Use this text as the system prompt VERBATIM instead
    /// of resolving one via the role's manifest/loader —
    /// [`dispatch_local_single_shot`]'s only caller today (the RADIO
    /// answering seat, `src/radio_answer.rs`) needs its persona text
    /// assembled per-call (a `{{humor}}` placeholder in the role's own
    /// `.md` template, substituted from `radio.humor` config at assembly
    /// time) — something the role-manifest loader has no hook for. `Some`
    /// also SKIPS the specialist preamble
    /// (`load_autonomous_dispatch_preamble`) `dispatch_local_single_shot`
    /// would otherwise prepend for a `role_family: specialist` role: an
    /// override caller is handing over the exact, complete system prompt it
    /// wants sent, and the preamble's turn-cap/agent-loop guidance is noise
    /// (or actively misleading) for a tool-less single-exchange dispatch.
    /// `None` (every existing caller) preserves today's loader-resolved
    /// behavior exactly.
    pub system_prompt_override: Option<String>,
    /// (#1959 packet 2) Mount `/workspace` read-only (`-v <ws>:/workspace:ro`)
    /// instead of the default read-write bind. The crawler role reads a
    /// workspace tree it must never modify — a role holding only `read`/`exec`/
    /// `create_finding` (no `edit`/`write`) is already tool-gated against
    /// writing, but the mount itself is the second, filesystem-level layer:
    /// a `write`/`edit` grant added to the role later, or a shell escape via
    /// `exec`, still can't touch the tree when the mount itself refuses
    /// writes. `false` (the default) preserves every existing caller's
    /// read-write workspace exactly.
    pub workspace_read_only: bool,
    /// (#1959 flow-record vocabulary retirement) Provenance the runtime
    /// itself has no concept of — merged under `payload.context` on EVERY
    /// record this dispatch's flow-record surface emits: the `dispatch
    /// start`/`dispatch complete`/`dispatch error` bookends and every
    /// live per-event record the host tailer produces (`dispatch.tool`,
    /// `dispatch.turn`, `telemetry.*`, …). Deliberately NOT scoped to one
    /// action — the hook layer stays event-agnostic; an external tracker
    /// that wants "an accepted `create_finding` call" subscribes via a
    /// `HookMatch` payload predicate (`"payload.tool_name":
    /// "create_finding", "payload.ok": true`) against the ordinary
    /// `dispatch.tool` record instead of a bespoke finding action. The
    /// crawl launcher is the one caller that sets this today (`workspace,
    /// source, sha, rule, unit`, per unit); every other caller passes
    /// `None`, which is a complete no-op — no `context` key appears at
    /// all. Must be a JSON object when `Some` (a non-object is ignored
    /// rather than corrupting the payload shape — see
    /// `dispatch_internal::merge_record_context`'s own doc).
    pub record_context: Option<serde_json::Value>,
    /// (#2114 follow-up) A PRIOR dispatch's host out dir (the
    /// `/darkmux-out` mount, `$TMPDIR/darkmux-out-<role>-<unix_micros>`)
    /// to resume from. `Some(dir)` is the trigger `resume_checkpoint`
    /// itself never was: `dispatch_internal::dispatch` verifies
    /// `<dir>/checkpoint.json` exists and parses — since #2162 that check
    /// runs before model selection and before the workspace/host-out dirs
    /// exist, so a refused resume costs no model load, no eviction, no
    /// directory materialization and no `dispatch.start` flow record
    /// (NOT "before anything else": the licensed-adjacent ack gate, the
    /// remote-endpoint early return, and the Docker preflight still run
    /// ahead of it) — then, once this dispatch's own fresh host out dir
    /// exists, WRITES the already-validated checkpoint bytes into it — the
    /// old dir is left untouched as evidence, this dispatch gets its own
    /// trajectory/run record — then sets
    /// `DockerRunConfig::resume_checkpoint = true` so `--resume` reaches
    /// the container. `None` (every existing caller) preserves the
    /// fresh-start behavior exactly. A remote-endpoint dispatch never
    /// reaches the gate at all and ignores `resume_from` outright — a known
    /// bypass, filed as #2561. See `dispatch_internal`'s
    /// `validate_resume_checkpoint` (the early gate) and
    /// `write_staged_resume_checkpoint` (the later write) for the
    /// validate-then-stage mechanics and the `resumed_from` provenance
    /// this stamps into the dispatch's flow records.
    pub resume_from: Option<PathBuf>,
    /// (#2153) Caller-named host out dir (the `/darkmux-out` mount) to use
    /// for THIS dispatch, instead of letting `dispatch_internal::dispatch`
    /// mint its own fresh tempdir. `Some(dir)` is for a caller that needs
    /// to know the out dir BEFORE the dispatch returns — the crawl
    /// launcher is the one caller that sets this today: it mints
    /// `<mission run dir>/units/<rule>/<unit-id>/out` (#2360 — rule-
    /// namespaced, since a per-rule plan numbers its own units from
    /// `u-0001` and two rules routinely grow the same unit id into one
    /// mission) and records it into a
    /// PROVISIONAL per-unit row before dispatching, so an interrupted or
    /// hard-crashed dispatch (which returns an `Err` with no
    /// `DispatchResult::out_dir` to read back) still leaves a resumable
    /// `out_dir` on record — a fresh tempdir's path is otherwise only ever
    /// learned from a SUCCESSFUL `DispatchResult`, which an interrupted
    /// dispatch never produces. `dispatch_internal::dispatch` creates the
    /// named dir with `create_dir` (never `create_dir_all` — a
    /// caller-named path's parent must already exist) and `0o700`
    /// permissions, and REFUSES with a named error if the dir already
    /// exists rather than reusing it (closes the symlink/TOCTOU race a
    /// pre-named, pre-existing path would otherwise open — #2158). `None`
    /// (every existing caller) preserves the fresh-tempdir behavior
    /// exactly.
    pub host_out: Option<PathBuf>,
    /// (#2193) A per-dispatch `max_turns` ceiling the CALLER derived on its
    /// own (e.g. the crawl launcher's per-unit ceiling, derived from the
    /// unit's own plan estimate) — NOT the same thing as an operator's
    /// explicit `runtime.max_turns` config/env setting, which always wins
    /// over this. `dispatch_internal::dispatch` applies this ONLY when
    /// `darkmux_types::config_access::max_turns_with_source()` resolves to
    /// `Source::BuiltIn` (the operator hasn't set one); when the operator
    /// HAS set one (config or env), this field is ignored and the resolved
    /// bounds block's `source` stays `"config"`/`"env"` — see
    /// `resolved_max_turns_block` in `dispatch_internal.rs`. `None` (every
    /// caller but the crawl launcher) preserves today's behavior exactly:
    /// uncapped unless the operator opted in globally.
    ///
    /// (#2480 review) **Read this together with
    /// `timeout_override_seconds` below: the two are adjacent
    /// `Option<u32>` "override" fields that resolve in OPPOSITE
    /// directions, and the difference is not visible from either name.**
    /// THIS one is a caller-derived FALLBACK — it loses to an operator's
    /// `env`/`config` setting. That one is direct operator input at the
    /// point of dispatch — it BEATS `env`/`config`. The split is
    /// deliberate (who typed the number decides who wins), but a reader
    /// who assumes one from the other will be wrong half the time.
    pub max_turns_override: Option<u32>,
    /// (#2480) `darkmux dispatch --timeout <n>`'s per-invocation override of
    /// the CONTAINER path's inactivity budget — the piece that was missing
    /// entirely before this field existed: `--timeout` was documented as
    /// "Timeout in seconds (default: 600)" but the container-agentic
    /// dispatch (`dispatch_internal::dispatch`, the default path for
    /// `darkmux dispatch <role>`) never read it at all, silently.
    ///
    /// (#2480 review) **Deliberately the OPPOSITE precedence from its
    /// neighbor `max_turns_override` above — two adjacent `Option<u32>`
    /// "override" fields that resolve in opposite directions.** That field
    /// is a caller-DERIVED fallback (the crawl launcher's
    /// own estimate) that only fills a gap the operator left open, so an
    /// explicit `env`/`config` setting always beats it. This field is
    /// direct operator input typed at the point of dispatch — more
    /// specific than a standing env var or config file, the same way a CLI
    /// flag outranks a config default everywhere else in darkmux
    /// (`--profile` over `default_profile`, etc.) — so when `Some`, it wins
    /// outright over `env(DARKMUX_INACTIVITY_TIMEOUT_SECONDS)` and
    /// `config.runtime.inactivity_timeout_seconds` for THIS dispatch only;
    /// it never mutates the standing config. See
    /// `dispatch_internal::effective_inactivity_timeout_seconds`. `None`
    /// (every caller but the CLI's `dispatch` verb) preserves the existing
    /// `env > config > 600` resolution exactly.
    pub timeout_override_seconds: Option<u32>,
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
/// hardcoded default for that knob — EXCEPT `compactor_model` (#2571):
/// omitting `--compactor-model` no longer falls back to a runtime default.
/// It disables compaction outright for this dispatch. See that field's own
/// doc.
#[derive(Debug, Clone, Default)]
pub struct CompactionDispatchArgs {
    /// Absolute trigger. Set from `profile.runtime.compaction.threshold_tokens`
    /// (typed v0.1 field, #357).
    pub threshold_tokens: Option<u32>,
    /// Compactor model override. `None` by default, overlaid from the
    /// machine's bound `internal.utility` model via `apply_utility_model`
    /// below. **Still `None` after that overlay ⇒ no compactor at all for
    /// this dispatch (#2571)** — the runtime no longer has a hardcoded
    /// fallback to fall back to; omitting `--compactor-model` disables
    /// compaction outright rather than addressing an identifier nothing
    /// loaded. See [`Self::unset_compactor_warning`].
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
        // no legacy-shape `extras["model"]` fallback. There is no
        // `compaction.compactor_model` field on the profile schema, so
        // this always starts `None` here; `apply_utility_model` (called
        // by dispatchers that have the machine registry) overlays the
        // bound `internal.utility` model afterward. If THAT is also
        // unset, `compactor_model` stays `None` all the way through —
        // and since #2571 that means no compactor for this dispatch,
        // not a silent fallback to the runtime's old hardcoded default.
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
    /// compile), decoupled from the profile. `None` (no binding) ⇒ left
    /// untouched, which since #2571 means `compactor_model` stays `None` —
    /// no compactor for this dispatch, not a fallback to a runtime default
    /// (the runtime no longer has one). See [`Self::unset_compactor_warning`]
    /// for the operator-facing disclosure this leaves for the caller to
    /// surface.
    pub fn apply_utility_model(&mut self, utility_model_id: Option<&str>) {
        if self.compactor_model.is_none() {
            self.compactor_model = utility_model_id.map(str::to_string);
        }
    }

    /// (MUST FIX 1, #2571 follow-up) The host-side twin of the runtime's
    /// own `compactor_disclosure_message`: fires under the identical
    /// condition (`compactor_model` still `None` after every overlay, but a
    /// real compaction trigger IS configured, so compaction would otherwise
    /// have fired) at the point the host is about to apply — or, in this
    /// case, skip applying — the compaction flags to the dispatch.
    ///
    /// (Second review round) The original version gated on `context_window`
    /// alone, which left the absolute-threshold-only mode silent — the
    /// compaction trigger also fires on `threshold_tokens` with no window
    /// at all, and `CompactionDispatchArgs::from_profile` reads that field
    /// independently of `context_window` (which comes from the primary
    /// model's `n_ctx` and is absent whenever the primary is
    /// endpoint-bearing). A dispatch configured threshold-only with no
    /// compactor is exactly the long dispatch that would have compacted and
    /// now silently doesn't, reachable from the host the same way it's
    /// reachable from the runtime directly. Fires on EITHER trigger now;
    /// `None` only when a compactor is configured, or when neither trigger
    /// is set (nothing was ever going to compact either way).
    pub fn unset_compactor_warning(&self) -> Option<String> {
        if self.compactor_model.is_some() {
            return None;
        }
        match (self.context_window, self.threshold_tokens) {
            (None, None) => None,
            (Some(window), _) => Some(format!(
                "darkmux dispatch: no compactor is bound for this dispatch (`internal.utility` \
                 is unset, and no `profile.runtime.compaction` compactor was pinned) — \
                 compaction is OFF. The primary model's context window is {window} tokens; a \
                 long-running dispatch will grow its transcript against that window with only \
                 the runtime's built-in trim between it and overflow. (#2571)"
            )),
            (None, Some(threshold)) => Some(format!(
                "darkmux dispatch: no compactor is bound for this dispatch (`internal.utility` \
                 is unset, and no `profile.runtime.compaction` compactor was pinned) — \
                 compaction is OFF. This dispatch has an absolute compaction threshold of \
                 {threshold} tokens configured (no context window is known); a long-running \
                 dispatch will grow its transcript toward that count with only the runtime's \
                 built-in trim between it and overflow. (#2571)"
            )),
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
/// Defaults to `<darkmux root>/identity.md`. The `DARKMUX_IDENTITY_PATH`
/// env var overrides — used by tests, also available for operators
/// with multi-user / multi-identity setups.
///
/// (#2450) The fallback routes through `paths::resolve` rather than straight
/// at `dirs::home_dir()`. This one is a READ, not a write, which makes it the
/// most privacy-bearing member of the class rather than the least: the file's
/// CONTENT is injected into the dispatch's system prompt, so before this fix a
/// `DARKMUX_HOME`-scoped install (a sandbox, a CI run, a second persona) read
/// the operator's REAL `~/.darkmux/identity.md` and sent it to the model.
/// Probed and confirmed broken before this fix.
///
/// `ForceUser`, deliberately — the operator's identity is user-global, and
/// `Auto` would silently switch personas based on which directory the process
/// happened to be standing in. `ForceUser` still honors `DARKMUX_HOME` (that
/// branch short-circuits ahead of the scope match), which is the bug fixed.
fn identity_path() -> Option<PathBuf> {
    // env(DARKMUX_IDENTITY_PATH) > config.dirs.identity > <darkmux root>/identity.md
    // (#661 Slice 3). Always `Some` since #2450 — `paths::resolve` has its own
    // no-HOME fallback, so the old "no HOME and no override" None arm is gone.
    // The Option is kept because callers already treat a missing path and a
    // missing FILE identically (the identity file is optional by design).
    darkmux_types::config_access::identity_path_override().or_else(|| {
        Some(
            darkmux_types::paths::resolve(darkmux_types::paths::ResolveScope::ForceUser)
                .root
                .join("identity.md"),
        )
    })
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

/// (#1698 Packet B) Container-free single-shot dispatch entry point — see
/// `dispatch_internal::dispatch_local_single_shot`'s own doc for the full
/// contract. A caller-injectable `local_dispatch` primitive for
/// `fleet::dispatch_routed_via` (the same substitution seam #1509 built
/// for `dispatch_as_crew_of_one`), never routed across the fleet itself.
pub fn dispatch_local_single_shot(opts: DispatchOpts) -> Result<DispatchResult> {
    crate::dispatch_internal::dispatch_local_single_shot(opts)
}

/// (#1698 Packet B2 gate) True when a dispatch with this (role, profile)
/// would resolve to a remote endpoint — the data-boundary question a caller
/// that COMPOSES its own payload must answer before assembling it. Fails
/// closed (unresolvable ⇒ `true`). See
/// `dispatch_internal::dispatch_resolves_remote`'s own doc.
pub fn dispatch_resolves_remote(
    role_id: &str,
    profile_name: Option<&str>,
    config_path: Option<&str>,
) -> bool {
    crate::dispatch_internal::dispatch_resolves_remote(role_id, profile_name, config_path)
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

    // ─── MUST FIX 1 (#2571 follow-up): host-side unset-compactor warning ──

    #[test]
    fn unset_compactor_warning_fires_when_no_compactor_and_a_window_is_set() {
        let c = CompactionDispatchArgs {
            context_window: Some(101_000),
            ..Default::default()
        };
        let msg = c
            .unset_compactor_warning()
            .expect("no compactor + a real context window must warn");
        assert!(msg.contains("101000"), "warning must name the window: {msg}");
        assert!(
            msg.to_ascii_lowercase().contains("compaction is off"),
            "warning must say plainly that compaction is off: {msg}"
        );
    }

    #[test]
    fn unset_compactor_warning_silent_when_a_compactor_is_bound() {
        let mut c = CompactionDispatchArgs {
            context_window: Some(101_000),
            ..Default::default()
        };
        c.apply_utility_model(Some("darkmux:util-4b"));
        assert_eq!(c.unset_compactor_warning(), None);
    }

    #[test]
    fn unset_compactor_warning_silent_when_no_context_window() {
        let c = CompactionDispatchArgs::default();
        assert_eq!(c.unset_compactor_warning(), None);
    }

    /// (Second review round) The threshold-only reachable case: an
    /// endpoint-bearing primary (or any profile with no declared `n_ctx`)
    /// leaves `context_window` `None`, but `threshold_tokens` is read
    /// independently from `profile.runtime.compaction.threshold_tokens` —
    /// a real, reachable compaction trigger that the pre-fix version of
    /// this function stayed silent about.
    #[test]
    fn unset_compactor_warning_fires_when_no_compactor_and_only_a_threshold_is_set() {
        let c = CompactionDispatchArgs {
            threshold_tokens: Some(30_000),
            ..Default::default()
        };
        let msg = c
            .unset_compactor_warning()
            .expect("no compactor + an absolute threshold must warn, even with no window");
        assert!(msg.contains("30000"), "warning must name the threshold: {msg}");
        assert!(
            msg.to_ascii_lowercase().contains("compaction is off"),
            "warning must say plainly that compaction is off: {msg}"
        );
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

    /// (#1918) `dispatch_internal::dispatch` composes THIS SAME resolved
    /// mission id into its own `session_id` (right beside the resolution
    /// tested above), closing the collision for the one producer that
    /// streams its own records directly (`dispatch start`/`dispatch.turn`/
    /// `dispatch.tool`/`dispatch complete`/telemetry — bypassing
    /// `StepOutcome.flow_records`/`StepRunCtx::emit`, both of which the
    /// launcher's own `emit`-wrap already scopes). A full end-to-end proof
    /// of `dispatch()`'s own wiring needs a real container
    /// (`mock_dispatch_proof.rs`'s tests are `#[ignore]`d for exactly that
    /// reason); this proves the COMPOSITION `dispatch()` applies at that
    /// resolution point is the one two missions running the SAME config
    /// (the `session_id::step` default, unset by config) need: distinct
    /// per-mission session ids for the identical, config-derived default.
    #[test]
    #[serial_test::serial]
    fn dispatch_internal_composes_the_resolved_mission_into_a_config_derived_session_id() {
        let tmp = TempDir::new().unwrap();
        let prev = std::env::var("DARKMUX_CREW_DIR").ok();
        unsafe {
            std::env::set_var("DARKMUX_CREW_DIR", tmp.path());
        }
        let phases_dir = tmp.path().join("missions").join("mission-a").join("phases");
        fs::create_dir_all(&phases_dir).unwrap();
        fs::write(
            phases_dir.join("p1.json"),
            r#"{"id":"p1","mission_id":"mission-a","description":"d","status":"planned","depends_on":[],"created_ts":0}"#,
        )
        .unwrap();
        let other_phases_dir = tmp.path().join("missions").join("mission-b").join("phases");
        fs::create_dir_all(&other_phases_dir).unwrap();
        fs::write(
            other_phases_dir.join("p1.json"),
            r#"{"id":"p1","mission_id":"mission-b","description":"d","status":"planned","depends_on":[],"created_ts":0}"#,
        )
        .unwrap();

        // The SAME `session_id::step` default a `dispatch.internal` step
        // with no explicit `config.session_id` falls back to — literally
        // out of the mission config document, byte-identical whichever
        // mission runs it.
        let raw_session_id = darkmux_types::session_id::step("s1");

        // Two DIFFERENT phases (as two launches of the same config would
        // each mint their own phase under their own mission), same phase
        // id `p1`, same step id `s1` — the actual #1918 collision shape.
        let mission_a = resolve_mission_for_phase(Some("p1"));
        // Force the second resolution to hit the OTHER mission's phase by
        // pointing `DARKMUX_CREW_DIR`'s layout differently is awkward here
        // (both phases share the literal id `p1`, one per mission dir) —
        // resolve each directly against its own known mission_id instead,
        // proving the composition, not the phase-lookup collision (a
        // SEPARATE, real limitation: two phases sharing a literal id
        // across missions is exactly why #1918 exists at the session_id
        // layer in the first place, and `load_phase_by_id` itself resolves
        // by id, not by mission — out of scope for this test).
        assert_eq!(mission_a.as_deref(), Some("mission-a"));

        let scoped_a = match &mission_a {
            Some(mid) => darkmux_types::session_id::scope_to_run(&raw_session_id, mid),
            None => raw_session_id.clone(),
        };
        let scoped_b = darkmux_types::session_id::scope_to_run(&raw_session_id, "mission-b");

        assert_eq!(scoped_a, "step-s1-mission-a");
        assert_eq!(scoped_b, "step-s1-mission-b");
        assert_ne!(
            scoped_a, scoped_b,
            "two missions running the identical config-derived session_id default must diverge \
             once scoped to their own mission — the #1918 collision surface `dispatch()` itself closes"
        );

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

    /// (#1511) The CHECK-ONLY variant the scheduler uses. It refuses
    /// without a recorded ack and passes with one, and — the property that
    /// matters — it reaches neither branch of the prompting variant's TTY
    /// test, so it can never block on `stdin`. The scheduler runs this
    /// sequentially on its main thread ahead of every job in a wave; a
    /// prompt there would stop the whole wave, and a mission launched
    /// detached with an inherited TTY would hang indefinitely.
    #[test]
    #[serial_test::serial]
    fn the_check_only_gate_refuses_without_an_ack_and_passes_with_one() {
        let tmp = TempDir::new().unwrap();
        let prev = std::env::var("DARKMUX_ACK_DIR").ok();
        // Safety: serialized.
        unsafe {
            std::env::set_var("DARKMUX_ACK_DIR", tmp.path());
        }

        // Unlisted role: no-op, same as the prompting variant.
        licensed_adjacent_ack_status("coder").unwrap();

        // Listed, unacked: refuses, with the operator-facing remediation.
        let err = licensed_adjacent_ack_status("health-research").unwrap_err();
        let s = format!("{err:#}");
        assert!(s.contains("requires operator acknowledgment"), "got: {s}");
        assert!(s.contains("mkdir -p"), "got: {s}");

        // The ack file's mere presence is the consent record (the
        // operator-sovereign escape hatch), and it lets the role through.
        fs::write(tmp.path().join("health-research.ack"), "acknowledged_at_unix_seconds=1\n").unwrap();
        licensed_adjacent_ack_status("health-research").unwrap();

        unsafe {
            match prev {
                Some(v) => std::env::set_var("DARKMUX_ACK_DIR", v),
                None => std::env::remove_var("DARKMUX_ACK_DIR"),
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

    /// (#2450) `ack_dir()`'s built-in default must scope under `DARKMUX_HOME`,
    /// the same bug class already fixed for `fleet_file`/`flows_dir`/
    /// `hooks_outbox_dir`/`lab_dir` in `darkmux-types::config_access`. Probed
    /// directly (not assumed from shape) before this fix: with no
    /// `DARKMUX_ACK_DIR` set and `DARKMUX_HOME` pointed at a throwaway root,
    /// `ack_dir()` still resolved to the operator's REAL
    /// `~/.darkmux/acks` — confirmed via a temporary probe test, since
    /// `ack_dir()` writes real acknowledgment files on the operator's behalf.
    #[test]
    #[serial_test::serial]
    fn ack_dir_honors_darkmux_home() {
        let tmp = TempDir::new().unwrap();
        let prev_home = std::env::var("DARKMUX_HOME").ok();
        let prev_ack = std::env::var("DARKMUX_ACK_DIR").ok();
        unsafe {
            std::env::remove_var("DARKMUX_ACK_DIR");
            std::env::set_var("DARKMUX_HOME", tmp.path());
        }
        let dir = ack_dir().unwrap();
        unsafe {
            match prev_home {
                Some(v) => std::env::set_var("DARKMUX_HOME", v),
                None => std::env::remove_var("DARKMUX_HOME"),
            }
            match prev_ack {
                Some(v) => std::env::set_var("DARKMUX_ACK_DIR", v),
                None => std::env::remove_var("DARKMUX_ACK_DIR"),
            }
        }
        assert_eq!(
            dir,
            tmp.path().join("acks"),
            "must scope under DARKMUX_HOME, not the real user home"
        );
    }

    /// (#2450) `identity_path()`'s built-in default must scope under
    /// `DARKMUX_HOME`. Probed before the fix and confirmed broken: with
    /// `DARKMUX_HOME` pointed at a throwaway root and no
    /// `DARKMUX_IDENTITY_PATH` override, it still resolved to the operator's
    /// REAL `~/.darkmux/identity.md` — whose CONTENT this module injects into
    /// the dispatch system prompt, so the leak was of the operator's own
    /// identity text into a scoped install's model calls.
    #[test]
    #[serial_test::serial]
    fn identity_path_honors_darkmux_home() {
        let tmp = TempDir::new().unwrap();
        let prev_home = std::env::var("DARKMUX_HOME").ok();
        let prev_id = std::env::var("DARKMUX_IDENTITY_PATH").ok();
        unsafe {
            std::env::remove_var("DARKMUX_IDENTITY_PATH");
            std::env::set_var("DARKMUX_HOME", tmp.path());
        }
        let got = identity_path();
        unsafe {
            match prev_home {
                Some(v) => std::env::set_var("DARKMUX_HOME", v),
                None => std::env::remove_var("DARKMUX_HOME"),
            }
            match prev_id {
                Some(v) => std::env::set_var("DARKMUX_IDENTITY_PATH", v),
                None => std::env::remove_var("DARKMUX_IDENTITY_PATH"),
            }
        }
        assert_eq!(
            got,
            Some(tmp.path().join("identity.md")),
            "must scope under DARKMUX_HOME, not the real user home"
        );
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

#[cfg(test)]
mod action_vocabulary_conformance {
    //! (#1852) The conformance test Contract 2 never had.
    //!
    //! Every other test on this seam asserts a hand-written literal on its own
    //! side: the producer test checks the string its author typed, the consumer
    //! test builds a fixture with the string ITS author typed. Both stay green
    //! forever no matter how far apart the two drift, because nothing ever
    //! takes a REAL emitted record and hands it to a REAL consumer matcher.
    //!
    //! That is the gap CLAUDE.md's contract registry names — "tests exercise
    //! the subsystem, not its alignment" — and it is why five separate
    //! consumers each had to rediscover the spelling split and patch it
    //! locally. This test closes the loop for the dispatch bookends.
    use super::*;

    /// Build through the REAL producer, match through the REAL consumer
    /// predicate. Nothing here types a bookend literal.
    fn emitted(action: &str) -> darkmux_flow::FlowRecord {
        build_dispatch_record(darkmux_flow::Level::Info, action, "coder", "sess-1", Some("m"))
    }

    #[test]
    fn a_record_this_crate_emits_is_recognized_by_the_shared_matcher() {
        let start = emitted(darkmux_flow::DISPATCH_START);
        let complete = emitted(darkmux_flow::DISPATCH_COMPLETE);
        let error = emitted(darkmux_flow::DISPATCH_ERROR);

        assert!(darkmux_flow::is_dispatch_start(&start.action), "start: {}", start.action);
        assert!(darkmux_flow::is_dispatch_complete(&complete.action), "complete: {}", complete.action);
        assert!(darkmux_flow::is_dispatch_error(&error.action), "error: {}", error.action);
        assert!(darkmux_flow::is_dispatch_terminal(&complete.action));
        assert!(darkmux_flow::is_dispatch_terminal(&error.action));
        assert!(!darkmux_flow::is_dispatch_terminal(&start.action), "a start is not a terminal");
    }

    /// The OTHER lineage. `darkmux-lab` and `runtime` emit the dotted form, so
    /// a consumer sees both shapes depending on which path ran. Pin that the
    /// matchers accept it — this is the half a literal comparison gets wrong.
    #[test]
    fn the_dotted_lineage_is_recognized_too() {
        for (dotted, ok) in [
            ("dispatch.start", darkmux_flow::is_dispatch_start as fn(&str) -> bool),
            ("dispatch.complete", darkmux_flow::is_dispatch_complete),
            ("dispatch.error", darkmux_flow::is_dispatch_error),
        ] {
            assert!(ok(dotted), "the dotted lineage must be recognized: {dotted}");
        }
    }

    /// The constants must keep their ON-DISK value. Changing them is a
    /// data-shape change: every historical record in the per-day JSONL and in
    /// Redis carries the spaced form, and a silent flip would strand all of it
    /// while every test that compares constant-to-constant stayed green.
    #[test]
    fn the_constants_still_carry_the_value_that_is_on_disk() {
        assert_eq!(darkmux_flow::DISPATCH_START, "dispatch start");
        assert_eq!(darkmux_flow::DISPATCH_COMPLETE, "dispatch complete");
        assert_eq!(darkmux_flow::DISPATCH_ERROR, "dispatch error");
    }

    /// A near-miss must NOT match. Without this the matchers could degrade to
    /// `starts_with("dispatch")` and every test above would still pass.
    #[test]
    fn unrelated_dispatch_actions_are_not_bookends() {
        for a in ["dispatch.turn", "dispatch.tool", "dispatch.reasoning", "dispatch route", "dispatch"] {
            assert!(!darkmux_flow::is_dispatch_start(a), "{a} is not a start");
            assert!(!darkmux_flow::is_dispatch_terminal(a), "{a} is not a terminal");
        }
    }
}
