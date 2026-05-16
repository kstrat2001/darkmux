//! Dispatch a crew member (role) for a single turn.
//!
//! This is the operator-facing entry point that ties the crew schema
//! (`templates/builtin/crew/roles/<id>.{json,md}`) to the actual runtime
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

use crate::crew::loader::{load_role_prompt, load_roles};
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
/// (e.g., for tests).
fn default_openclaw_config() -> PathBuf {
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
        "at templates/builtin/crew/roles/{role_id}.md in the darkmux source"
    );
    eprintln!(
        "(or your override at ~/.darkmux/crew/roles/{role_id}.md if set)."
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
    // 0. Pre-flight: nudge the operator if the daemon isn't up. The
    //    dispatch will still write flow records to disk, but they
    //    won't be observable in the viewer until the daemon comes up.
    //    Non-blocking; the dispatch proceeds either way (#104 S3).
    crate::serve::nudge_if_daemon_unreachable("crew dispatch");

    // 1. Load the role + its .md prompt
    let role = load_role_or_bail(&opts.role_id)?;
    let prompt = role_prompt_or_bail(&role)?;
    let agent_id = agent_id_for(&opts.role_id);

    // 1.5. Licensed-adjacent ACK gate. For roles whose prompts operate
    //      in domains regulated by professional licensure (health, law,
    //      fitness), require an operator acknowledgment on first dispatch.
    //      The prompts encode the boundary at runtime; this gate makes the
    //      same boundary visible to the operator at the CLI surface.
    require_licensed_adjacent_ack(&opts.role_id)
        .context("licensed-adjacent role dispatch requires acknowledgment")?;

    // 2. Pre-flight against openclaw config
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
    let _ = crate::flow::record(build_dispatch_record(
        crate::flow::Level::Info,
        "dispatch start",
        &opts.role_id,
        &resolved_session_id,
        resolved_model.as_deref(),
    ));

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
    cmd.args(["--message", &opts.message]);

    let output_result = cmd
        .output()
        .with_context(|| format!("running `openclaw agent {agent_id}`"));

    // Emit the dispatch end record BEFORE propagating any error or
    // returning Ok — emission must reflect both success and failure paths
    // so the viewer never sees a dangling start with no terminal event.
    let (action, level) = match &output_result {
        Ok(o) if o.status.success() => ("dispatch complete", crate::flow::Level::Info),
        _ => ("dispatch error", crate::flow::Level::Error),
    };
    let _ = crate::flow::record(build_dispatch_record(
        level,
        action,
        &opts.role_id,
        &resolved_session_id,
        resolved_model.as_deref(),
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

    Ok(DispatchResult {
        exit_code: output.status.code().unwrap_or(-1),
        stdout: String::from_utf8_lossy(&output.stdout).to_string(),
        stderr: String::from_utf8_lossy(&output.stderr).to_string(),
        session_id: resolved_session_id,
        watched_state,
    })
}

/// Build a flow record for a dispatch lifecycle event (`dispatch start`,
/// `dispatch complete`, `dispatch error`). All three share the same
/// session_id so the viewer pairs start↔end into a single wall-clock
/// arc per dispatch. `handle` is the role id (operator-readable label);
/// `session_id` is the full openclaw session identifier; `model` is the
/// resolved LMStudio model id (best-effort — `None` when the openclaw
/// config can't be read or no model is pinned for this agent).
pub(crate) fn build_dispatch_record(
    level: crate::flow::Level,
    action: &str,
    role_id: &str,
    session_id: &str,
    model: Option<&str>,
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
             `templates/builtin/crew/roles/{}.md` (or override at \
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
        bail!(
            "agent `{agent_id}` systemPromptOverride drifted from the role manifest's `.md`. \
             Manifest expects {expected_chars} chars; openclaw has {actual_chars} chars. \
             Run `darkmux crew sync` to reconcile.",
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
        let prompt = match load_role_prompt(&role.id) {
            Some(p) => p,
            None => {
                result.skipped_no_prompt.push(role.id.clone());
                continue;
            }
        };

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
    json!({
        "id": agent_id_for(&role.id),
        "name": role.id,
        "agentDir": agent_dir.display().to_string(),
        "workspace": workspace.display().to_string(),
        "systemPromptOverride": prompt,
        "tools": Value::Object(tools),
        "skills": []
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crew::types::{EscalationContract, Role, ToolPalette};
    use tempfile::TempDir;

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
        assert!(preflight_check(&config, "darkmux/code-reviewer", &role, "EXPECTED").is_ok());
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
