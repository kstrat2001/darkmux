//! Dispatch a crew member (role) for a single turn.
//!
//! This is the operator-facing entry point that ties the crew schema
//! (`templates/builtin/roles/<id>.{json,md}`) to the actual runtime
//! (openclaw). Three responsibilities:
//!
//!   1. **Load the role** — manifest + `.md` system prompt
//!   2. **Pre-flight check** — verify the corresponding openclaw agent
//!      exists under the `darkmux/<role-id>` namespace and matches the
//!      manifest's expectations (system prompt + tool palette)
//!   3. **Dispatch** — invoke `openclaw agent darkmux/<role-id>` and return
//!      the result
//!
//! `darkmux crew sync` is the operator-explicit way to make openclaw's
//! `agents.list[]` reflect the manifests on disk — writes/updates the
//! `darkmux/<role>` entries to match what the manifests + `.md` prompts say.

use crate::crew::loader::{load_role_prompt, load_roles, load_sprints};
use crate::crew::types::Role;
use anyhow::{Context, Result, anyhow, bail};
use serde_json::{Map, Value, json};
use std::fs;
use std::io::{BufRead, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::process::Command;
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
const LICENSED_ADJACENT_ROLES: &[&str] = &[
    "health-research",
    "legal-research",
    "fitness-coach",
];

/// Default openclaw config path. `DARKMUX_OPENCLAW_CONFIG` env var overrides
/// (e.g., for tests). Visible to other crates so the doctor pin-drift
/// check (#160) reads from the same path sync writes to.
pub(crate) fn default_openclaw_config() -> PathBuf {
    if let Ok(p) = std::env::var("DARKMUX_OPENCLAW_CONFIG") {
        let trimmed = p.trim();
        if !trimmed.is_empty() {
            return PathBuf::from(trimmed);
        }
    }
    let home = dirs::home_dir().unwrap_or_else(|| PathBuf::from("."));
    home.join(".openclaw/openclaw.json")
}

/// The openclaw agent id darkmux uses for a given role. Per the
/// CLAUDE.md namespace convention: `darkmux/<role-id>`.
fn agent_id_for(role_id: &str) -> String {
    format!("darkmux/{role_id}")
}

/// The role's openclaw workspace dir, derived from the standard
/// `~/.openclaw/workspace-darkmux-<role-id>/` layout. Used as the default
/// `--watch` target when the caller doesn't supply explicit paths (#89).
pub fn default_workspace_for_role(role_id: &str) -> PathBuf {
    let home = dirs::home_dir().unwrap_or_else(|| PathBuf::from("."));
    home.join(".openclaw")
        .join(format!("workspace-darkmux-{role_id}"))
}

/// Slug for the agent's on-disk dirs. Translates the `darkmux/<role>` id
/// to a filesystem-safe nested path (`agents/darkmux/<role>/agent`) and a
/// flat workspace slug (`workspace-darkmux-<role>`).
fn agent_dirs_for(role_id: &str, openclaw_root: &Path) -> (PathBuf, PathBuf) {
    let agent_dir = openclaw_root.join("agents").join("darkmux").join(role_id).join("agent");
    let workspace = openclaw_root.join(format!("workspace-darkmux-{role_id}"));
    (agent_dir, workspace)
}

/// Resolve the directory where licensed-adjacent acknowledgment files
/// live. Defaults to `~/.darkmux/acks/`. The `DARKMUX_ACK_DIR` env var
/// overrides — used by tests, also available for operators who want to
/// keep the acks in a different location.
fn ack_dir() -> Result<PathBuf> {
    if let Ok(p) = std::env::var("DARKMUX_ACK_DIR") {
        let trimmed = p.trim();
        if !trimmed.is_empty() {
            return Ok(PathBuf::from(trimmed));
        }
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
    eprintln!(
        "This role operates in a domain regulated by professional licensure."
    );
    eprintln!(
        "It is a research / organization assistant — NOT a substitute for a"
    );
    eprintln!(
        "licensed professional. The role's full doctrine is in the .md prompt"
    );
    eprintln!(
        "at templates/builtin/roles/{role_id}.md in the darkmux source"
    );
    eprintln!(
        "(or your override at ~/.darkmux/roles/{role_id}.md if set)."
    );
    eprintln!();
    eprintln!("By acknowledging, you confirm you understand:");
    eprintln!(
        "  - The local LLM may deviate from its system prompt under adversarial"
    );
    eprintln!(
        "    or persistent prompting. The prompt IS the only runtime boundary."
    );
    eprintln!(
        "  - You are solely responsible for following jurisdiction-specific"
    );
    eprintln!(
        "    licensure rules (UPL / UPM / scope-of-practice)."
    );
    eprintln!(
        "  - Time-sensitive situations (medical emergency, served lawsuit,"
    );
    eprintln!(
        "    acute pain) go to professionals, not this tool."
    );
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
fn require_licensed_adjacent_ack(role_id: &str) -> Result<()> {
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

/// Which agent runtime services a dispatch. `Internal` is the default
/// as of the runtime-default flip: darkmux's in-house container-
/// bounded Rust runtime (see `runtime/` and `dispatch_internal.rs`).
/// Kernel-enforced workspace isolation via Docker; no external runtime
/// binary to install, no separate config to maintain.
///
/// `Openclaw` is the legacy shell-out path that's shipped since v0.1.
/// Available via the explicit `--runtime openclaw` flag for operators
/// who already use openclaw, want the runtime the article-series
/// numbers were measured against, or need workspace-permissive
/// behavior the container doesn't allow.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Runtime {
    #[default]
    Internal,
    Openclaw,
}

impl Runtime {
    /// String parse for CLI-flag plumbing (`--runtime <name>`). The
    /// queue boundary uses `serde::Deserialize` directly — a mistyped
    /// runtime on a WorkJob is rejected at JSON parse time rather than
    /// in `validate()`, which is what Wave-E.14 lifted into the type
    /// (#255 / PR-C.1 code-reviewer MEDIUM).
    pub fn parse(s: &str) -> Result<Self> {
        match s {
            "openclaw" => Ok(Runtime::Openclaw),
            "internal" => Ok(Runtime::Internal),
            other => bail!(
                "unknown runtime: {other}. \
                 Known: openclaw, internal"
            ),
        }
    }
}

#[derive(Debug)]
pub struct DispatchOpts {
    pub role_id: String,
    pub message: String,
    /// Optional delivery target in `<channel>:<target>` form
    /// (e.g. `discord:1500166601909993503`).
    pub deliver: Option<String>,
    pub session_id: Option<String>,
    pub timeout_seconds: u32,
    /// Skip the pre-flight checks. Use only when explicitly debugging.
    pub skip_preflight: bool,
    /// Paths to capture post-dispatch filesystem state for (#89 —
    /// SIGNOFF verification visibility). The dispatcher walks each
    /// path (immediate children + one level deep into subdirs;
    /// excludes openclaw state files) after the openclaw call returns
    /// and emits a stderr summary so the operator can compare the
    /// actual filesystem state against any "files written" claims in
    /// the SIGNOFF block. Empty defaults to the role's openclaw
    /// workspace dir.
    pub watch_paths: Vec<PathBuf>,
    /// Explicit working-directory override for the dispatch (#143).
    /// When `Some(path)`, the dispatcher sets up
    /// `~/.openclaw/workspace-darkmux-<role>/repo` as a symlink to
    /// the given path before invoking openclaw. The agent then sees
    /// the operator-named scope as `repo/` inside its workspace.
    /// When `None`, the dispatcher does NOT touch the workspace —
    /// whatever symlink the operator has set up (or none at all)
    /// is what the agent gets. Per the operator-sovereignty contract
    /// in #143, darkmux doesn't auto-create or auto-remove scope
    /// links; --workdir is the explicit operator opt-in.
    pub workdir: Option<PathBuf>,
    /// Optional sprint id binding this dispatch to a sprint in a
    /// mission (#146 Stage 1). When set:
    ///
    ///   1. The dispatcher loads the sprint manifest and resolves
    ///      `depends_on` parents. For each parent that has a recorded
    ///      output file (`<sprint-id>-output.txt`), the parent's
    ///      output text is prepended to the dispatch message as a
    ///      "Prior sprint outputs" context block. One-hop only —
    ///      transitive ancestors are NOT walked (Stage 1 scope).
    ///   2. After the dispatch returns, the agent's reply text is
    ///      persisted to `<sprint-id>-output.txt` alongside the sprint
    ///      manifest, so downstream sprints with this sprint in their
    ///      `depends_on` can read it on their own dispatch.
    ///
    /// When `None`, the dispatcher behaves as before — no sprint
    /// awareness, no output persistence. Backwards-compatible default.
    pub sprint_id: Option<String>,
    /// Which agent runtime to dispatch through. See [`Runtime`].
    /// Default: `Runtime::Internal` (the in-house container-bounded path).
    pub runtime: Runtime,
    /// Target machine for the dispatch (#246 PR-C.3). When `Some(<id>)`
    /// and `<id>` differs from the local `DARKMUX_MACHINE_ID`, the
    /// dispatch is published to `darkmux:work:<role-tier>` via
    /// `fleet::publish_job` instead of running locally; a worker on
    /// the target machine picks it up. When `None`, the dispatch runs
    /// locally (today's behavior; preserved for backward compat).
    ///
    /// PR-D will add implicit tier-aware routing (no `--machine` flag
    /// required); this field is the operator-explicit override that
    /// will continue to win over implicit routing per
    /// operator-sovereignty doctrine.
    pub machine: Option<String>,
    /// Whether to block on completion when the dispatch routes to a
    /// remote machine (#246 PR-C.3). `true` — the default for
    /// `crew dispatch` — tails the local flow stream for the matching
    /// `session_id`'s `dispatch.complete` record and returns the
    /// outcome (preserves today's "spawn, block, see result" CLI
    /// ergonomics). `false` returns immediately with a synthetic
    /// success result; the operator polls via `darkmux flow tail`
    /// (or PR-D's `mission dispatch --no-wait` path).
    ///
    /// Ignored when the dispatch runs locally — local dispatches are
    /// always synchronous.
    pub wait: bool,
}

/// One file's state for the watched-paths summary (#89).
#[derive(Debug, Clone)]
pub struct WatchedFile {
    pub path: PathBuf,
    pub size: u64,
}

/// Post-dispatch state of one watched path.
#[derive(Debug, Clone)]
pub struct WatchedPathState {
    pub root: PathBuf,
    pub files: Vec<WatchedFile>,
    /// True if `root` itself didn't exist or wasn't readable at snapshot
    /// time. The dispatcher reports the gap rather than silently dropping
    /// the path.
    pub unreachable: bool,
}

#[derive(Debug)]
pub struct DispatchResult {
    pub exit_code: i32,
    pub stdout: String,
    pub stderr: String,
    /// The session id actually used for this dispatch. Echoes back the
    /// caller-supplied `opts.session_id` when set, or the fresh one this
    /// dispatch generated when `opts.session_id` was `None` (closes #88 —
    /// without an explicit `--session-id`, openclaw's per-agent session
    /// reuse caused cross-task context pollution).
    pub session_id: String,
    /// Post-dispatch state of each `opts.watch_paths` entry, in the same
    /// order. Surfaces the actual filesystem so the operator can compare
    /// against the SIGNOFF block's "files written" claims (#89).
    pub watched_state: Vec<WatchedPathState>,
}

/// Process-local monotonic counter — guarantees uniqueness for rapid
/// successive `fresh_session_id` calls in the same process even when the
/// wall-clock micros component collides (loops faster than the system clock).
static SESSION_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Generate a fresh, unique session id for an unscoped `crew dispatch` call.
/// Shape: `crew-dispatch-<role>-<unix_micros>-<process_counter>`.
///
/// The micros component distinguishes calls across processes (different
/// invocations of `darkmux crew dispatch` from a shell each get their own
/// process start time). The counter component distinguishes calls within
/// the same process (scripted callers or future server backends could call
/// faster than microsecond resolution allows). Together they guarantee no
/// two `fresh_session_id` calls return the same string, closing the
/// per-agent session reuse this helper is meant to prevent (#88).
pub(crate) fn fresh_session_id(role_id: &str) -> String {
    let micros = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_micros())
        .unwrap_or(0);
    let counter = SESSION_COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("crew-dispatch-{role_id}-{micros}-{counter}")
}

/// Bounded directory walk that's resilient to non-existent paths +
/// permission errors. Returns the immediate-child + one-level-down
/// regular files under `root`, excluding openclaw state files (which
/// change on every dispatch and would drown out the operator's signal).
///
/// Symlinks are reported as files only when their target is a regular
/// file. Symlinked directories are NOT followed (would unbounded-walk
/// the live source tree the workspace symlinks into).
///
/// Cap: 200 files per `root`. The dispatcher's job is to surface the
/// signal, not to dump entire repos.
pub(crate) fn snapshot_watched_path(root: &Path) -> WatchedPathState {
    const MAX_FILES_PER_ROOT: usize = 200;

    if !root.exists() {
        return WatchedPathState {
            root: root.to_path_buf(),
            files: Vec::new(),
            unreachable: true,
        };
    }

    let mut files: Vec<WatchedFile> = Vec::new();
    walk_one_level(root, &mut files, MAX_FILES_PER_ROOT);

    if files.len() >= MAX_FILES_PER_ROOT {
        // Truncate; operator sees the cap was hit via the leading entries.
        files.truncate(MAX_FILES_PER_ROOT);
    }

    // Sort by size descending so the operator sees the largest (often
    // most-relevant — actual outputs vs scratch files) first.
    files.sort_by_key(|f| std::cmp::Reverse(f.size));

    WatchedPathState {
        root: root.to_path_buf(),
        files,
        unreachable: false,
    }
}

fn walk_one_level(dir: &Path, out: &mut Vec<WatchedFile>, cap: usize) {
    let entries = match fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return,
    };
    for entry in entries.flatten() {
        if out.len() >= cap {
            return;
        }
        let path = entry.path();
        if is_openclaw_noise(&path) {
            continue;
        }
        let meta = match entry.metadata() {
            Ok(m) => m,
            Err(_) => continue,
        };
        if meta.is_file() {
            out.push(WatchedFile {
                path,
                size: meta.len(),
            });
        } else if meta.is_dir() {
            // One level only — don't recurse into subdirs of subdirs.
            // (Distinguishes from symlinks via is_symlink check below.)
            if path.is_symlink() {
                continue;
            }
            // Walk the subdir flat (no further recursion).
            let sub_entries = match fs::read_dir(&path) {
                Ok(e) => e,
                Err(_) => continue,
            };
            for sub in sub_entries.flatten() {
                if out.len() >= cap {
                    return;
                }
                let sub_path = sub.path();
                if is_openclaw_noise(&sub_path) {
                    continue;
                }
                let sub_meta = match sub.metadata() {
                    Ok(m) => m,
                    Err(_) => continue,
                };
                if sub_meta.is_file() {
                    out.push(WatchedFile {
                        path: sub_path,
                        size: sub_meta.len(),
                    });
                }
            }
        }
    }
}

/// Files the dispatcher excludes from watched-state snapshots — openclaw's
/// own session bookkeeping, trajectory files, and the workspace bootstrap
/// markdowns. These change on every dispatch and would drown out the
/// signal the operator is actually looking for.
/// Resolve the path to the optional operator-identity file (#147).
/// Defaults to `~/.darkmux/identity.md`. The `DARKMUX_IDENTITY_PATH`
/// env var overrides — used by tests, also available for operators
/// with multi-user / multi-identity setups.
fn identity_path() -> Option<PathBuf> {
    if let Ok(p) = std::env::var("DARKMUX_IDENTITY_PATH") {
        let trimmed = p.trim();
        if !trimmed.is_empty() {
            return Some(PathBuf::from(trimmed));
        }
    }
    dirs::home_dir().map(|h| h.join(".darkmux").join("identity.md"))
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
/// section to the role prompt. Called at BOTH sync time (so the
/// systemPromptOverride written to openclaw.json reflects the
/// augmented form) AND preflight time (so drift detection compares
/// like-for-like).
pub(crate) fn augment_prompt_with_identity(role_prompt: &str) -> String {
    match load_operator_identity() {
        Some(identity) => format!(
            "{role_prompt}\n\n---\n\n## About the operator\n\n{}\n",
            identity.trim_end()
        ),
        None => role_prompt.to_string(),
    }
}

/// Sprint-output file path for a given sprint id. Lives alongside the
/// sprint manifest at `<crew_root>/sprints/<id>-output.txt`. Used by
/// #146 Stage 1 (cross-sprint context) to:
///
///   - Read parent sprint outputs when dispatching a sprint with
///     `depends_on` (one-hop only)
///   - Persist this sprint's agent reply so downstream sprints can
///     read it on their own dispatch
///
/// Plain text, not JSON — the agent's reply IS prose. Storing it raw
/// keeps the inject-back-into-message format friction-free.
///
/// Output file path for a sprint's recorded agent reply.
///
/// New layout (#148): `<crew_root>/missions/<mission_id>/sprints/<sprint_id>-output.txt`
/// co-located with the sprint manifest under the per-mission directory.
fn sprint_output_path(mission_id: &str, sprint_id: &str) -> PathBuf {
    crate::crew::lifecycle::sprints_dir(mission_id).join(format!("{sprint_id}-output.txt"))
}

/// Resolve the dispatch message: when `sprint_id` is `Some(id)`, look
/// up the sprint's `depends_on` parents and prepend each parent's
/// recorded output as a "Prior sprint outputs" context block. Returns
/// the augmented message (or the original message unchanged if there
/// are no parents, no recorded outputs, or no sprint_id at all).
///
/// One-hop only — transitive ancestors are NOT walked. Stage 1 scope
/// per #146. The two-hop / DAG / context-budget refinements are Stage 2.
///
/// Missing parents / missing output files are NOT fatal — the dispatch
/// proceeds with whatever outputs are available. The dispatcher logs
/// to stderr which parents were found vs. missing so the operator can
/// see what context the agent received.
fn augment_message_with_sprint_context(
    sprint_id: Option<&str>,
    original_message: &str,
) -> Result<String> {
    let Some(sprint_id) = sprint_id else {
        return Ok(original_message.to_string());
    };

    // Load all sprints (cheap — small number of JSONs on disk). Find
    // the named sprint; if not found, log + dispatch as-is.
    let sprints = match load_sprints() {
        Ok(s) => s,
        Err(_) => {
            // Loader failure shouldn't block dispatch — just log.
            eprintln!(
                "darkmux crew dispatch: sprint loader unavailable; \
                 dispatching `{sprint_id}` without cross-sprint context."
            );
            return Ok(original_message.to_string());
        }
    };
    let sprint = match sprints.into_iter().find(|s| s.id == sprint_id) {
        Some(s) => s,
        None => {
            eprintln!(
                "darkmux crew dispatch: sprint `{sprint_id}` not found in \
                 crew root; dispatching without cross-sprint context."
            );
            return Ok(original_message.to_string());
        }
    };

    if sprint.depends_on.is_empty() {
        // No dependencies declared; nothing to inject.
        return Ok(original_message.to_string());
    }

    // For each parent, look up its recorded output. Missing outputs
    // are accumulated in `missing_parents` so the operator sees which
    // parents the agent didn't get context for.
    //
    // Per-mission layout (#148): parent sprints are assumed to live in the
    // same mission as the child sprint. Output files are co-located with
    // sprint manifests under `missions/<mission_id>/sprints/`.
    let mut parent_blocks: Vec<String> = Vec::new();
    let mut missing_parents: Vec<String> = Vec::new();
    for parent_id in &sprint.depends_on {
        let path = sprint_output_path(&sprint.mission_id, parent_id);
        match fs::read_to_string(&path) {
            Ok(content) if !content.trim().is_empty() => {
                parent_blocks.push(format!(
                    "### {parent_id}\n\n{}\n",
                    content.trim_end()
                ));
            }
            _ => {
                missing_parents.push(parent_id.clone());
            }
        }
    }

    if parent_blocks.is_empty() {
        if !missing_parents.is_empty() {
            eprintln!(
                "darkmux crew dispatch: sprint `{}` depends on {} \
                 (no recorded output for any parent yet — dispatching with \
                 bare message).",
                sprint_id,
                missing_parents.join(", ")
            );
        }
        return Ok(original_message.to_string());
    }

    if !missing_parents.is_empty() {
        eprintln!(
            "darkmux crew dispatch: sprint `{sprint_id}` got context from {} \
             parent(s); missing recorded output for: {}",
            parent_blocks.len(),
            missing_parents.join(", ")
        );
    } else {
        eprintln!(
            "darkmux crew dispatch: sprint `{sprint_id}` got context from {} \
             parent(s).",
            parent_blocks.len()
        );
    }

    let context_block = format!(
        "## Prior sprint outputs\n\n\
         The following sprints in this mission have completed and produced \
         output you can reference. Use them as context for your task below.\n\n\
         {}\n\
         ---\n\n\
         ## Your task\n\n\
         {original_message}",
        parent_blocks.join("\n"),
    );
    Ok(context_block)
}

/// Persist the agent's reply text to the sprint's output file after a
/// dispatch completes (#146 Stage 1 / #148 layout). Operator-visible
/// side effect: `<crew_root>/missions/<mission_id>/sprints/<sprint_id>-output.txt`
/// is created or overwritten with the agent's text reply.
///
/// No-op when `sprint_id` is `None` (dispatcher was called without
/// sprint context — typical for ad-hoc role dispatches).
///
/// The `mission_id` is resolved via `lifecycle::load_sprint_by_id` at
/// persist time. If the sprint is not found (e.g. the loader is
/// unavailable or the id is stale), persistence is skipped silently —
/// the output is already on stdout.
///
/// Returns the path written when persistence happened, `None`
/// otherwise. Errors are logged but don't fail the dispatch itself —
/// best-effort for downstream sprints.
fn persist_sprint_output(
    sprint_id: Option<&str>,
    reply_text: &str,
) -> Option<PathBuf> {
    let sprint_id = sprint_id?;
    if reply_text.trim().is_empty() {
        return None;
    }
    // Resolve mission_id via lifecycle so the output file lands in the
    // per-mission directory next to the sprint manifest (#148).
    let mission_id = match crate::crew::lifecycle::load_sprint_by_id(sprint_id) {
        Ok(s) => s.mission_id,
        Err(_) => {
            eprintln!(
                "darkmux crew dispatch: sprint `{sprint_id}` not found; \
                 skipping output persistence."
            );
            return None;
        }
    };
    let path = sprint_output_path(&mission_id, sprint_id);
    if let Some(parent) = path.parent() {
        if let Err(e) = fs::create_dir_all(parent) {
            eprintln!(
                "darkmux crew dispatch: failed to create output dir {}: {e}",
                parent.display()
            );
            return None;
        }
    }
    match fs::write(&path, reply_text) {
        Ok(()) => Some(path),
        Err(e) => {
            eprintln!(
                "darkmux crew dispatch: failed to persist sprint output to {}: {e}",
                path.display()
            );
            None
        }
    }
}

/// Outcome of applying a `--workdir` override for a dispatch. Returned
/// from `apply_workdir_override` so the caller can decide whether to
/// emit any operator-facing notice.
#[derive(Debug, PartialEq)]
enum WorkdirOutcome {
    /// `--workdir` not set; no change applied.
    NoChange,
    /// `repo` symlink created/replaced; previous state (if any) is
    /// included so the operator can see what was overwritten.
    Applied { previous_target: Option<PathBuf> },
}

/// Apply a `--workdir` override to the role's openclaw workspace. The
/// override is a symlink at `<role-workspace>/repo` pointing at the
/// operator-named path.
///
/// **Operator-sovereign contract** (#143):
/// - When `workdir` is `None`, this function is a no-op. Whatever
///   symlink already exists in the workspace (or none) is what the
///   agent sees.
/// - When `workdir` is `Some(path)`, the path MUST exist as a directory
///   (or symlink to one) — we don't fabricate scope. The function
///   replaces any existing `repo` symlink with one pointing at the
///   operator's choice.
/// - We do NOT remove the symlink after dispatch. The operator's
///   explicit declaration persists until they `rm` it or pass a
///   different `--workdir`. This avoids the crash-mid-dispatch
///   restore-fragility class of bugs.
fn apply_workdir_override(
    workdir: Option<&Path>,
    role_workspace: &Path,
) -> Result<WorkdirOutcome> {
    let Some(target) = workdir else {
        return Ok(WorkdirOutcome::NoChange);
    };

    // Symlink-escape guard + existence + is-dir check via the shared
    // validator (#255 Wave-E.2). Closes the long-deferred parity gap
    // where the openclaw path silently followed symlinks while
    // dispatch_internal had the guard since #232. Validator returns
    // the canonical (symlink-free) path — operations below use the
    // operator-supplied `target` for messages but the canonical path
    // for filesystem ops is captured at the link-create site below.
    let _resolved = crate::workdir::validate_workdir(target)?;

    fs::create_dir_all(role_workspace).with_context(|| {
        format!("creating role workspace {}", role_workspace.display())
    })?;

    let link_path = role_workspace.join("repo");

    // Read previous state for the operator-facing notice.
    let previous_target = if link_path.is_symlink() {
        fs::read_link(&link_path).ok()
    } else if link_path.exists() {
        // Not a symlink but exists — refuse to clobber. This catches
        // the case where an operator has a real `repo/` directory in
        // the workspace.
        bail!(
            "{} exists but is not a symlink; refusing to clobber. \
             Remove it manually if you want --workdir to manage scope, \
             or omit --workdir to use the existing path.",
            link_path.display()
        );
    } else {
        None
    };

    // Remove the existing symlink (if any) and create a fresh one.
    if link_path.is_symlink() {
        fs::remove_file(&link_path).with_context(|| {
            format!("removing existing symlink at {}", link_path.display())
        })?;
    }

    let absolute_target = target
        .canonicalize()
        .unwrap_or_else(|_| target.to_path_buf());
    std::os::unix::fs::symlink(&absolute_target, &link_path).with_context(|| {
        format!(
            "creating symlink {} -> {}",
            link_path.display(),
            absolute_target.display()
        )
    })?;

    Ok(WorkdirOutcome::Applied { previous_target })
}

fn is_openclaw_noise(path: &Path) -> bool {
    let name = match path.file_name().and_then(|s| s.to_str()) {
        Some(n) => n,
        None => return false,
    };
    name.ends_with(".trajectory.jsonl")
        || name.ends_with(".trajectory-path.json")
        || name.ends_with(".checkpoint.jsonl")
        || (name.ends_with(".jsonl") && name.contains("checkpoint"))
        || matches!(
            name,
            "AGENTS.md"
                | "BOOTSTRAP.md"
                | "HEARTBEAT.md"
                | "IDENTITY.md"
                | "SOUL.md"
                | "TOOLS.md"
                | "USER.md"
                | "sessions.json"
        )
}

/// Run a single dispatch end-to-end.
pub fn dispatch(opts: DispatchOpts) -> Result<DispatchResult> {
    // PR-C.3 tier-routing branch (#246): when --machine is set AND it's
    // not the local machine, publish to the work queue and (if --wait)
    // block on the worker's dispatch.complete flow record. Local
    // dispatches (no --machine, OR --machine == local) fall through to
    // the existing local path unchanged.
    if let Some(target) = opts.machine.clone() {
        let local = crate::flow::resolve_machine_id();
        match routing_decision(Some(target.as_str()), local.as_deref()) {
            RoutingDecision::Local { matches_was_explicit: true } => {
                eprintln!(
                    "darkmux crew dispatch: --machine={target} matches local machine_id; \
                     routing locally."
                );
            }
            RoutingDecision::Remote { target, local_unknown: true } => {
                // PR-C.3 review MEDIUM (Wave-E.7): local machine_id is
                // unresolvable (no DARKMUX_MACHINE_ID, hostname failed).
                // Routing via queue is the only option — surface the
                // ambiguity loudly so the operator sees what happened.
                eprintln!(
                    "darkmux crew dispatch: WARNING — local DARKMUX_MACHINE_ID is unresolvable. \
                     --machine={target} routes via the queue regardless. \
                     If you intended a local dispatch, set DARKMUX_MACHINE_ID to make \
                     tier-routing decisions deterministic."
                );
                // #290 — emit the pinned route record so the audit
                // trail + topology UI see the operator-pinned routing
                // decision (parity with the auto-route path's record).
                // Validation runs BEFORE the emit so a role-load
                // failure OR an invalid tier doesn't leave a misleading
                // "pinned" record in the audit chain.
                let role_tier = resolve_role_tier_for_record(&opts)?;
                let session_id = emit_route_record_and_resolve_session(
                    &opts,
                    &role_tier,
                    Some(&target),
                );
                let mut opts = opts;
                opts.session_id = Some(session_id);
                return dispatch_via_queue(opts, Some(&target));
            }
            RoutingDecision::Remote { target, local_unknown: false } => {
                // #290 — emit the pinned route record so the audit
                // trail + topology UI see the operator-pinned routing
                // decision (parity with the auto-route path's record).
                // Validation runs BEFORE the emit so a role-load
                // failure OR an invalid tier doesn't leave a misleading
                // "pinned" record in the audit chain.
                let role_tier = resolve_role_tier_for_record(&opts)?;
                let session_id = emit_route_record_and_resolve_session(
                    &opts,
                    &role_tier,
                    Some(&target),
                );
                let mut opts = opts;
                opts.session_id = Some(session_id);
                return dispatch_via_queue(opts, Some(&target));
            }
            RoutingDecision::Local { matches_was_explicit: false } => {
                // Unreachable in this branch (we matched Some(target) above)
                // — but the enum's total shape covers it.
            }
        }
    } else {
        // #247 PR-B — auto-route by tier when no explicit --machine.
        // If the role's tier doesn't match the local machine's tier
        // AND the fleet has a peer in the role's tier, publish to
        // the tier-stream and let the consumer group claim. The
        // worker that picks it up does its own preflight — we skip
        // the local one (same shape as the explicit --machine path
        // above).
        if let Some(auto_target_tier) = auto_route_target_tier(&opts)? {
            let local_tier = crate::flow::resolve_machine_tier();
            eprintln!(
                "darkmux crew dispatch: auto-routing role=`{}` via tier=`{}` \
                 (local tier=`{}`, no --machine — consumer group claims).",
                opts.role_id,
                auto_target_tier,
                local_tier.as_deref().unwrap_or("<unknown>"),
            );
            // Emit the dispatch-route flow record so the topology UI
            // and the audit trail can render WHY the work went to the
            // tier-stream rather than running locally. (#247 PR-C)
            // Session id resolved + re-attached so dispatch_via_queue
            // uses the same one — the worker's start/complete records
            // pair with this route record by session_id.
            let session_id = emit_route_record_and_resolve_session(
                &opts,
                &auto_target_tier,
                None, // auto-route — consumer group claims
            );
            let mut opts = opts;
            opts.session_id = Some(session_id);
            return dispatch_via_queue(opts, None);
        }
    }

    // Route to the in-house container-bounded runtime when the operator
    // explicitly opts in via `--runtime internal`. Default stays the
    // openclaw path (everything below this branch).
    if opts.runtime == Runtime::Internal {
        return crate::crew::dispatch_internal::dispatch(opts);
    }

    // 0. Pre-flight: nudge the operator if the daemon isn't up. The
    //    dispatch will still write flow records to disk, but they
    //    won't be observable in the viewer until the daemon comes up.
    //    Non-blocking; the dispatch proceeds either way (#104 S3).
    crate::serve::nudge_if_daemon_unreachable("crew dispatch");

    // 1. Load the role + its .md prompt
    let role = load_role_or_bail(&opts.role_id)?;
    let bare_prompt = role_prompt_or_bail(&role)?;
    // #147: augment with operator-identity from ~/.darkmux/identity.md
    // if present. Pre-flight compares against the augmented form so
    // drift detection matches what `darkmux crew sync` wrote.
    let prompt = augment_prompt_with_identity(&bare_prompt);
    let agent_id = agent_id_for(&opts.role_id);

    // 1.5. Licensed-adjacent ACK gate. For roles whose prompts operate
    //      in domains regulated by professional licensure (health, law,
    //      fitness), require an operator acknowledgment on first dispatch.
    //      The prompts encode the boundary at runtime; this gate makes the
    //      same boundary visible to the operator at the CLI surface.
    require_licensed_adjacent_ack(&opts.role_id)
        .context("licensed-adjacent role dispatch requires acknowledgment")?;

    // 2. Pre-flight against openclaw config. Run BEFORE --workdir
    //    mutates state (per QA review on #143 Stage 1): cheap-and-
    //    reversible checks come first so a pre-flight failure doesn't
    //    leave the operator with a half-applied symlink. The
    //    operator-never-has-to-wonder rule from CLAUDE.md says
    //    silent-partial-state from a failed dispatch is exactly the
    //    wondering this discipline prevents.
    let role_workspace = default_workspace_for_role(&opts.role_id);
    if !opts.skip_preflight {
        let openclaw_path = default_openclaw_config();
        let openclaw_config = read_openclaw_config(&openclaw_path)?;
        preflight_check(&openclaw_config, &agent_id, &role, &prompt)
            .with_context(|| {
                format!(
                    "pre-flight failed for `{agent_id}`. Run `darkmux crew sync` to update openclaw config from the manifests."
                )
            })?;
    }

    // 2.5. Apply --workdir override (#143 Stage 1). State mutation only
    //      after pre-flight has cleared. When the operator passes an
    //      explicit workdir, set up the role's workspace `repo` symlink
    //      to point at it. When omitted, the workspace is left whatever
    //      state it was in (no auto-mutation).
    let workdir_outcome = apply_workdir_override(
        opts.workdir.as_deref(),
        &role_workspace,
    )?;
    if let WorkdirOutcome::Applied { previous_target } = &workdir_outcome {
        let absolute_target = opts
            .workdir
            .as_deref()
            .and_then(|p| p.canonicalize().ok())
            .unwrap_or_else(|| opts.workdir.clone().unwrap_or_default());
        match previous_target {
            Some(prev) => eprintln!(
                "darkmux crew dispatch: --workdir replaced `{}/repo` (was -> {}; now -> {})",
                role_workspace.display(),
                prev.display(),
                absolute_target.display()
            ),
            None => eprintln!(
                "darkmux crew dispatch: --workdir installed `{}/repo` (now -> {})",
                role_workspace.display(),
                absolute_target.display()
            ),
        }
    }

    // 2.75. Cross-sprint context injection (#146 Stage 1). When a
    //       --sprint-id was passed, look up the sprint's depends_on
    //       parents and prepend each recorded output as a "Prior
    //       sprint outputs" context block. One-hop only — transitive
    //       ancestors are NOT walked. Missing parent outputs are
    //       logged and the dispatch proceeds with whatever's available.
    let augmented_message = augment_message_with_sprint_context(
        opts.sprint_id.as_deref(),
        &opts.message,
    )?;

    // 3. Resolve session id. Always pass `--session-id` to openclaw — when
    //    the caller didn't supply one, generate a fresh `crew-dispatch-
    //    <role>-<timestamp>`. Without this, openclaw silently reuses the
    //    per-agent `agent:darkmux-<role>:main` session across dispatches,
    //    leading to cross-task context pollution (#88).
    let resolved_session_id = opts
        .session_id
        .clone()
        .unwrap_or_else(|| fresh_session_id(&opts.role_id));

    // Resolve the model openclaw will route to for this agent — stamped
    // onto both the start and end flow records so the viewer can show
    // "which model ran this dispatch" without cross-referencing the
    // model-status pill (#106). Resolved once, reused for both records
    // of the pair so they reference the same model even if the agent's
    // config is edited mid-dispatch. Non-fatal: None on failure.
    let resolved_model = resolve_dispatch_model(&agent_id);

    // Flow emission: dispatch_start lands on disk before openclaw is
    // even invoked, so the viewer sees the local-tier event the instant
    // we begin. Pairs with the dispatch_complete / dispatch_error
    // record below via session_id (the viewer's computeDispatchDurations
    // does the start↔end pairing). Non-fatal: emission errors are
    // ignored so a flows-dir write problem doesn't sink the dispatch.
    //
    // Schema 1.6 (#204) enriches the payload with runtime metadata so
    // the viewer can render which runtime + prompt size handled the
    // work without cross-referencing other state.
    let dispatch_start_payload = serde_json::json!({
        "runtime": "openclaw",
        "prompt_chars": augmented_message.chars().count(),
        "agent_id": agent_id,
    });
    let _ = crate::flow::record(build_dispatch_record_with_payload(
        crate::flow::Level::Info,
        "dispatch start",
        &opts.role_id,
        &resolved_session_id,
        resolved_model.as_deref(),
        Some(dispatch_start_payload),
    ));

    let dispatch_start_instant = std::time::Instant::now();

    // 4. Invoke openclaw agent
    let mut cmd = Command::new("openclaw");
    cmd.args(["agent", "--local", "--agent", &agent_id, "--json"]);
    cmd.args(["--session-id", &resolved_session_id]);
    cmd.args(["--timeout", &opts.timeout_seconds.to_string()]);
    if let Some(deliver) = &opts.deliver {
        let (chan, target) = deliver
            .split_once(':')
            .ok_or_else(|| anyhow!("--deliver must be `<channel>:<target>`, got `{deliver}`"))?;
        cmd.args(["--channel", chan, "--reply-to", target, "--deliver"]);
    }
    cmd.args(["--message", &augmented_message]);

    let output_result = cmd
        .output()
        .with_context(|| format!("running `openclaw agent {agent_id}`"));

    let wall_ms = dispatch_start_instant.elapsed().as_millis() as u64;

    // Emit the dispatch end record BEFORE propagating any error or
    // returning Ok — emission must reflect both success and failure paths
    // so the viewer never sees a dangling start with no terminal event.
    let (action, level) = match &output_result {
        Ok(o) if o.status.success() => ("dispatch complete", crate::flow::Level::Info),
        _ => ("dispatch error", crate::flow::Level::Error),
    };
    let (stdout_chars, stderr_chars, exit_code) = match &output_result {
        Ok(o) => (
            o.stdout.len(),
            o.stderr.len(),
            o.status.code(),
        ),
        Err(_) => (0, 0, None),
    };
    let dispatch_complete_payload = serde_json::json!({
        "runtime": "openclaw",
        "wall_ms": wall_ms,
        "stdout_chars": stdout_chars,
        "stderr_chars": stderr_chars,
        "exit_code": exit_code,
        "result_class": if matches!(&output_result, Ok(o) if o.status.success()) {
            "ok"
        } else {
            "error"
        },
    });
    let _ = crate::flow::record(build_dispatch_record_with_payload(
        level,
        action,
        &opts.role_id,
        &resolved_session_id,
        resolved_model.as_deref(),
        Some(dispatch_complete_payload),
    ));

    let output = output_result?;

    // 5. Post-dispatch: snapshot filesystem state at each caller-supplied
    //    watch path. Surfaces ground-truth file presence + sizes so SIGNOFF
    //    claims are verifiable (#89). Empty watch_paths => empty snapshot
    //    vector; the CLI handler decides whether to default to the role's
    //    workspace dir, so library callers (e.g. sprint_cli's internal
    //    dispatch) can opt out of the echo without ceremony.
    let watched_state: Vec<WatchedPathState> = opts
        .watch_paths
        .iter()
        .map(|p| snapshot_watched_path(p))
        .collect();

    let stdout_text = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr_text = String::from_utf8_lossy(&output.stderr).to_string();

    // 6. Persist sprint output for downstream context injection (#146
    //    Stage 1). Best-effort — failures logged to stderr but don't
    //    fail the dispatch. The reply text comes from openclaw's JSON
    //    envelope on stdout; we extract `payloads[0].text` if present
    //    and fall back to the full stdout otherwise.
    //
    //    **Gated on dispatch success** (QA review on #157): a failed
    //    dispatch (timeout, agent error, partial truncation) must NOT
    //    clobber a previously-clean output. Operators re-running a
    //    sprint that already had a recorded parent output should not
    //    have downstream sprints silently start reading garbage.
    //    Stderr already tells them what failed; they can decide whether
    //    to hand-edit the output file or accept the prior recording.
    if output.status.success() {
        if let Some(sprint_id) = opts.sprint_id.as_deref() {
            let reply_text = extract_payload_text(&stdout_text)
                .unwrap_or_else(|| stdout_text.clone());
            if let Some(path) = persist_sprint_output(Some(sprint_id), &reply_text) {
                eprintln!(
                    "darkmux crew dispatch: sprint `{sprint_id}` output persisted to {}",
                    path.display()
                );
            }
        }
    } else if opts.sprint_id.is_some() {
        eprintln!(
            "darkmux crew dispatch: dispatch failed (exit {}); NOT persisting sprint output. \
             Any prior recorded output remains intact.",
            output.status.code().unwrap_or(-1)
        );
    }

    Ok(DispatchResult {
        exit_code: output.status.code().unwrap_or(-1),
        stdout: stdout_text,
        stderr: stderr_text,
        session_id: resolved_session_id,
        watched_state,
    })
}

/// Extract the `payloads[0].text` field from openclaw's JSON envelope.
/// Returns `None` if the stdout isn't JSON or the field is missing.
/// Used for sprint-output persistence (#146 Stage 1) so the recorded
/// file contains the agent's prose, not openclaw's outer envelope.
///
/// Trims leading/trailing whitespace before parsing — openclaw sometimes
/// emits a trailing newline that would otherwise fail `serde_json::from_str`.
///
/// **First payload only** — if openclaw ever emits multi-payload replies
/// (multiple `text` segments for a single turn), this returns the first.
/// A future expansion that concats segments would be a Stage 2 concern.
fn extract_payload_text(stdout: &str) -> Option<String> {
    let value: serde_json::Value = serde_json::from_str(stdout.trim()).ok()?;
    value
        .get("payloads")?
        .as_array()?
        .first()?
        .get("text")?
        .as_str()
        .map(|s| s.to_string())
}

/// Publish a dispatch to the fleet work queue instead of running it
/// locally (#246 PR-C.3). Called from `dispatch` when `opts.machine`
/// is set to a non-local id. If `opts.wait` is true (the default for
/// `crew dispatch`), blocks on the worker's `dispatch.complete` flow
/// record before returning; otherwise returns immediately with a
/// fire-and-forget synthetic result.
/// `target_machine: Some(id)` stamps the WorkJob's hint field so the
/// audit trail and topology view see the operator-pinned target.
/// `None` is the auto-route case (#247 PR-B) — the role's tier
/// alone drives the work-stream choice; consumer-group claim picks
/// whichever matching-tier worker is free first.
fn dispatch_via_queue(opts: DispatchOpts, target_machine: Option<&str>) -> Result<DispatchResult> {
    use crate::fleet;

    // Determine the role's tier requirement (drives the work stream
    // selection). Roles MUST declare a concrete tier for cross-machine
    // dispatch — workers register on `darkmux:work:<inference|hub|client>`
    // streams; a role with `tier: None` would publish to
    // `darkmux:work:any` which has no consumer and the wait loop would
    // time out without explanation. Bail loud with operator-actionable
    // hints. (PR-C.3 review HIGH-1)
    let role = load_role_or_bail(&opts.role_id)?;
    let role_tier = match role.tier.clone() {
        Some(t) if !t.trim().is_empty() && t != "any" => t,
        Some(t) => {
            bail!(
                "role `{}` has tier={:?} which has no fleet consumer (workers \
                 register on inference/hub/client streams). Either: (a) edit \
                 the role manifest to declare a concrete tier, or (b) omit \
                 --machine to dispatch locally.",
                opts.role_id, t
            );
        }
        None => {
            bail!(
                "role `{}` has no tier declaration in its manifest. \
                 Cross-machine dispatch requires the role to declare which \
                 machine class it runs on. Either: (a) add \"tier\": \
                 \"inference\" (or \"hub\") to the role's JSON manifest, or \
                 (b) omit --machine to dispatch locally.",
                opts.role_id
            );
        }
    };

    // The Redis URL is required for cross-machine dispatch. If it's
    // unset, the operator hasn't configured the fleet substrate — bail
    // loud with the fix-it pointer.
    let redis_url = std::env::var("DARKMUX_REDIS_URL")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .ok_or_else(|| {
            let context = match target_machine {
                Some(m) => format!("--machine={m}"),
                None => format!(
                    "cross-tier auto-route (local tier != role tier=`{role_tier}`)"
                ),
            };
            anyhow!(
                "{context} requires DARKMUX_REDIS_URL to be set \
                 (the fleet work queue lives on Redis). \
                 Single-machine fleets shouldn't dispatch cross-tier."
            )
        })?;

    // Resolve session_id up front — the worker needs it to stamp on
    // the dispatch.complete record, and --wait needs it as the join key.
    let session_id = opts
        .session_id
        .clone()
        .unwrap_or_else(|| fresh_session_id(&opts.role_id));

    // Build the WorkJob from DispatchOpts. The shape mirrors what the
    // worker side reconstructs via `WorkJob::into_dispatch_opts` —
    // round-trip parity matters for cross-machine dispatch.
    let job = fleet::build_work_job(
        role_tier,
        target_machine.map(|s| s.to_string()),
        opts.role_id.clone(),
        opts.message.clone(),
        session_id.clone(),
        opts.deliver.clone(),
        opts.workdir.as_ref().map(|p| p.display().to_string()),
        opts.sprint_id.clone(),
        opts.runtime,
        opts.timeout_seconds,
        crate::flow::resolve_machine_id(),
        crate::flow::resolve_orchestrator(),
    );

    // Open the Redis client lazily here (not at darkmux startup) so the
    // local-dispatch path doesn't pay any connection cost. The same
    // `raw_url` is reused by `wait_for_completion` below.
    let raw_url = crate::flow::RawRedisUrl::new(redis_url);
    let client = redis::Client::open(raw_url.expose_for_probe())
        .with_context(|| format!("opening Redis client {raw_url} for --machine dispatch"))?;

    // Publish — `publish_job` runs validate() before XADD, so a
    // malformed job bails before crossing the network.
    let work_id = fleet::publish_job(&client, &job)
        .context("publishing WorkJob to fleet queue")?;

    eprintln!(
        "darkmux crew dispatch: published work_id={work_id} tier={} \
         target_machine={} session={session_id}",
        job.target_tier,
        target_machine.unwrap_or("<auto-route>"),
    );

    if !opts.wait {
        // Fire-and-forget. Return a synthetic success result; the
        // operator polls via `darkmux flow tail --session <id>`.
        return Ok(DispatchResult {
            exit_code: 0,
            stdout: format!("published; not waiting (session_id={session_id})\n"),
            stderr: String::new(),
            session_id,
            watched_state: Vec::new(),
        });
    }

    // Block on the worker's dispatch.complete. Timeout = the job's own
    // timeout + a small slack (the worker's clock starts at claim, so
    // the dispatching client's wait must outlast the worker's budget).
    let wait_timeout = std::time::Duration::from_secs(
        (opts.timeout_seconds as u64).saturating_add(30),
    );
    eprintln!(
        "darkmux crew dispatch: waiting for dispatch.complete (session={session_id}, \
         timeout={}s)…",
        wait_timeout.as_secs()
    );
    let completion = fleet::wait_for_completion(&raw_url, &session_id, wait_timeout)
        .context("waiting for remote dispatch completion")?;

    eprintln!(
        "darkmux crew dispatch: completed session={} result={} wall_ms={:?}",
        completion.session_id, completion.result_class, completion.wall_ms
    );

    // Translate completion → DispatchResult. We don't have stdout from
    // the worker side (it lives in the worker's flow records, not the
    // dispatching CLI's stdout); surface the result_class + wall_ms in
    // the synthetic stdout so the operator sees something useful.
    Ok(completion_to_dispatch_result(completion))
}

/// Translate a queue completion (from `fleet::wait_for_completion`)
/// into the `DispatchResult` shape the CLI returns. Pulls the actual
/// `exit_code` out of the dispatch.complete payload when present;
/// falls back to a binary 0/1 derived from `result_class` only when
/// the payload lacks an explicit exit_code.
///
/// Closes the PR-C.3 review MEDIUM: the prior code unconditionally
/// squashed to `exit_code = if result_class == "ok" { 0 } else { 1 }`,
/// discarding the worker's actual exit code (which dispatchers reading
/// the flow record DO see). Operators relying on specific exit codes
/// for CI gating or shell scripting lost that signal in the cross-
/// machine path. (#255 Wave-E.6)
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

/// Look up the role's tier for stamping into a `dispatch route`
/// record's payload + validating it's acceptable for cross-machine
/// dispatch (#290). Returns the concrete tier on success. Returns
/// `Err` on:
/// - role manifest not found / unreadable / unparseable (B1)
/// - role.tier is `None`, empty, or `"any"` — cross-machine
///   dispatch needs a concrete tier; bailing here means the
///   record never lands in the audit chain claiming a "pinned"
///   decision for a role the substrate would then reject (B2)
///
/// Bailing BEFORE the record emit keeps the audit substrate honest:
/// every persisted `dispatch route` record corresponds to a routing
/// decision the substrate actually accepted.
fn resolve_role_tier_for_record(opts: &DispatchOpts) -> Result<String> {
    let role = load_role_or_bail(&opts.role_id)?;
    match role.tier.as_deref().map(str::trim) {
        Some(t) if !t.is_empty() && t != "any" => Ok(t.to_string()),
        Some(t) => bail!(
            "role `{}` has tier={t:?} which is invalid for cross-machine \
             dispatch (workers register on inference/hub/client streams). \
             Either edit the role manifest to declare a concrete tier, or \
             omit --machine to dispatch locally.",
            opts.role_id,
        ),
        None => bail!(
            "role `{}` has no tier declaration in its manifest. \
             Cross-machine dispatch requires the role to declare which \
             machine class it runs on. Either add \"tier\": \"inference\" \
             (or \"hub\") to the role's JSON manifest, or omit --machine \
             to dispatch locally.",
            opts.role_id,
        ),
    }
}

/// Emit a `dispatch route` flow record at the moment the routing
/// decision is made and return the resolved session_id so the caller
/// can re-attach it to `opts.session_id`. This ensures the route
/// record's session_id matches the worker's subsequent `dispatch
/// start` / `dispatch complete` records — the topology UI's pair-
/// rendering depends on session_id continuity.
///
/// Called from BOTH the auto-route path (`target_machine: None`,
/// `decision: "auto-route"`) AND the explicit-`--machine` path
/// (`target_machine: Some(id)`, `decision: "pinned"`) — #290 closed
/// the gap where #285 PR-C only emitted on the auto-route arm,
/// leaving operator-pinned routing decisions unrecorded in the audit
/// trail.
fn emit_route_record_and_resolve_session(
    opts: &DispatchOpts,
    role_tier: &str,
    target_machine: Option<&str>,
) -> String {
    let local_tier = crate::flow::resolve_machine_tier();
    let session_id = opts
        .session_id
        .clone()
        .unwrap_or_else(|| fresh_session_id(&opts.role_id));
    let payload = build_route_payload(role_tier, local_tier.as_deref(), target_machine);
    let _ = crate::flow::record(build_dispatch_record_with_payload(
        crate::flow::Level::Info,
        "dispatch route",
        &opts.role_id,
        &session_id,
        None,
        Some(payload),
    ));
    session_id
}

/// Construct the payload for a `dispatch route` flow record (#247
/// PR-C). Pure; testable in isolation. `target_machine: Some(id)`
/// signals an operator-pinned explicit-machine dispatch; `None` is
/// the auto-route case (consumer group claims).
fn build_route_payload(
    role_tier: &str,
    local_tier: Option<&str>,
    target_machine: Option<&str>,
) -> serde_json::Value {
    serde_json::json!({
        "role_tier": role_tier,
        "local_tier": local_tier,
        "target_machine": target_machine,
        // `decision` makes the operator-visible verdict explicit in
        // the audit trail without re-deriving it from the other
        // fields (the topology UI uses this to color routing edges).
        "decision": if target_machine.is_some() { "pinned" } else { "auto-route" },
    })
}

/// Decide whether the dispatch should auto-route via the work queue
/// because the role's declared tier doesn't match the local machine's
/// tier. Returns:
/// - `Ok(None)` — dispatch locally (role.tier ∈ {None, "any"}, OR
///   role.tier == local tier, OR auto-route would be pointless)
/// - `Ok(Some(role_tier))` — fleet has a peer in `role_tier`; publish
///   via queue. Caller (dispatch entry) emits the banner and calls
///   `dispatch_via_queue(opts, None)`.
/// - `Err(_)` — role.tier doesn't match local AND no fleet peer
///   matches; bail loud with operator-actionable hint pointing at
///   `darkmux fleet add`. (#247 PR-B)
fn auto_route_target_tier(opts: &DispatchOpts) -> Result<Option<String>> {
    let role = load_role_or_bail(&opts.role_id)?;
    let role_tier = match role.tier.as_deref().map(str::trim) {
        Some("") | Some("any") | None => return Ok(None), // local
        Some(t) => t.to_string(),
    };
    let local_tier = crate::flow::resolve_machine_tier();
    if local_tier.as_deref() == Some(role_tier.as_str()) {
        // Local matches role's tier — dispatch locally; no queue cost.
        return Ok(None);
    }
    // Tier mismatch — consult the fleet roster.
    let roster = match crate::fleet::load_roster() {
        Ok(r) => r,
        Err(e) => bail!(
            "role `{}` requires tier=`{role_tier}` (local tier=`{}`) but the fleet \
             roster couldn't be loaded: {e}. Run `darkmux fleet status` to inspect.",
            opts.role_id,
            local_tier.as_deref().unwrap_or("<unset>"),
        ),
    };
    let candidates = crate::fleet::candidates_for_tier(&roster, &role_tier);
    if candidates.is_empty() {
        bail!(
            "role `{}` requires tier=`{role_tier}` but no fleet peer is in that \
             tier (local tier=`{}`). Either: \
             (a) add a peer with `darkmux fleet add <id> --tier {role_tier} --address <addr>`, \
             or (b) edit the role manifest to declare `tier: \"{}\"` if this work belongs \
             on the local machine.",
            opts.role_id,
            local_tier.as_deref().unwrap_or("<unset>"),
            local_tier.as_deref().unwrap_or("any"),
        );
    }
    Ok(Some(role_tier))
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
pub fn routing_decision(
    machine: Option<&str>,
    local_machine_id: Option<&str>,
) -> RoutingDecision {
    match (machine, local_machine_id) {
        (None, _) => RoutingDecision::Local { matches_was_explicit: false },
        (Some(t), Some(l)) if t == l => {
            RoutingDecision::Local { matches_was_explicit: true }
        }
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

fn completion_to_dispatch_result(c: crate::fleet::CompletionResult) -> DispatchResult {
    let payload_exit_code = c
        .payload
        .as_ref()
        .and_then(|p| p.get("exit_code"))
        .and_then(|v| v.as_i64())
        .map(|n| n as i32);
    let exit_code = payload_exit_code.unwrap_or(if c.result_class == "ok" { 0 } else { 1 });
    let stdout = format!(
        "remote dispatch complete; result_class={} exit_code={exit_code} wall_ms={:?} session={}\n\
         (full output in worker's flow records — \
          tail `~/.darkmux/flows/<date>.jsonl` for session={})\n",
        c.result_class, c.wall_ms, c.session_id, c.session_id,
    );
    DispatchResult {
        exit_code,
        stdout,
        stderr: String::new(),
        session_id: c.session_id,
        watched_state: Vec::new(),
    }
}

/// Build a flow record for a dispatch lifecycle event (`dispatch start`,
/// `dispatch complete`, `dispatch error`). All three share the same
/// session_id so the viewer pairs start↔end into a single wall-clock
/// arc per dispatch. `handle` is the role id (operator-readable label);
/// `session_id` is the full openclaw session identifier; `model` is the
/// resolved LMStudio model id (best-effort — `None` when the openclaw
/// config can't be read or no model is pinned for this agent).
///
/// Legacy wrapper around `build_dispatch_record_with_payload` for the
/// pre-#204 call shape. The two main openclaw-path emit sites now go
/// through `_with_payload` directly to carry runtime metadata; this
/// wrapper survives for tests + future callers that don't need payload.
#[allow(dead_code)]
pub(crate) fn build_dispatch_record(
    level: crate::flow::Level,
    action: &str,
    role_id: &str,
    session_id: &str,
    model: Option<&str>,
) -> crate::flow::FlowRecord {
    build_dispatch_record_with_payload(
        level,
        action,
        role_id,
        session_id,
        model,
        None,
    )
}

/// Same as `build_dispatch_record` but with an explicit `payload` for
/// event-specific fields (#204). The richer dispatch events (turn,
/// tool, compaction, reasoning) use this directly; the bare
/// `build_dispatch_record` wrapper preserves the legacy call shape.
pub(crate) fn build_dispatch_record_with_payload(
    level: crate::flow::Level,
    action: &str,
    role_id: &str,
    session_id: &str,
    model: Option<&str>,
    payload: Option<serde_json::Value>,
) -> crate::flow::FlowRecord {
    crate::flow::FlowRecord {
        ts: crate::flow::ts_utc_now(),
        level,
        category: crate::flow::Category::Work,
        tier: crate::flow::Tier::Local,
        stage: crate::flow::Stage::Dispatch,
        action: action.to_string(),
        handle: role_id.to_string(),
        sprint_id: None,
        session_id: Some(session_id.to_string()),
        source: Some("crew_dispatch".to_string()),
        model: model.map(String::from),
        reasoning: None,
        mission_id: None,
        machine_id: None,
        orchestrator: None,
        prev_hash: None,
        hash: None,
        payload,
        machine_tier: None,
        work_id: None,
        attempt: None,
    }
}

/// Best-effort resolve the LMStudio model id openclaw will route this
/// agent's dispatches to. Tries `agents.list[<agent-id>].model` first,
/// falls back to `agents.defaults.model.primary`. Returns `None` when:
///   - The openclaw config can't be located or parsed.
///   - Neither path resolves to a string.
///
/// Non-fatal everywhere — a missing model annotation degrades to no
/// `model` field on the flow record, not a failed dispatch.
///
/// Deliberately silent on failure: surfacing "no model resolved" as
/// stderr noise on every dispatch would conflict with the dispatcher's
/// existing minimal output. The structural "is the model pinned?"
/// check belongs to `darkmux doctor`'s `agents-default-model-resolves`
/// rule (#91/#102), which runs at operator-explicit pre-flight time
/// and is the right place for that signal. The flow record's absent
/// `model` field is operator-visible enough on its own — it shows as
/// missing in the viewer, which is the same signal in the right place.
pub(crate) fn resolve_dispatch_model(agent_id: &str) -> Option<String> {
    let path = default_openclaw_config();
    let raw = fs::read_to_string(&path).ok()?;
    let config: Value = serde_json::from_str(&raw).ok()?;

    // Try per-agent override in agents.list[].
    if let Some(agents) = config
        .get("agents")
        .and_then(|a| a.get("list"))
        .and_then(|l| l.as_array())
    {
        if let Some(m) = agents
            .iter()
            .find(|a| a.get("id").and_then(|i| i.as_str()) == Some(agent_id))
            .and_then(|a| a.get("model"))
            .and_then(|m| m.as_str())
        {
            return Some(m.to_string());
        }
    }

    // Fall back to defaults.model.primary.
    config
        .get("agents")
        .and_then(|a| a.get("defaults"))
        .and_then(|d| d.get("model"))
        .and_then(|m| m.get("primary"))
        .and_then(|p| p.as_str())
        .map(String::from)
}

fn load_role_or_bail(role_id: &str) -> Result<Role> {
    let roles = load_roles().context("loading crew role manifests")?;
    roles
        .into_iter()
        .find(|r| r.id == role_id)
        .ok_or_else(|| anyhow!("no role with id `{role_id}` found in crew manifests"))
}

fn role_prompt_or_bail(role: &Role) -> Result<String> {
    load_role_prompt(&role.id).ok_or_else(|| {
        anyhow!(
            "role `{}` has no `.md` system prompt. Author one at \
             `templates/builtin/roles/{}.md` (or override at \
             `<crew_root>/roles/{}.md`).",
            role.id, role.id, role.id
        )
    })
}

fn read_openclaw_config(path: &Path) -> Result<Value> {
    if !path.exists() {
        bail!(
            "openclaw config not found at {}. \
             Set DARKMUX_OPENCLAW_CONFIG to override.",
            path.display()
        );
    }
    let raw = fs::read_to_string(path)
        .with_context(|| format!("reading openclaw config at {}", path.display()))?;
    serde_json::from_str(&raw)
        .with_context(|| format!("parsing openclaw config at {}", path.display()))
}

/// The three pre-flight checks from issue #55, scoped to what's verifiable
/// pre-dispatch:
///
///   - inventory: the `darkmux/<role>` agent entry exists
///   - field consistency: systemPromptOverride matches the manifest's `.md`
///   - tool palette matches the manifest's `tool_palette`
///   - **model pin (#182)**: `agents.list[].model` matches the active pin
///     table's expectation for this role (closes the last silent-fallback
///     hole — sync-time + doctor-time + dispatch-time enforcement chain)
fn preflight_check(config: &Value, agent_id: &str, role: &Role, expected_prompt: &str) -> Result<()> {
    let agents_list = config
        .get("agents")
        .and_then(|a| a.get("list"))
        .and_then(|l| l.as_array())
        .ok_or_else(|| anyhow!("openclaw config has no `agents.list` array"))?;

    let entry = agents_list
        .iter()
        .find(|a| a.get("id").and_then(|s| s.as_str()) == Some(agent_id))
        .ok_or_else(|| {
            anyhow!(
                "agent `{agent_id}` not found in openclaw `agents.list[]`. \
                 The crew dispatch expects darkmux-namespaced agents to exist; \
                 run `darkmux crew sync` to create them from the manifests."
            )
        })?;

    // System prompt match
    let actual_prompt = entry
        .get("systemPromptOverride")
        .and_then(|s| s.as_str())
        .ok_or_else(|| anyhow!("agent `{agent_id}` has no systemPromptOverride"))?;
    if actual_prompt.trim() != expected_prompt.trim() {
        // After #147 the effective expected prompt may include
        // operator-identity injected from `~/.darkmux/identity.md`.
        // Mention both sources in the error so operators who edited
        // identity.md don't go debugging the role manifest by mistake.
        let identity_note = if load_operator_identity().is_some() {
            " (Effective prompt includes operator-identity from \
             `~/.darkmux/identity.md`; if you edited that file, run \
             sync to update.)"
        } else {
            ""
        };
        bail!(
            "agent `{agent_id}` systemPromptOverride drifted from the role manifest's `.md`. \
             Manifest expects {expected_chars} chars; openclaw has {actual_chars} chars. \
             Run `darkmux crew sync` to reconcile.{identity_note}",
            expected_chars = expected_prompt.len(),
            actual_chars = actual_prompt.len(),
        );
    }

    // Tool palette match (allow set)
    let expected_allow: Vec<&str> = role.tool_palette.allow.iter().map(|s| s.as_str()).collect();
    let actual_allow: Vec<&str> = entry
        .get("tools")
        .and_then(|t| t.get("allow"))
        .and_then(|a| a.as_array())
        .map(|arr| arr.iter().filter_map(|v| v.as_str()).collect())
        .unwrap_or_default();
    let expected_sorted = {
        let mut v = expected_allow.clone();
        v.sort();
        v
    };
    let actual_sorted = {
        let mut v = actual_allow.clone();
        v.sort();
        v
    };
    if expected_sorted != actual_sorted {
        bail!(
            "agent `{agent_id}` tool palette (allow) drifted from manifest. \
             Manifest expects {expected:?}; openclaw has {actual:?}. \
             Run `darkmux crew sync` to reconcile.",
            expected = expected_sorted,
            actual = actual_sorted,
        );
    }

    // Model pin match (#182). Sync-time enforcement (#160) writes the
    // pinned model into `agents.list[].model`; doctor's drift check
    // catches stale state on operator-explicit doctor run; this check
    // is the dispatch-time enforcement that closes the last silent-
    // fallback hole. An operator who edited <crew_root>/role-model-
    // pins.json (or pulled a new release with updated pins) but didn't
    // run `darkmux crew sync` would otherwise dispatch to the stale
    // model with no signal — the whole point of #160's "loud beats
    // quiet" principle is to make that scenario impossible.
    //
    // Pin-table load failures bail with the underlying error so the
    // operator sees what went wrong with their pin file; we don't
    // swallow that into a generic warning. Dispatch hot path = strict.
    let pin_table = crate::crew::pins::load_pins()
        .context("loading pin table for dispatch-time preflight (#182)")?;
    let expected_model = pin_table.pin_for(&role.id);
    let actual_model = entry.get("model").and_then(|m| m.as_str());
    if actual_model != Some(expected_model) {
        let actual_display = actual_model.unwrap_or("(no model field)");
        bail!(
            "agent `{agent_id}` pinned model drifted from the pin table. \
             Pin table expects `{expected_model}`; openclaw has `{actual_display}`. \
             Run `darkmux crew sync` to reconcile, then re-try dispatch."
        );
    }

    Ok(())
}

// ─────────────────────────────────────────────────────────────────────────
//   sync
// ─────────────────────────────────────────────────────────────────────────

#[derive(Debug, Default)]
pub struct SyncResult {
    /// Agents added.
    pub added: Vec<String>,
    /// Agents updated (entry existed but drifted; reconciled to match manifest).
    pub updated: Vec<String>,
    /// Agents already in sync — no change.
    pub unchanged: Vec<String>,
    /// Roles skipped because they have no `.md` prompt (can't dispatch them).
    pub skipped_no_prompt: Vec<String>,
}

#[derive(Debug)]
pub struct SyncOpts {
    pub dry_run: bool,
}

/// Reconcile openclaw's `agents.list[]` with the crew role manifests:
/// for every role that has a `.md` system prompt, ensure a
/// `darkmux/<role-id>` agent exists with the manifest-derived shape.
pub fn sync(opts: SyncOpts) -> Result<SyncResult> {
    let roles = load_roles().context("loading crew role manifests")?;
    let openclaw_path = default_openclaw_config();
    let mut config = read_openclaw_config(&openclaw_path)?;
    let openclaw_root = openclaw_path
        .parent()
        .ok_or_else(|| anyhow!("openclaw config path has no parent: {}", openclaw_path.display()))?
        .to_path_buf();

    let mut result = SyncResult::default();
    let mut config_modified = false;

    // Ensure agents.list exists.
    let agents = config
        .as_object_mut()
        .ok_or_else(|| anyhow!("openclaw config root is not an object"))?
        .entry("agents".to_string())
        .or_insert_with(|| json!({}));
    let agents_obj = agents
        .as_object_mut()
        .ok_or_else(|| anyhow!("`agents` is not an object"))?;
    let agents_list = agents_obj
        .entry("list".to_string())
        .or_insert_with(|| json!([]))
        .as_array_mut()
        .ok_or_else(|| anyhow!("`agents.list` is not an array"))?;

    for role in &roles {
        // Only sync roles that have an authored `.md` prompt — otherwise
        // there's nothing to dispatch with. Checks user dir first, then
        // bundled BUILTIN_ROLE_PROMPTS.
        let bare_prompt = match load_role_prompt(&role.id) {
            Some(p) => p,
            None => {
                result.skipped_no_prompt.push(role.id.clone());
                continue;
            }
        };
        // #147: inject operator-identity into the effective system prompt
        // when ~/.darkmux/identity.md exists. No-op when absent.
        let prompt = augment_prompt_with_identity(&bare_prompt);

        let agent_id = agent_id_for(&role.id);
        let (agent_dir, workspace_dir) = agent_dirs_for(&role.id, &openclaw_root);

        let expected_entry = build_agent_entry(role, &prompt, &agent_dir, &workspace_dir);
        let existing_pos = agents_list.iter().position(|a| {
            a.get("id").and_then(|s| s.as_str()) == Some(&agent_id)
        });

        match existing_pos {
            None => {
                if !opts.dry_run {
                    agents_list.push(expected_entry);
                    // Create the on-disk dirs the openclaw runtime will expect.
                    fs::create_dir_all(&agent_dir)
                        .with_context(|| format!("creating {}", agent_dir.display()))?;
                    fs::create_dir_all(&workspace_dir)
                        .with_context(|| format!("creating {}", workspace_dir.display()))?;
                    config_modified = true;
                }
                result.added.push(agent_id);
            }
            Some(i) => {
                if agents_list[i] != expected_entry {
                    if !opts.dry_run {
                        agents_list[i] = expected_entry;
                        fs::create_dir_all(&agent_dir)
                            .with_context(|| format!("creating {}", agent_dir.display()))?;
                        fs::create_dir_all(&workspace_dir)
                            .with_context(|| format!("creating {}", workspace_dir.display()))?;
                        config_modified = true;
                    }
                    result.updated.push(agent_id);
                } else {
                    result.unchanged.push(agent_id);
                }
            }
        }
    }

    if config_modified && !opts.dry_run {
        let pretty = serde_json::to_string_pretty(&config)?;
        fs::write(&openclaw_path, pretty + "\n")
            .with_context(|| format!("writing {}", openclaw_path.display()))?;
    }

    Ok(result)
}

fn build_agent_entry(role: &Role, prompt: &str, agent_dir: &Path, workspace: &Path) -> Value {
    let mut tools = Map::new();
    tools.insert(
        "allow".to_string(),
        Value::Array(role.tool_palette.allow.iter().cloned().map(Value::String).collect()),
    );
    if !role.tool_palette.deny.is_empty() {
        tools.insert(
            "deny".to_string(),
            Value::Array(role.tool_palette.deny.iter().cloned().map(Value::String).collect()),
        );
    }

    // Per-role model pin (#160). Reads the active pin table (user-dir
    // override → embedded default) and emits `agents.list[].model` so
    // openclaw routes the dispatch to the hired model regardless of
    // what's ambient-loaded. Pin-table read failures degrade to NO
    // model field — agent loses pin protection but the sync itself
    // doesn't fail; doctor's pin-drift check surfaces the gap.
    let pinned_model = crate::crew::pins::load_pins()
        .ok()
        .map(|t| t.pin_for(&role.id).to_string());

    let mut entry = json!({
        "id": agent_id_for(&role.id),
        "name": role.id,
        "agentDir": agent_dir.display().to_string(),
        "workspace": workspace.display().to_string(),
        "systemPromptOverride": prompt,
        "tools": Value::Object(tools),
        "skills": []
    });
    if let Some(model) = pinned_model {
        if let Value::Object(map) = &mut entry {
            map.insert("model".to_string(), Value::String(model));
        }
    }
    entry
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crew::types::{EscalationContract, Role, ToolPalette};
    use tempfile::TempDir;

    // ─── #247 PR-C build_route_payload ────────────────────────────────

    /// Auto-route payload — no target_machine; decision="auto-route".
    /// Topology UI uses these fields to render WHY the dispatch edge
    /// went to the tier-stream instead of running locally.
    #[test]
    fn build_route_payload_auto_route_has_no_target_and_auto_decision() {
        let p = build_route_payload("inference", Some("hub"), None);
        assert_eq!(p["role_tier"], "inference");
        assert_eq!(p["local_tier"], "hub");
        assert_eq!(p["target_machine"], serde_json::Value::Null);
        assert_eq!(p["decision"], "auto-route");
    }

    /// Pinned payload — operator-supplied target_machine;
    /// decision="pinned". The explicit-machine path still emits a
    /// dispatch route record so the audit trail captures that the
    /// operator made the decision (not the substrate).
    #[test]
    fn build_route_payload_pinned_has_target_and_pinned_decision() {
        let p = build_route_payload("inference", Some("hub"), Some("laptop"));
        assert_eq!(p["role_tier"], "inference");
        assert_eq!(p["local_tier"], "hub");
        assert_eq!(p["target_machine"], "laptop");
        assert_eq!(p["decision"], "pinned");
    }

    /// Local-tier-unknown variant — `DARKMUX_MACHINE_TIER` unset.
    /// Still emits a sensible payload (local_tier: null); operator
    /// can correlate with the doctor warning.
    #[test]
    fn build_route_payload_handles_unknown_local_tier() {
        let p = build_route_payload("inference", None, None);
        assert_eq!(p["role_tier"], "inference");
        assert_eq!(p["local_tier"], serde_json::Value::Null);
        assert_eq!(p["decision"], "auto-route");
    }

    // ─── completion_to_dispatch_result (Wave-E.6 #255) ────────────────

    fn completion(result_class: &str, payload: Option<serde_json::Value>) -> crate::fleet::CompletionResult {
        crate::fleet::CompletionResult {
            session_id: "test-sess".to_string(),
            result_class: result_class.to_string(),
            wall_ms: Some(1234),
            payload,
        }
    }

    #[test]
    fn completion_extracts_explicit_exit_code_from_payload() {
        // Worker emitted exit_code=42 (e.g. a build script's exit
        // code). Translation must surface it verbatim, NOT squash
        // to 1 via result_class.
        let c = completion(
            "error",
            Some(serde_json::json!({"result_class": "error", "exit_code": 42})),
        );
        let r = completion_to_dispatch_result(c);
        assert_eq!(r.exit_code, 42, "operator-facing exit code must match worker's");
        assert!(r.stdout.contains("exit_code=42"), "stdout includes exit code");
    }

    #[test]
    fn completion_extracts_zero_exit_code_even_on_ok() {
        let c = completion(
            "ok",
            Some(serde_json::json!({"result_class": "ok", "exit_code": 0})),
        );
        let r = completion_to_dispatch_result(c);
        assert_eq!(r.exit_code, 0);
    }

    #[test]
    fn completion_falls_back_to_zero_on_ok_without_exit_code() {
        // Payload present but no exit_code field; result_class=ok →
        // fallback 0.
        let c = completion("ok", Some(serde_json::json!({"result_class": "ok"})));
        let r = completion_to_dispatch_result(c);
        assert_eq!(r.exit_code, 0);
    }

    #[test]
    fn completion_falls_back_to_one_on_error_without_exit_code() {
        let c = completion("error", Some(serde_json::json!({"result_class": "error"})));
        let r = completion_to_dispatch_result(c);
        assert_eq!(r.exit_code, 1);
    }

    #[test]
    fn completion_falls_back_when_payload_absent() {
        let c = completion("error", None);
        let r = completion_to_dispatch_result(c);
        assert_eq!(r.exit_code, 1);
    }

    #[test]
    fn completion_passes_session_id_through() {
        let mut c = completion("ok", None);
        c.session_id = "mission-foo-sprint-bar-12345-0".to_string();
        let r = completion_to_dispatch_result(c);
        assert_eq!(r.session_id, "mission-foo-sprint-bar-12345-0");
        assert!(r.stdout.contains("mission-foo-sprint-bar-12345-0"));
    }

    #[test]
    fn completion_handles_negative_exit_code() {
        // SIGKILL-style exit codes can be negative (per std::process::ExitStatus).
        let c = completion(
            "error",
            Some(serde_json::json!({"result_class": "error", "exit_code": -9})),
        );
        let r = completion_to_dispatch_result(c);
        assert_eq!(r.exit_code, -9);
    }

    // ─── routing_decision (Wave-E.7 #255) ─────────────────────────────

    #[test]
    fn routing_decision_no_machine_is_local() {
        assert_eq!(
            routing_decision(None, Some("laptop")),
            RoutingDecision::Local { matches_was_explicit: false }
        );
        assert_eq!(
            routing_decision(None, None),
            RoutingDecision::Local { matches_was_explicit: false }
        );
    }

    #[test]
    fn routing_decision_machine_matches_local_is_local_explicit() {
        assert_eq!(
            routing_decision(Some("laptop"), Some("laptop")),
            RoutingDecision::Local { matches_was_explicit: true }
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

    fn sample_role() -> Role {
        Role {
            id: "code-reviewer".to_string(),
            description: "test".to_string(),
            capabilities: vec!["code-reviewing".to_string()],
            tool_palette: ToolPalette {
                allow: vec!["read".to_string(), "exec".to_string()],
                deny: vec!["edit".to_string(), "write".to_string()],
            },
            escalation_contract: EscalationContract::BailWithExplanation,
            prompt_path: None,
            tier: None,
        }
    }

    #[test]
    fn agent_id_uses_darkmux_namespace() {
        assert_eq!(agent_id_for("code-reviewer"), "darkmux/code-reviewer");
        assert_eq!(agent_id_for("analyst"), "darkmux/analyst");
    }

    #[test]
    #[serial_test::serial]
    fn licensed_adjacent_ack_passes_when_ack_file_exists() {
        let tmp = TempDir::new().unwrap();
        let prev = std::env::var("DARKMUX_ACK_DIR").ok();
        // Safety: tests mutate process env; the serial attribute keeps them
        // from racing each other.
        unsafe { std::env::set_var("DARKMUX_ACK_DIR", tmp.path()); }
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
        ).unwrap();
        let prev = std::env::var("DARKMUX_IDENTITY_PATH").ok();
        unsafe { std::env::set_var("DARKMUX_IDENTITY_PATH", &identity_path); }

        let augmented = augment_prompt_with_identity("# Role\n\nyou are X");
        // Role prompt preserved verbatim at the start.
        assert!(augmented.starts_with("# Role\n\nyou are X"), "got: {augmented}");
        // About-the-operator section appended.
        assert!(augmented.contains("## About the operator"), "got: {augmented}");
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
        unsafe { std::env::set_var("DARKMUX_IDENTITY_PATH", &identity_path); }

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
    fn extract_payload_text_pulls_first_text_from_envelope() {
        let stdout = r#"{"payloads":[{"text":"hello world","mediaUrl":null}],"meta":{}}"#;
        assert_eq!(extract_payload_text(stdout).as_deref(), Some("hello world"));
    }

    #[test]
    fn extract_payload_text_none_on_malformed_or_missing_fields() {
        assert!(extract_payload_text("not json").is_none());
        assert!(extract_payload_text("{}").is_none());
        assert!(extract_payload_text(r#"{"payloads":[]}"#).is_none());
        assert!(extract_payload_text(r#"{"payloads":[{"mediaUrl":null}]}"#).is_none());
    }

    #[test]
    #[serial_test::serial]
    fn augment_message_passes_through_when_no_sprint_id() {
        let result = augment_message_with_sprint_context(None, "do the thing").unwrap();
        assert_eq!(result, "do the thing");
    }

    #[test]
    #[serial_test::serial]
    fn augment_message_passes_through_when_sprint_has_no_deps() {
        let tmp = TempDir::new().unwrap();
        let prev = std::env::var("DARKMUX_CREW_DIR").ok();
        unsafe { std::env::set_var("DARKMUX_CREW_DIR", tmp.path()); }
        // Per-mission layout (#148): missions/<mission_id>/sprints/<sprint_id>.json
        let sprints_dir = tmp.path().join("missions").join("m").join("sprints");
        fs::create_dir_all(&sprints_dir).unwrap();
        fs::write(
            sprints_dir.join("solo-sprint.json"),
            r#"{"id":"solo-sprint","mission_id":"m","description":"d","status":"planned","depends_on":[],"created_ts":0}"#,
        ).unwrap();

        let result = augment_message_with_sprint_context(Some("solo-sprint"), "task body").unwrap();
        assert_eq!(result, "task body");

        unsafe {
            match prev {
                Some(v) => std::env::set_var("DARKMUX_CREW_DIR", v),
                None => std::env::remove_var("DARKMUX_CREW_DIR"),
            }
        }
    }

    #[test]
    #[serial_test::serial]
    fn augment_message_injects_parent_output_when_recorded() {
        let tmp = TempDir::new().unwrap();
        let prev = std::env::var("DARKMUX_CREW_DIR").ok();
        unsafe { std::env::set_var("DARKMUX_CREW_DIR", tmp.path()); }
        // Per-mission layout (#148): missions/<mission_id>/sprints/<sprint_id>.json
        let sprints_dir = tmp.path().join("missions").join("m").join("sprints");
        fs::create_dir_all(&sprints_dir).unwrap();
        // Parent + child sprint manifests.
        fs::write(
            sprints_dir.join("parent.json"),
            r#"{"id":"parent","mission_id":"m","description":"d","status":"done","depends_on":[],"created_ts":0}"#,
        ).unwrap();
        fs::write(
            sprints_dir.join("child.json"),
            r#"{"id":"child","mission_id":"m","description":"d","status":"planned","depends_on":["parent"],"created_ts":0}"#,
        ).unwrap();
        // Parent's recorded output co-located with manifests.
        fs::write(sprints_dir.join("parent-output.txt"), "parent did X and Y").unwrap();

        let result = augment_message_with_sprint_context(Some("child"), "task body").unwrap();
        assert!(result.contains("## Prior sprint outputs"), "got: {result}");
        assert!(result.contains("### parent"), "got: {result}");
        assert!(result.contains("parent did X and Y"), "got: {result}");
        assert!(result.contains("## Your task"), "got: {result}");
        assert!(result.contains("task body"), "got: {result}");

        unsafe {
            match prev {
                Some(v) => std::env::set_var("DARKMUX_CREW_DIR", v),
                None => std::env::remove_var("DARKMUX_CREW_DIR"),
            }
        }
    }

    #[test]
    #[serial_test::serial]
    fn augment_message_handles_mixed_recorded_and_missing_parents() {
        // Realistic case: child depends on two parents; one has recorded
        // output, the other doesn't. Child should get context for the
        // recorded one and the stderr should flag the missing one.
        let tmp = TempDir::new().unwrap();
        let prev = std::env::var("DARKMUX_CREW_DIR").ok();
        unsafe { std::env::set_var("DARKMUX_CREW_DIR", tmp.path()); }
        // Per-mission layout (#148): missions/<mission_id>/sprints/<sprint_id>.json
        let sprints_dir = tmp.path().join("missions").join("m").join("sprints");
        fs::create_dir_all(&sprints_dir).unwrap();
        fs::write(
            sprints_dir.join("parent-a.json"),
            r#"{"id":"parent-a","mission_id":"m","description":"d","status":"done","depends_on":[],"created_ts":0}"#,
        ).unwrap();
        fs::write(
            sprints_dir.join("parent-b.json"),
            r#"{"id":"parent-b","mission_id":"m","description":"d","status":"planned","depends_on":[],"created_ts":0}"#,
        ).unwrap();
        fs::write(
            sprints_dir.join("child.json"),
            r#"{"id":"child","mission_id":"m","description":"d","status":"planned","depends_on":["parent-a","parent-b"],"created_ts":0}"#,
        ).unwrap();
        // Only parent-a has a recorded output; co-located with manifests.
        fs::write(sprints_dir.join("parent-a-output.txt"), "parent-a finished X").unwrap();

        let result = augment_message_with_sprint_context(Some("child"), "child task").unwrap();
        assert!(result.contains("### parent-a"), "got: {result}");
        assert!(result.contains("parent-a finished X"), "got: {result}");
        // parent-b shouldn't show up in the context block — it has no
        // recorded output. The stderr line (not asserted here) flags it.
        assert!(!result.contains("### parent-b"), "got: {result}");
        assert!(result.contains("## Your task"), "got: {result}");
        assert!(result.contains("child task"), "got: {result}");

        unsafe {
            match prev {
                Some(v) => std::env::set_var("DARKMUX_CREW_DIR", v),
                None => std::env::remove_var("DARKMUX_CREW_DIR"),
            }
        }
    }

    #[test]
    #[serial_test::serial]
    fn augment_message_falls_back_to_bare_when_no_parent_outputs_recorded() {
        let tmp = TempDir::new().unwrap();
        let prev = std::env::var("DARKMUX_CREW_DIR").ok();
        unsafe { std::env::set_var("DARKMUX_CREW_DIR", tmp.path()); }
        // Per-mission layout (#148): missions/<mission_id>/sprints/<sprint_id>.json
        let sprints_dir = tmp.path().join("missions").join("m").join("sprints");
        fs::create_dir_all(&sprints_dir).unwrap();
        fs::write(
            sprints_dir.join("parent.json"),
            r#"{"id":"parent","mission_id":"m","description":"d","status":"planned","depends_on":[],"created_ts":0}"#,
        ).unwrap();
        fs::write(
            sprints_dir.join("child.json"),
            r#"{"id":"child","mission_id":"m","description":"d","status":"planned","depends_on":["parent"],"created_ts":0}"#,
        ).unwrap();
        // No parent-output.txt — dispatch proceeds with bare message.

        let result = augment_message_with_sprint_context(Some("child"), "task body").unwrap();
        assert_eq!(result, "task body");

        unsafe {
            match prev {
                Some(v) => std::env::set_var("DARKMUX_CREW_DIR", v),
                None => std::env::remove_var("DARKMUX_CREW_DIR"),
            }
        }
    }

    #[test]
    #[serial_test::serial]
    fn persist_sprint_output_writes_text_to_canonical_path() {
        let tmp = TempDir::new().unwrap();
        let prev = std::env::var("DARKMUX_CREW_DIR").ok();
        unsafe { std::env::set_var("DARKMUX_CREW_DIR", tmp.path()); }

        // Seed sprint manifest so load_sprint_by_id can resolve mission_id (#148).
        let sprints_dir = tmp.path().join("missions").join("m").join("sprints");
        fs::create_dir_all(&sprints_dir).unwrap();
        fs::write(
            sprints_dir.join("my-sprint.json"),
            r#"{"id":"my-sprint","mission_id":"m","description":"d","status":"planned","depends_on":[],"created_ts":0}"#,
        ).unwrap();

        let path = persist_sprint_output(Some("my-sprint"), "agent reply text").unwrap();
        let content = fs::read_to_string(&path).unwrap();
        assert_eq!(content, "agent reply text");
        // New layout: missions/<mission_id>/sprints/<sprint_id>-output.txt
        assert!(
            path.ends_with("missions/m/sprints/my-sprint-output.txt"),
            "got: {}", path.display()
        );

        // No-op for None sprint_id.
        assert!(persist_sprint_output(None, "ignored").is_none());
        // No-op for empty reply.
        assert!(persist_sprint_output(Some("my-sprint"), "").is_none());

        unsafe {
            match prev {
                Some(v) => std::env::set_var("DARKMUX_CREW_DIR", v),
                None => std::env::remove_var("DARKMUX_CREW_DIR"),
            }
        }
    }

    #[test]
    fn apply_workdir_override_noop_when_workdir_is_none() {
        let tmp = TempDir::new().unwrap();
        let workspace = tmp.path().join("workspace-darkmux-coder");
        // No --workdir; function should be a no-op + workspace stays empty.
        let outcome = apply_workdir_override(None, &workspace).unwrap();
        assert_eq!(outcome, WorkdirOutcome::NoChange);
        // Workspace was NOT auto-created either — no scope = no side effect.
        assert!(!workspace.exists());
    }

    #[test]
    fn apply_workdir_override_creates_symlink_to_existing_dir() {
        let tmp = TempDir::new().unwrap();
        let project = tmp.path().join("my-project");
        fs::create_dir(&project).unwrap();
        let workspace = tmp.path().join("workspace-darkmux-coder");

        let outcome = apply_workdir_override(Some(&project), &workspace).unwrap();
        assert!(matches!(outcome, WorkdirOutcome::Applied { previous_target: None }));

        let link = workspace.join("repo");
        assert!(link.is_symlink());
        let target = fs::read_link(&link).unwrap();
        // Symlink target is canonicalized to absolute.
        assert_eq!(target, project.canonicalize().unwrap());
    }

    #[test]
    fn apply_workdir_override_replaces_existing_symlink_and_reports_prev() {
        let tmp = TempDir::new().unwrap();
        let old_project = tmp.path().join("old-project");
        let new_project = tmp.path().join("new-project");
        fs::create_dir(&old_project).unwrap();
        fs::create_dir(&new_project).unwrap();
        let workspace = tmp.path().join("workspace-darkmux-coder");
        fs::create_dir(&workspace).unwrap();
        std::os::unix::fs::symlink(&old_project, workspace.join("repo")).unwrap();

        let outcome = apply_workdir_override(Some(&new_project), &workspace).unwrap();
        match outcome {
            WorkdirOutcome::Applied { previous_target } => {
                let prev = previous_target.expect("previous target should be captured");
                assert!(prev.ends_with("old-project"));
            }
            other => panic!("unexpected outcome: {other:?}"),
        }
        let link_target = fs::read_link(workspace.join("repo")).unwrap();
        assert_eq!(link_target, new_project.canonicalize().unwrap());
    }

    #[test]
    fn apply_workdir_override_refuses_when_workdir_does_not_exist() {
        let tmp = TempDir::new().unwrap();
        let workspace = tmp.path().join("workspace-darkmux-coder");
        let missing = tmp.path().join("does-not-exist");

        let err = apply_workdir_override(Some(&missing), &workspace).unwrap_err();
        let s = format!("{err:#}");
        assert!(s.contains("does not exist") || s.contains("not readable"), "got: {s}");
    }

    #[test]
    fn apply_workdir_override_refuses_when_repo_path_is_real_dir() {
        // If the workspace already has a REAL `repo` dir (not a symlink),
        // refuse to clobber. Operator-sovereign: don't trash files we
        // didn't put there.
        let tmp = TempDir::new().unwrap();
        let project = tmp.path().join("my-project");
        fs::create_dir(&project).unwrap();
        let workspace = tmp.path().join("workspace-darkmux-coder");
        fs::create_dir_all(&workspace).unwrap();
        let real_repo = workspace.join("repo");
        fs::create_dir(&real_repo).unwrap();
        fs::write(real_repo.join("OPERATOR_FILE.txt"), "do not clobber").unwrap();

        let err = apply_workdir_override(Some(&project), &workspace).unwrap_err();
        let s = format!("{err:#}");
        assert!(s.contains("not a symlink") || s.contains("refusing to clobber"), "got: {s}");
        // And the operator file is intact.
        assert!(real_repo.join("OPERATOR_FILE.txt").exists());
    }

    #[test]
    #[serial_test::serial]
    fn licensed_adjacent_ack_bails_when_no_tty_and_no_ack_file() {
        let tmp = TempDir::new().unwrap();
        let prev = std::env::var("DARKMUX_ACK_DIR").ok();
        // Safety: serialized.
        unsafe { std::env::set_var("DARKMUX_ACK_DIR", tmp.path()); }

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

    #[test]
    fn agent_dirs_use_nested_namespace_for_agentdir() {
        let root = Path::new("/tmp/.openclaw");
        let (agent_dir, workspace) = agent_dirs_for("code-reviewer", root);
        assert_eq!(agent_dir, Path::new("/tmp/.openclaw/agents/darkmux/code-reviewer/agent"));
        assert_eq!(workspace, Path::new("/tmp/.openclaw/workspace-darkmux-code-reviewer"));
    }

    #[test]
    fn build_agent_entry_includes_tools_and_prompt() {
        let role = sample_role();
        let agent_dir = PathBuf::from("/tmp/agent");
        let workspace = PathBuf::from("/tmp/ws");
        let entry = build_agent_entry(&role, "PROMPT", &agent_dir, &workspace);
        assert_eq!(entry["id"], "darkmux/code-reviewer");
        assert_eq!(entry["systemPromptOverride"], "PROMPT");
        assert_eq!(entry["tools"]["allow"], json!(["read", "exec"]));
        assert_eq!(entry["tools"]["deny"], json!(["edit", "write"]));
        assert_eq!(entry["skills"], json!([]));
    }

    #[test]
    fn build_agent_entry_omits_empty_deny() {
        let mut role = sample_role();
        role.tool_palette.deny.clear();
        let entry = build_agent_entry(&role, "PROMPT", Path::new("/x"), Path::new("/y"));
        // No "deny" key when the list is empty.
        assert!(entry["tools"].get("deny").is_none());
    }

    #[test]
    fn preflight_passes_when_config_matches() {
        // sample_role's id is "code-reviewer" which the shipped pin
        // table maps to `darkmux:qwen3.6-35b-a3b-turboquant-mlx` — the
        // config below has to include that model field for the new
        // dispatch-time pin check (#182) to pass.
        let role = sample_role();
        let config = json!({
            "agents": {
                "list": [
                    {
                        "id": "darkmux/code-reviewer",
                        "systemPromptOverride": "EXPECTED",
                        "model": "darkmux:qwen3.6-35b-a3b-turboquant-mlx",
                        "tools": {"allow": ["read", "exec"], "deny": ["edit", "write"]}
                    }
                ]
            }
        });
        assert!(preflight_check(&config, "darkmux/code-reviewer", &role, "EXPECTED").is_ok());
    }

    #[test]
    fn preflight_fails_when_pinned_model_drifts() {
        // sync-time + doctor-time + dispatch-time enforcement chain.
        // Operator edits pin file, doesn't run sync, then dispatches —
        // dispatch must bail loudly with the fix-it pointer rather
        // than silently routing to the stale model. (#182)
        let role = sample_role();
        let config = json!({
            "agents": {
                "list": [
                    {
                        "id": "darkmux/code-reviewer",
                        "systemPromptOverride": "EXPECTED",
                        "model": "darkmux:something-stale",
                        "tools": {"allow": ["read", "exec"], "deny": ["edit", "write"]}
                    }
                ]
            }
        });
        let err = preflight_check(&config, "darkmux/code-reviewer", &role, "EXPECTED").unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("pinned model drifted"), "got: {msg}");
        assert!(msg.contains("darkmux:something-stale"), "got: {msg}");
        assert!(msg.contains("darkmux crew sync"), "got: {msg}");
    }

    #[test]
    fn preflight_fails_when_model_field_absent() {
        // Pre-#160 openclaw.json files written by `darkmux crew sync`
        // had no `model` field at all (the field was added in #160).
        // An operator who upgraded darkmux but didn't re-sync would
        // hit this case — should bail loudly with the same fix-it.
        let role = sample_role();
        let config = json!({
            "agents": {
                "list": [
                    {
                        "id": "darkmux/code-reviewer",
                        "systemPromptOverride": "EXPECTED",
                        "tools": {"allow": ["read", "exec"], "deny": ["edit", "write"]}
                    }
                ]
            }
        });
        let err = preflight_check(&config, "darkmux/code-reviewer", &role, "EXPECTED").unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("pinned model drifted"), "got: {msg}");
        assert!(msg.contains("(no model field)"), "got: {msg}");
        assert!(msg.contains("darkmux crew sync"), "got: {msg}");
    }

    #[test]
    fn preflight_fails_when_agent_missing() {
        let role = sample_role();
        let config = json!({"agents": {"list": []}});
        let err = preflight_check(&config, "darkmux/code-reviewer", &role, "EXPECTED").unwrap_err();
        assert!(err.to_string().contains("not found in openclaw"));
    }

    #[test]
    fn preflight_fails_when_prompt_drifts() {
        let role = sample_role();
        let config = json!({
            "agents": {
                "list": [
                    {
                        "id": "darkmux/code-reviewer",
                        "systemPromptOverride": "STALE",
                        "tools": {"allow": ["read", "exec"]}
                    }
                ]
            }
        });
        let err = preflight_check(&config, "darkmux/code-reviewer", &role, "EXPECTED").unwrap_err();
        assert!(err.to_string().contains("systemPromptOverride drifted"));
    }

    #[test]
    fn preflight_fails_when_tool_palette_drifts() {
        let role = sample_role();
        let config = json!({
            "agents": {
                "list": [
                    {
                        "id": "darkmux/code-reviewer",
                        "systemPromptOverride": "EXPECTED",
                        "tools": {"allow": ["read", "edit"]}
                    }
                ]
            }
        });
        let err = preflight_check(&config, "darkmux/code-reviewer", &role, "EXPECTED").unwrap_err();
        assert!(err.to_string().contains("tool palette"));
    }

    #[test]
    fn sync_adds_missing_agent_to_empty_config() {
        let tmp = TempDir::new().unwrap();
        let openclaw_path = tmp.path().join("openclaw.json");
        fs::write(&openclaw_path, "{}").unwrap();

        // Test the build_agent_entry logic directly — sync() itself depends
        // on load_roles() which reads on-disk manifests; we test the
        // single-role write path here, then exercise the integration path
        // via a CLI test (in tests/cli.rs).
        let role = sample_role();
        let mut config: Value = serde_json::from_str(&fs::read_to_string(&openclaw_path).unwrap()).unwrap();
        let agents = config.as_object_mut().unwrap().entry("agents".to_string()).or_insert_with(|| json!({}));
        let list = agents.as_object_mut().unwrap().entry("list".to_string()).or_insert_with(|| json!([]));
        let entry = build_agent_entry(&role, "PROMPT", Path::new("/x"), Path::new("/y"));
        list.as_array_mut().unwrap().push(entry);

        assert_eq!(
            config["agents"]["list"][0]["id"],
            "darkmux/code-reviewer"
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
        assert_eq!(parts.len(), 2, "expected <micros>-<counter>, got {suffix:?}");
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

    // ─── #89: watched-state snapshot ───────────────────────────────────────
    // (std::io::Write is already imported at the top of the module.)

    fn write_file(path: &Path, contents: &[u8]) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        let mut f = fs::File::create(path).unwrap();
        f.write_all(contents).unwrap();
    }

    #[test]
    fn snapshot_reports_unreachable_for_missing_path() {
        let s = snapshot_watched_path(Path::new("/no/such/path/anywhere"));
        assert!(s.unreachable);
        assert!(s.files.is_empty());
    }

    #[test]
    fn snapshot_walks_top_level_files() {
        let tmp = TempDir::new().unwrap();
        write_file(&tmp.path().join("a.txt"), b"AAAAA");      // 5 bytes
        write_file(&tmp.path().join("b.txt"), b"BB");          // 2 bytes
        write_file(&tmp.path().join("c.txt"), b"CCCCCCCCCC");  // 10 bytes

        let s = snapshot_watched_path(tmp.path());
        assert!(!s.unreachable);
        assert_eq!(s.files.len(), 3);
        // Sort order: largest first.
        assert_eq!(s.files[0].size, 10);
        assert_eq!(s.files[1].size, 5);
        assert_eq!(s.files[2].size, 2);
    }

    #[test]
    fn snapshot_walks_one_level_into_subdirs() {
        let tmp = TempDir::new().unwrap();
        write_file(&tmp.path().join("top.txt"), b"top");
        write_file(&tmp.path().join("sub").join("nested.txt"), b"nested");
        write_file(
            &tmp.path().join("sub").join("deeper").join("deep.txt"),
            b"too-deep",
        );

        let s = snapshot_watched_path(tmp.path());
        let names: std::collections::HashSet<String> = s
            .files
            .iter()
            .map(|f| f.path.file_name().unwrap().to_str().unwrap().to_string())
            .collect();
        assert!(names.contains("top.txt"));
        assert!(names.contains("nested.txt"));
        // Recursion stops at one level deep — `deep.txt` is two levels in.
        assert!(!names.contains("deep.txt"), "should not recurse beyond one level");
    }

    #[test]
    fn snapshot_excludes_openclaw_noise() {
        let tmp = TempDir::new().unwrap();
        // Real output file the operator cares about.
        write_file(&tmp.path().join("output.md"), b"real content");
        // Openclaw bookkeeping that changes every dispatch — should NOT
        // appear in the operator-facing summary.
        write_file(
            &tmp.path().join("abc-123.trajectory.jsonl"),
            b"{\"type\":\"event\"}",
        );
        write_file(&tmp.path().join("BOOTSTRAP.md"), b"workspace bootstrap");
        write_file(&tmp.path().join("HEARTBEAT.md"), b"heartbeat");
        write_file(&tmp.path().join("AGENTS.md"), b"agents list");

        let s = snapshot_watched_path(tmp.path());
        let names: Vec<String> = s
            .files
            .iter()
            .map(|f| f.path.file_name().unwrap().to_str().unwrap().to_string())
            .collect();
        assert_eq!(names, vec!["output.md".to_string()]);
    }

    #[test]
    fn snapshot_skips_symlinked_subdirs() {
        let tmp = TempDir::new().unwrap();
        let real = TempDir::new().unwrap();
        // Real outside content — a symlinked subdir to it should NOT walk.
        write_file(&real.path().join("should-not-appear.txt"), b"shadow");
        // Symlink from `tmp/repo` -> `real.path()`. Skips into it would be
        // unbounded across the operator's actual source tree.
        #[cfg(unix)]
        std::os::unix::fs::symlink(real.path(), tmp.path().join("repo")).unwrap();
        // A regular top-level file in tmp — should appear.
        write_file(&tmp.path().join("plain.txt"), b"x");

        let s = snapshot_watched_path(tmp.path());
        let names: Vec<String> = s
            .files
            .iter()
            .map(|f| f.path.file_name().unwrap().to_str().unwrap().to_string())
            .collect();
        assert!(names.contains(&"plain.txt".to_string()));
        assert!(!names.contains(&"should-not-appear.txt".to_string()),
            "must not descend into symlinked subdir; got {names:?}");
    }

    #[test]
    fn is_openclaw_noise_classifies_known_files() {
        // Workspace bootstrap markdowns
        assert!(is_openclaw_noise(Path::new("/x/AGENTS.md")));
        assert!(is_openclaw_noise(Path::new("/x/BOOTSTRAP.md")));
        assert!(is_openclaw_noise(Path::new("/x/HEARTBEAT.md")));
        assert!(is_openclaw_noise(Path::new("/x/USER.md")));
        // Session bookkeeping
        assert!(is_openclaw_noise(Path::new(
            "/x/abc-123.trajectory.jsonl"
        )));
        assert!(is_openclaw_noise(Path::new("/x/sessions.json")));
        // Real operator content stays
        assert!(!is_openclaw_noise(Path::new("/x/output.md")));
        assert!(!is_openclaw_noise(Path::new("/x/decisions.md")));
        assert!(!is_openclaw_noise(Path::new("/x/deck-revised.pptx")));
        assert!(!is_openclaw_noise(Path::new("/x/2026-05-14.jsonl"))); // flow records aren't noise
    }

    #[test]
    fn default_workspace_for_role_uses_namespace_convention() {
        let p = default_workspace_for_role("code-reviewer");
        // Path ends with the expected segment regardless of $HOME's shape.
        let s = p.to_string_lossy();
        assert!(
            s.ends_with(".openclaw/workspace-darkmux-code-reviewer"),
            "unexpected workspace path: {s}",
        );
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

    // ─── build_dispatch_record (Sprint 2 of #104) ──────────────────────────

    #[test]
    fn dispatch_record_carries_role_id_session_and_local_tier() {
        let rec = build_dispatch_record(
            crate::flow::Level::Info,
            "dispatch start",
            "coder",
            "crew-dispatch-coder-12345-1",
            Some("darkmux:qwen3.6-35b-a3b"),
        );
        assert_eq!(rec.action, "dispatch start");
        assert_eq!(rec.handle, "coder");
        assert_eq!(rec.session_id.as_deref(), Some("crew-dispatch-coder-12345-1"));
        assert_eq!(rec.source.as_deref(), Some("crew_dispatch"));
        assert_eq!(rec.model.as_deref(), Some("darkmux:qwen3.6-35b-a3b"));
        assert!(matches!(rec.tier, crate::flow::Tier::Local));
        assert!(matches!(rec.stage, crate::flow::Stage::Dispatch));
        assert!(matches!(rec.category, crate::flow::Category::Work));
        // sprint_id is None for dispatch records — crew dispatch is a
        // lower-level concept than sprint, so the dispatcher doesn't
        // assume a sprint context. The viewer joins via session_id.
        assert!(rec.sprint_id.is_none());
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
            crate::flow::Level::Info,
            "dispatch start",
            "coder",
            "session-no-model",
            None,
        );
        assert!(rec.model.is_none());
        let json = serde_json::to_string(&rec).unwrap();
        assert!(!json.contains("\"model\""), "absent field should serialize away: {json}");
    }

    #[test]
    fn dispatch_record_error_level_serializes_distinctly() {
        // Error-level records render differently in the viewer (red tag,
        // not green). Lock the error level on dispatch_error so the
        // failure path is visually distinct from completion.
        let ok = build_dispatch_record(
            crate::flow::Level::Info,
            "dispatch complete",
            "coder",
            "session-abc",
            Some("darkmux:foo"),
        );
        let err = build_dispatch_record(
            crate::flow::Level::Error,
            "dispatch error",
            "coder",
            "session-abc",
            Some("darkmux:foo"),
        );
        assert!(matches!(ok.level, crate::flow::Level::Info));
        assert!(matches!(err.level, crate::flow::Level::Error));
        // Same session_id so the viewer pairs them — this is the contract
        // that makes computeDispatchDurations() work for the failure path
        // too (an erroring dispatch still has a wall-clock arc).
        assert_eq!(ok.session_id, err.session_id);
    }
}
