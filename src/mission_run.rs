//! `darkmux mission run` — the local dispatch-to-PR loop, up to the gate.
//!
//! `mission dispatch` (see `cmd_mission_dispatch`) fans a mission's ready
//! sprints onto the global Redis work queue for the fleet to claim. `mission
//! run` is its **local, synchronous, single-sprint sibling**: it owns the
//! mechanical per-sprint loop on THIS machine —
//!
//!   1. create an isolated git worktree for the sprint,
//!   2. dispatch the coder role into it (sprint-bound, internal runtime),
//!   3. run the local `code-reviewer` QA against the worktree diff,
//!   4. surface the coder result + tokens-off-meter + QA findings,
//!   5. **stop at the gate** — worktree left in place, nothing committed.
//!
//! Why it stops: adjudicating the QA findings and deciding to merge are
//! judgment/gate steps that belong to the frontier orchestrator + operator,
//! never to a CLI verb (operator sovereignty, #44; never-auto-merge). `mission
//! run` tees everything up so sign-off is one follow-on step — `darkmux mission
//! ship <id> --sprint <sprint-id>` (PR2) does the commit → PR → CI → merge →
//! teardown after the operator/frontier signs off. This verb kills the
//! worktree-dance + manual-token-tally frictions (#782) without taking the
//! merge decision out of the operator's hands.

use anyhow::{bail, Context, Result};
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::crew;
use crate::fleet;
use crate::flow;
use darkmux_types::style;

/// Emit a mission-run lifecycle flow record so the frontier orchestrator can
/// **track** the run as a unit on the stream (`mission.run.*`), bracketing the
/// inner coder + reviewer dispatch records. This is the "track" half of the
/// hybrid contract: the verb runs the mechanical loop, but every step is
/// observable (tail the stream / watch the viewer), the gate is a hard pause,
/// and `mission abort` is the explicit teardown — the operator/frontier stays
/// in control of a CLI verb. Best-effort (observability, never loop-failing).
fn emit_run_record(
    level: flow::Level,
    action: &str,
    mission_id: &str,
    sprint_id: &str,
    session_id: &str,
    payload: serde_json::Value,
) {
    let _ = flow::record(crew::dispatch::build_dispatch_record_with_payload(
        level,
        action,
        "mission-run",
        session_id,
        None,
        Some(mission_id),
        Some(sprint_id),
        Some(payload),
    ));
}

/// Resolve the base directory holding per-sprint worktrees:
/// `~/.darkmux/worktrees` (HOME-less fallback `/tmp/darkmux/worktrees`).
/// Outside the main working tree by design — git refuses a worktree nested
/// inside another, and a stable, discoverable location lets `mission ship`
/// recompute the path without recording it in mission state.
fn worktrees_base() -> PathBuf {
    dirs::home_dir()
        .map(|h| h.join(".darkmux").join("worktrees"))
        .unwrap_or_else(|| PathBuf::from("/tmp/darkmux/worktrees"))
}

/// The MAIN working tree of the current repository, resolved identically
/// whether invoked from the main checkout or from inside a linked worktree.
///
/// `mission run` creates the sprint worktree off this repo and `mission ship`
/// recomputes that worktree's path from `(repo-name, sprint)` — both must
/// agree on the repo name. `git rev-parse --show-toplevel` returns the
/// *current* working tree, which inside a mission's linked worktree is the
/// sprint dir (basename = sprint id, NOT the repo name); using it made
/// `mission ship` from inside a worktree recompute a different (wrong) path
/// than `mission run` created (#846). The first `worktree` entry of
/// `git worktree list --porcelain` is always the main working tree, so it
/// yields the stable repo name AND a valid dir to run worktree teardown from
/// (git refuses to remove the worktree you are standing in).
fn repo_root() -> Result<PathBuf> {
    let out = Command::new("git")
        .args(["worktree", "list", "--porcelain"])
        .output()
        .context("running `git worktree list --porcelain`")?;
    if !out.status.success() {
        bail!(
            "`darkmux mission` must be invoked from inside a git repository \
             (git worktree list failed). cd into the engagement's repo first."
        );
    }
    parse_main_worktree(&String::from_utf8_lossy(&out.stdout))
        .ok_or_else(|| anyhow::anyhow!("git worktree list reported no main working tree"))
}

/// Parse the main working tree path from `git worktree list --porcelain`
/// output: the first `worktree <path>` line. Pure, for testability (#846).
fn parse_main_worktree(porcelain: &str) -> Option<PathBuf> {
    porcelain
        .lines()
        .find_map(|l| l.strip_prefix("worktree "))
        // `.lines()` already strips the trailing `\r\n`; deliberately NOT
        // `.trim()` — for an UNQUOTED path git emits it raw, so a worktree dir
        // whose name legitimately ends in whitespace must round-trip intact.
        .filter(|p| !p.is_empty())
        .map(decode_git_path)
}

/// Decode git's C-quoted porcelain path form (#907). When a path contains
/// bytes git considers "unusual" (control chars, high/non-ASCII bytes, `"`,
/// `\`) and `core.quotePath` is on (the default), git wraps the path in
/// double-quotes with C-style escapes (`\t`, `\n`, `\r`, `\"`, `\\`, and octal
/// `\NNN` for raw bytes). An unquoted path is returned verbatim, preserving
/// any legitimate trailing whitespace. Without this, `mission run`/`ship`/
/// `abort` would point at the literal quoted string for repos at non-ASCII or
/// special-char paths (fails loudly, no corruption — but breaks the verb).
fn decode_git_path(raw: &str) -> PathBuf {
    if raw.len() < 2 || !raw.starts_with('"') || !raw.ends_with('"') {
        return PathBuf::from(raw);
    }
    let bytes = &raw.as_bytes()[1..raw.len() - 1];
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'\\' && i + 1 < bytes.len() {
            match bytes[i + 1] {
                b'n' => { out.push(b'\n'); i += 2; }
                b't' => { out.push(b'\t'); i += 2; }
                b'r' => { out.push(b'\r'); i += 2; }
                b'"' => { out.push(b'"'); i += 2; }
                b'\\' => { out.push(b'\\'); i += 2; }
                b'0'..=b'7' => {
                    // Up to 3 octal digits → one raw byte.
                    let mut val: u32 = 0;
                    let mut n = 0;
                    let mut j = i + 1;
                    while n < 3 && j < bytes.len() && bytes[j].is_ascii_digit() && bytes[j] < b'8' {
                        val = val * 8 + u32::from(bytes[j] - b'0');
                        j += 1;
                        n += 1;
                    }
                    out.push(val as u8);
                    i = j;
                }
                // Unknown escape — keep the backslash literally.
                _ => { out.push(bytes[i]); i += 1; }
            }
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    // git octal-escapes raw UTF-8 bytes, so the reassembled bytes are UTF-8 in
    // the common case; fall back to the OS-native byte path on unix otherwise.
    match String::from_utf8(out) {
        Ok(s) => PathBuf::from(s),
        Err(e) => {
            #[cfg(unix)]
            {
                use std::os::unix::ffi::OsStringExt;
                PathBuf::from(std::ffi::OsString::from_vec(e.into_bytes()))
            }
            #[cfg(not(unix))]
            {
                PathBuf::from(String::from_utf8_lossy(&e.into_bytes()).into_owned())
            }
        }
    }
}

/// Authoritative answer to "did this PR merge?" vs. "couldn't tell". The
/// `Unknown` arm is load-bearing: if the verifying `gh pr view` itself blips
/// (5xx / expired token / network), collapsing that into "not merged" would
/// let the caller assert a falsehood and re-create the exact #844 silent drift
/// one step later. So the caller distinguishes the three.
enum MergeState {
    Merged,
    NotMerged,
    Unknown,
}

/// Whether the PR at `pr_url` merged on the remote. Distinguishes a real
/// `gh pr merge` failure from gh's local post-merge sync failing under the
/// mission worktree layout (#844): gh performs the squash-merge + remote-branch
/// deletion via the API BEFORE its local git ops, so a non-zero exit can still
/// mean "merged".
///
/// Takes the PR **URL** specifically (not a branch / number): `--delete-branch`
/// removes the head branch, after which a branch selector is unresolvable and a
/// bare number is sensitive to which repo `dir` points at — a URL pins the PR
/// identity unambiguously. A gh error or empty state is reported as `Unknown`
/// (could-not-reach), never silently as `NotMerged`.
fn pr_merge_state(dir: &Path, pr_url: &str) -> MergeState {
    let out = match Command::new("gh")
        .current_dir(dir)
        .args(["pr", "view", pr_url, "--json", "state", "-q", ".state"])
        .output()
    {
        Ok(o) if o.status.success() => o,
        _ => return MergeState::Unknown,
    };
    match String::from_utf8_lossy(&out.stdout).trim() {
        "MERGED" => MergeState::Merged,
        "" => MergeState::Unknown,
        _ => MergeState::NotMerged, // OPEN / CLOSED-unmerged
    }
}

/// Deterministic worktree path for a sprint: `<base>/<repo-name>/<sprint-id>`.
/// Recomputable by `mission ship` from the same (repo, sprint) inputs.
fn worktree_path(repo_root: &Path, sprint_id: &str) -> PathBuf {
    let repo_name = repo_root
        .file_name()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| "repo".to_string());
    worktrees_base().join(repo_name).join(sprint_id)
}

/// Branch name for a sprint's worktree. The sprint id is already charset-
/// validated (`fleet::validate_identifier`) before this is called, so it's a
/// safe git ref component; we prefix `darkmux/` to namespace the branch and
/// keep it recognizable as a darkmux-managed worktree branch.
fn branch_name(sprint_id: &str) -> String {
    format!("darkmux/{sprint_id}")
}

/// (#816) Conventions-aware branch name: the repo's `branch_template`
/// (expanded + validated as a safe git ref) when present, else the
/// darkmux default. ALL THREE verbs (run / abort / ship) must resolve the
/// branch through this one fn so they always agree on the name. A
/// template that can't expand (ticketless mission) or expands to an
/// invalid ref falls back loudly-but-softly to the default.
fn conventions_branch(
    sprint: &crew::types::Sprint,
    mission: &crew::types::Mission,
    conv: Option<&crate::conventions::Conventions>,
) -> String {
    let default = branch_name(&sprint.id);
    let Some(template) = conv.and_then(|c| c.branch_template.as_deref()) else {
        return default;
    };
    let vars = crate::conventions::Vars {
        ticket: mission.ticket.as_deref(),
        sprint: &sprint.id,
        mission: &mission.id,
        subject: "",
    };
    match crate::conventions::expand(template, &vars) {
        Some(b) if crate::conventions::valid_branch(&b) => b,
        Some(b) => {
            eprintln!(
                "darkmux: warning — conventions branch_template expanded to an invalid git ref ({b:?}); using `{default}`"
            );
            default
        }
        None => {
            eprintln!(
                "darkmux: warning — conventions branch_template references {{ticket}} but mission `{}` has no ticket (set one: `mission propose --ticket <ID>`); using `{default}`",
                mission.id
            );
            default
        }
    }
}

/// Choose which sprint to run. Explicit `--sprint` wins (validated to belong
/// to the mission and not be terminal). Otherwise auto-select the single
/// ready sprint (`depends_on` empty + `Planned`); 0 or >1 is ambiguous and
/// bails with guidance rather than guessing — the operator stays in the loop.
fn select_sprint(
    sprints: &[crew::types::Sprint],
    mission_id: &str,
    explicit: Option<&str>,
) -> Result<crew::types::Sprint> {
    use crew::types::SprintStatus;

    if let Some(id) = explicit {
        let s = sprints
            .iter()
            .find(|s| s.id == id)
            .ok_or_else(|| anyhow::anyhow!("sprint `{id}` not found"))?;
        if s.mission_id != mission_id {
            bail!(
                "sprint `{id}` belongs to mission `{}`, not `{mission_id}`",
                s.mission_id
            );
        }
        if matches!(s.status, SprintStatus::Complete) {
            bail!("sprint `{id}` is already Complete (terminal) — nothing to run");
        }
        return Ok(s.clone());
    }

    let ready: Vec<&crew::types::Sprint> = sprints
        .iter()
        .filter(|s| s.mission_id == mission_id && s.depends_on.is_empty())
        .filter(|s| matches!(s.status, SprintStatus::Planned))
        .collect();

    match ready.as_slice() {
        [] => bail!(
            "mission `{mission_id}` has no ready sprint to run (need a Planned sprint with \
             no unmet dependencies). Pass `--sprint <id>` to target one explicitly, or check \
             `darkmux mission show {mission_id}`."
        ),
        [one] => Ok((*one).clone()),
        many => {
            let ids: Vec<&str> = many.iter().map(|s| s.id.as_str()).collect();
            bail!(
                "mission `{mission_id}` has {} ready sprints ({}). `mission run` does one sprint \
                 at a time — pass `--sprint <id>` to choose.",
                many.len(),
                ids.join(", ")
            )
        }
    }
}

/// Create the git worktree for this sprint, branching off `base`. If the
/// worktree path already exists (a prior `mission run` for the same sprint
/// that wasn't shipped/torn down), bail with a pointer rather than clobbering
/// — the operator decides whether to resume, ship, or `git worktree remove`.
fn add_worktree(repo_root: &Path, wt_path: &Path, branch: &str, base: &str) -> Result<()> {
    if wt_path.exists() {
        bail!(
            "worktree already exists at {} — a previous `mission run` for this sprint hasn't \
             been shipped or torn down. Inspect it, run `darkmux mission ship` to finish, or \
             `git worktree remove {}` to discard.",
            wt_path.display(),
            wt_path.display()
        );
    }
    if let Some(parent) = wt_path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating worktree parent dir {}", parent.display()))?;
    }
    let out = Command::new("git")
        .current_dir(repo_root)
        .args([
            "worktree",
            "add",
            "-b",
            branch,
            &wt_path.to_string_lossy(),
            base,
        ])
        .output()
        .context("running `git worktree add`")?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        bail!(
            "git worktree add failed (base `{base}`, branch `{branch}`): {}",
            stderr.trim()
        );
    }
    Ok(())
}

/// `darkmux mission run` entry. Returns the process exit code:
/// `0` clean (coder ran, QA clean or flags-only), `1` coder dispatch error,
/// `2` QA found blockers (operator must resolve before ship),
/// `3` QA could not run (reviewer dispatch failed — manual review required).
#[allow(clippy::too_many_arguments)]
pub fn run(
    mission_id: &str,
    sprint_id: Option<&str>,
    role: &str,
    image: Option<&str>,
    base: &str,
    timeout_seconds: u32,
) -> Result<i32> {
    use crew::loader::{load_missions, load_roles, load_sprints};

    // CLI-boundary charset validation — these flow into branch names,
    // worktree paths, session ids, and flow records.
    fleet::validate_identifier("mission_id", mission_id)?;
    fleet::validate_identifier("role_id", role)?;
    if let Some(s) = sprint_id {
        fleet::validate_identifier("--sprint", s)?;
    }

    // 1. Validate the mission + role exist.
    let missions = load_missions()?;
    let mission = missions
        .iter()
        .find(|m| m.id == mission_id)
        .ok_or_else(|| {
            anyhow::anyhow!(
                "mission `{mission_id}` not found. Run `darkmux mission propose` first \
                 or check the id."
            )
        })?;
    if !matches!(mission.status, crew::types::MissionStatus::Active) {
        eprintln!(
            "{}",
            style::warn(&format!(
                "darkmux mission run: warning — mission `{mission_id}` status is {:?}, not Active. \
                 Proceeding anyway (operator-explicit override).",
                mission.status
            ))
        );
    }
    let roles = load_roles()?;
    if !roles.iter().any(|r| r.id == role) {
        bail!("role `{role}` not found (check `darkmux crew roles`)");
    }

    // 2. Select the sprint to run.
    let sprints = load_sprints()?;
    let sprint = select_sprint(&sprints, mission_id, sprint_id)?;

    // Shared session id for every record this run emits — the frontier
    // tails the stream on this id to track the run end to end.
    let session_id = format!("mission-run-{}-{}", mission_id, sprint.id);

    // 3. Set up the isolated worktree.
    let root = repo_root()?;
    let wt_path = worktree_path(&root, &sprint.id);
    let conv = crate::conventions::load(&root);
    let branch = conventions_branch(&sprint, mission, conv.as_ref());

    println!(
        "{}",
        style::header(&format!(
            "▶ mission run — {} · sprint {}",
            mission_id, sprint.id
        ))
    );
    println!("  {}  {}", style::dim("mission:"), mission.description);
    println!("  {}   {}", style::dim("sprint:"), sprint.description);
    println!(
        "  {} {} {} {}",
        style::dim("worktree:"),
        wt_path.display(),
        style::dim("← branch"),
        style::accent(&branch)
    );
    println!();

    add_worktree(&root, &wt_path, &branch, base)?;
    println!(
        "{}",
        style::success(&format!("✓ worktree ready at {}", wt_path.display()))
    );
    emit_run_record(
        flow::Level::Info,
        "mission.run.start",
        mission_id,
        &sprint.id,
        &session_id,
        serde_json::json!({
            "role": role,
            "base": base,
            "branch": branch,
            "worktree": wt_path.display().to_string(),
        }),
    );

    // 4. Flip the sprint Planned → Running (consistent with `mission
    //    dispatch`). It IS being worked on now; `mission ship` flips it to
    //    Complete on merge. If it was already Running (a resumed run), the
    //    lifecycle call is a no-op-ish; surface any error softly.
    if matches!(sprint.status, crew::types::SprintStatus::Planned) {
        if let Err(e) = crew::lifecycle::sprint_start(&sprint.id) {
            eprintln!(
                "{}",
                style::warn(&format!(
                    "darkmux mission run: sprint_start({}) failed: {e:#} — continuing; \
                     state can be reconciled with `darkmux sprint` verbs.",
                    sprint.id
                ))
            );
        }
    }

    // 5. Dispatch the coder into the worktree, sprint-bound, internal
    //    runtime, --json so the token totals (#782a) land in metrics.json.
    println!(
        "\n{}",
        style::header(&format!("▶ dispatching `{role}` into the worktree…"))
    );
    // (#849 half 1) Carry forward corrections the reviewer recorded on earlier
    // dispatches in this mission (the doom-loop fix). Scope to the mission's
    // EXACT dispatch session ids (built from its sprints) — a `mission-run-<id>-`
    // prefix match would bleed a sibling mission whose id is a hyphen-extension.
    // Surface the texts so the operator sees what's injected — provenance, not
    // a silent rule (#44).
    let mission_session_ids: std::collections::HashSet<String> = sprints
        .iter()
        .filter(|s| s.mission_id.as_str() == mission_id)
        .map(|s| format!("mission-run-{}-{}", mission_id, s.id))
        .collect();
    // (#1002) Files this dispatch is about to work on (from the sprint
    // description) — used to rank file-in-play cautions + lessons above
    // engagement-level ones, and to staleness-check cautions against the
    // worktree's current content.
    let intent = intent_files(&sprint.description);

    // (#994 retrieve+inject) The three injected-context sources, each fully
    // ranked but UNCAPPED here — the proportional budget (#1011) decides how
    // much of each lands, so a large-window profile uses its headroom and a
    // small one isn't over-fed. Authority order: corrections (operator/reviewer
    // overrides) > lessons (authored) > cautions (auto-derived, flood-prone).
    let corrections = mission_adjudication_notes(&mission_session_ids);
    let cautions = mission_cautions(&mission_session_ids, &intent, &wt_path);
    let authored = engagement_lessons(&intent);

    // (#1011) Distribute a single budget — a fraction of THIS dispatch model's
    // context window — across the blocks with per-authority floors (no category
    // starves another), priority-ordered remainder, and a cautions cap.
    let budget = injected_budget_chars(
        // (#1282) `Err` = the default profile is quarantined; the coder
        // dispatch below would hard-fail with the same error, so fail early.
        crew::dispatch_internal::resolve_context_window_internal(None, None)?,
    );
    let (prior_corrections, detected_cautions, lessons) =
        allocate_injected_context(corrections, cautions, authored, budget);

    // Surface what's injected — provenance, not a silent rule (#44). Counts
    // reflect the post-budget selection.
    if !prior_corrections.is_empty() {
        println!(
            "{}",
            style::dim(&format!(
                "  carrying {} prior adjudication correction(s) into the brief \
                 (recorded by the reviewer on earlier dispatches in this mission):",
                prior_corrections.len()
            ))
        );
        for c in &prior_corrections {
            let first = c.lines().next().unwrap_or("").trim();
            println!("{}", style::dim(&format!("    • {first}")));
        }
    }
    if !detected_cautions.is_empty() {
        println!(
            "{}",
            style::dim(&format!(
                "  carrying {} detected loop caution(s) into the brief \
                 (darkmux's detectors flagged these on earlier dispatches in this mission):",
                detected_cautions.len()
            ))
        );
        for c in &detected_cautions {
            let first = c.lines().next().unwrap_or("").trim();
            println!("{}", style::dim(&format!("    • {first}")));
        }
    }
    if !lessons.is_empty() {
        println!(
            "{}",
            style::dim(&format!(
                "  carrying {} engagement lesson(s) into the brief:",
                lessons.len()
            ))
        );
        for c in &lessons {
            let first = c.lines().next().unwrap_or("").trim();
            println!("{}", style::dim(&format!("    • {first}")));
        }
    }
    let opts = crew::dispatch::DispatchOpts {
        role_id: role.to_string(),
        message: coder_brief(&sprint, mission, &lessons, &prior_corrections, &detected_cautions),
        deliver: None,
        session_id: Some(session_id.clone()),
        timeout_seconds,
        skip_preflight: false,
        json: true,
        // mission run drives its own surfacing; don't watch the role's
        // default openclaw workspace dir (library-caller convention).
        watch_paths: Vec::new(),
        workdir: Some(wt_path.clone()),
        sprint_id: Some(sprint.id.clone()),
        runtime: crew::dispatch::Runtime::Internal,
        runtime_cmd: "openclaw".to_string(),
        machine: None,
        wait: true,
        compaction: crew::dispatch::CompactionDispatchArgs::default(),
        profile_name: None,
        // (#984) mission run uses the default registry — no --profiles-file.
        config_path: None,
        // (#1199) Bench-only knobs; defaults preserve existing behavior.
        force_container: false,
        max_completion_tokens: None,
        image: image.map(String::from),
        model_base_url_override: None,
    };
    let result = fleet::dispatch_routed(opts)?;
    eprintln!(
        "{}",
        style::dim(&format!("darkmux mission run: session id `{session_id}`"))
    );

    // Token tally — the off-meter number, read from the same canonical
    // metrics.json #782a emits into the stream. Tokens only, never currency.
    let tokens = result
        .out_dir
        .as_deref()
        .map(crew::dispatch_internal::read_token_totals)
        .unwrap_or_default();

    if result.exit_code != 0 {
        eprintln!(
            "{}",
            style::error(&format!(
                "✗ coder dispatch exited {} — see stderr above. The sprint stays Running and \
                 the worktree is left at {} for inspection. Re-running `darkmux mission run` will \
                 refuse until you tear it down: `darkmux mission abort {mission_id} --sprint {}`.",
                result.exit_code,
                wt_path.display(),
                sprint.id,
            ))
        );
        print_token_line(&tokens);
        emit_run_record(
            flow::Level::Error,
            "mission.run.error",
            mission_id,
            &sprint.id,
            &session_id,
            serde_json::json!({
                "exit_code": result.exit_code,
                "total_tokens": tokens.total(),
            }),
        );
        return Ok(1);
    }
    println!("{}", style::success("✓ coder dispatch complete"));
    print_token_line(&tokens);

    // (#799 part 2) Verifier-fabrication backstop: the runtime stamped any bash
    // verifier commands that FAILED TO RUN (never executed) onto the dispatch
    // envelope. Emit a per-run `mission.run.verification` record UNCONDITIONALLY
    // (empty `failed` on an honest run) keyed by this run's deterministic
    // session id, so `mission ship --merge` reads the LATEST run's status and
    // HOLDs only when that run had failures. Emitting on EVERY run is what lets
    // a clean re-run CLEAR a prior dirty run's record (the reader is latest-wins
    // by overwrite); a conditional emit would leave a stale dirty record that
    // the documented fix-and-retry could never clear. The gate banner reads the
    // in-memory parse below, so an empty record adds no operator-facing noise.
    // Soft everywhere: `run` still returns 0 at a clean gate; the operator
    // decides (operator sovereignty #44).
    let failed_verifiers = parse_failed_verifiers(&result.stdout);
    emit_run_record(
        if failed_verifiers.is_empty() {
            flow::Level::Info
        } else {
            flow::Level::Warn
        },
        "mission.run.verification",
        mission_id,
        &sprint.id,
        &session_id,
        serde_json::json!({
            "failed": failed_verifiers,
            "count": failed_verifiers.len(),
        }),
    );

    // 6. Local QA — reuse `sprint review` against the worktree diff vs base.
    //    require_clean=false: the worktree has uncommitted changes by design
    //    (the whole point is reviewing pre-commit work).
    println!(
        "\n{}",
        style::header("▶ local QA — dispatching `code-reviewer` against the worktree diff…")
    );
    // A QA *dispatch* failure (reviewer image pull, timeout, etc.) is NOT a
    // coder failure — don't propagate it as exit 1. The coder's work is in
    // the worktree and the gate still matters; surface that QA couldn't run
    // and let the operator/frontier review manually. Distinct exit 3.
    let review = match crate::sprint_cli::sprint_review_output_at(
        &wt_path,
        Some(base),
        Some(&sprint.id),
    ) {
        Ok(r) => r,
        Err(e) => {
            eprintln!(
                "{}",
                style::warn(&format!(
                    "⚠ QA could not run ({e:#}). The coder's work is in the worktree — \
                     review the diff manually before shipping."
                ))
            );
            emit_run_record(
                flow::Level::Warn,
                "mission.run.qa-unavailable",
                mission_id,
                &sprint.id,
                &session_id,
                serde_json::json!({ "error": format!("{e:#}"), "total_tokens": tokens.total() }),
            );
            println!("\n{}", style::header("▶ gate — QA unavailable, manual review required"));
            print_unverified_banner(&failed_verifiers);
            println!("  {} {}", style::dim("worktree:"), wt_path.display());
            println!("  {} {}", style::dim("branch:  "), style::accent(&branch));
            println!(
                "\n{}",
                style::warn(&format!(
                    "review the diff manually, then:  darkmux mission ship {mission_id} --sprint {} \
                     (or abort: darkmux mission abort {mission_id} --sprint {})",
                    sprint.id, sprint.id
                ))
            );
            return Ok(3);
        }
    };

    print_review_summary(&review);

    // 7. Stop at the gate. Tee up the ship step; never commit/PR/merge here.
    println!("\n{}", style::header("▶ gate — awaiting frontier/operator sign-off"));
    println!(
        "  {} {}",
        style::dim("worktree:"),
        wt_path.display()
    );
    println!(
        "  {} {}",
        style::dim("branch:  "),
        style::accent(&branch)
    );
    print_unverified_banner(&failed_verifiers);

    let blockers = review.by_severity.block > 0;
    if blockers {
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
            style::dim("re-run QA after fixing: darkmux sprint review (in the worktree)")
        );
        println!(
            "  {}",
            style::dim(&format!(
                "or abandon this run: darkmux mission abort {mission_id} --sprint {}",
                sprint.id
            ))
        );
        emit_run_record(
            flow::Level::Warn,
            "mission.run.blocked",
            mission_id,
            &sprint.id,
            &session_id,
            serde_json::json!({
                "verdict": review.verdict,
                "blockers": review.by_severity.block,
                "flags": review.by_severity.flag,
                "total_tokens": tokens.total(),
            }),
        );
        return Ok(2);
    }

    println!(
        "\n{}",
        style::success(&format!(
            "✓ ready for sign-off. After review:  darkmux mission ship {mission_id} --sprint {}",
            sprint.id
        ))
    );
    // (#807/#817) Cue the frontier orchestrator at the decision moment —
    // tool output is the one hint channel every harness reads, and the
    // scaffold's placeholder IS the style direction (operator feedback:
    // the first cut's "<verdict · what you overrode · why>" produced wordy
    // technical notes on the dashboard card). Two channels, routed by tag:
    //   source=adjudication → the audit trail (technical reasoning, never
    //                          rendered on the hero card)
    //   source=orchestrator → the dashboard card (positive, digestible)
    println!(
        "{}",
        style::dim(&format!(
            "  record your adjudication (audit trail):  darkmux flow note \
             --session-id {session_id} \
             --text \"<verdict · what you overrode · why>\" --source adjudication",
        ))
    );
    emit_run_record(
        flow::Level::Info,
        "mission.run.gate",
        mission_id,
        &sprint.id,
        &session_id,
        serde_json::json!({
            "verdict": review.verdict,
            "flags": review.by_severity.flag,
            "nits": review.by_severity.nit,
            "total_tokens": tokens.total(),
        }),
    );
    Ok(0)
}

/// `darkmux mission abort` — the explicit teardown half of the hybrid
/// contract. Removes the sprint's worktree + its branch and flips the sprint
/// `Running → Abandoned`, so a frontier/operator who decides mid-loop that the
/// run is going nowhere can cleanly back it out (vs. leaving an orphan
/// worktree). Idempotent-ish: a missing worktree/branch is reported, not
/// fatal. Returns `0` on a clean teardown.
pub fn abort(mission_id: &str, sprint_id: Option<&str>) -> Result<i32> {
    use crew::loader::{load_missions, load_sprints};

    fleet::validate_identifier("mission_id", mission_id)?;
    if let Some(s) = sprint_id {
        fleet::validate_identifier("--sprint", s)?;
    }

    let missions = load_missions()?;
    let mission = missions
        .iter()
        .find(|m| m.id == mission_id)
        .ok_or_else(|| anyhow::anyhow!("mission `{mission_id}` not found"))?;
    let sprints = load_sprints()?;
    // Explicit `--sprint` resolves by id (any status — a Running sprint, the
    // common abort case after a `run`, resolves); auto-path requires a ready
    // Planned sprint. So to abort a Running sprint, pass `--sprint`.
    let sprint = resolve_sprint(&sprints, mission_id, sprint_id)?;

    let root = repo_root()?;
    let wt_path = worktree_path(&root, &sprint.id);
    let conv = crate::conventions::load(&root);
    let branch = conventions_branch(&sprint, mission, conv.as_ref());

    println!(
        "{}",
        style::header(&format!(
            "▶ mission abort — {} · sprint {}",
            mission_id, sprint.id
        ))
    );

    // Remove the worktree (force — it has uncommitted work by design).
    if wt_path.exists() {
        let out = Command::new("git")
            .current_dir(&root)
            .args(["worktree", "remove", "--force", &wt_path.to_string_lossy()])
            .output()
            .context("running `git worktree remove`")?;
        if out.status.success() {
            println!("{}", style::success(&format!("✓ removed worktree {}", wt_path.display())));
        } else {
            eprintln!(
                "{}",
                style::warn(&format!(
                    "git worktree remove failed: {} — you may need `git worktree prune`.",
                    String::from_utf8_lossy(&out.stderr).trim()
                ))
            );
        }
    } else {
        println!("{}", style::dim(&format!("worktree {} already gone", wt_path.display())));
    }

    // Delete the branch (best-effort — it may be checked out elsewhere or
    // already gone).
    let out = Command::new("git")
        .current_dir(&root)
        .args(["branch", "-D", &branch])
        .output()
        .context("running `git branch -D`")?;
    if out.status.success() {
        println!("{}", style::success(&format!("✓ deleted branch {branch}")));
    } else {
        println!(
            "{}",
            style::dim(&format!(
                "branch {branch} not deleted ({}) — likely already gone",
                String::from_utf8_lossy(&out.stderr).trim()
            ))
        );
    }

    // Flip the sprint Running/Planned → Abandoned (legal restart later).
    match crew::lifecycle::sprint_abandon(&sprint.id) {
        Ok(_) => println!("{}", style::success(&format!("✓ sprint {} → Abandoned", sprint.id))),
        Err(e) => eprintln!(
            "{}",
            style::warn(&format!(
                "sprint_abandon({}) failed: {e:#} — reconcile with `darkmux sprint` verbs.",
                sprint.id
            ))
        ),
    }

    let session_id = format!("mission-run-{}-{}", mission_id, sprint.id);
    emit_run_record(
        flow::Level::Info,
        "mission.run.abort",
        mission_id,
        &sprint.id,
        &session_id,
        serde_json::json!({ "branch": branch, "worktree": wt_path.display().to_string() }),
    );
    Ok(0)
}

/// Resolve the sprint a post-run verb (`ship` / `abort`) targets. An explicit
/// `--sprint` is looked up by id directly (no status filter — so a Running
/// sprint, the common post-`run` case, resolves); otherwise fall back to
/// `select_sprint`'s ready-Planned auto-pick.
fn resolve_sprint(
    sprints: &[crew::types::Sprint],
    mission_id: &str,
    explicit: Option<&str>,
) -> Result<crew::types::Sprint> {
    match explicit {
        Some(id) => sprints
            .iter()
            .find(|s| s.id == id && s.mission_id == mission_id)
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("sprint `{id}` not found in mission `{mission_id}`")),
        None => select_sprint(sprints, mission_id, None),
    }
}

/// Run a git subcommand in `dir`, returning its captured output. Thin wrapper
/// so the ship/abort git calls read uniformly.
fn git_in(dir: &Path, args: &[&str]) -> Result<std::process::Output> {
    Command::new("git")
        .current_dir(dir)
        .args(args)
        .output()
        .with_context(|| format!("running `git {}`", args.join(" ")))
}

/// (#834) Commit under a declared bot identity, airtight against ambient env.
/// Git resolves the author/committer from `GIT_*_NAME`/`GIT_*_EMAIL` env vars
/// FIRST, then `-c user.name/email`, then config — so a shell that already
/// exports `GIT_AUTHOR_*` would override a bare `-c`. Set BOTH the env vars
/// (authoritative, both author + committer) AND the `-c` args (visible/belt),
/// so the identity holds regardless of the inherited environment. Caller
/// guarantees name/email are non-blank (via `config_args().is_some()`).
fn commit_with_identity(
    dir: &Path,
    a: &crate::conventions::CommitAuthor,
    msg: &str,
) -> Result<std::process::Output> {
    let (n, e) = (a.name.trim(), a.email.trim());
    Command::new("git")
        .current_dir(dir)
        .args([
            "-c",
            &format!("user.name={n}"),
            "-c",
            &format!("user.email={e}"),
            "commit",
            "-m",
            msg,
        ])
        .env("GIT_AUTHOR_NAME", n)
        .env("GIT_AUTHOR_EMAIL", e)
        .env("GIT_COMMITTER_NAME", n)
        .env("GIT_COMMITTER_EMAIL", e)
        .output()
        .with_context(|| "running `git commit` under the declared commit_author".to_string())
}

/// (#834) The identity git would commit under in `dir` (local→global config),
/// formatted `Name <email>` for the soft-guard message. Best-effort: missing
/// pieces render as `(unset)` so the guard still reads sensibly.
fn resolved_git_identity(dir: &Path) -> String {
    let g = |k: &str| {
        git_in(dir, &["config", k])
            .ok()
            .filter(|o| o.status.success())
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "(unset)".to_string())
    };
    format!("{} <{}>", g("user.name"), g("user.email"))
}

/// Commit subject for a shipped sprint: the sprint description's first line,
/// trimmed to a conventional ~72-char subject.
/// (#815) The coder's dispatch brief: the sprint's compiled description
/// (the STRUCTURE) plus, when the mission carries it, the operator's
/// verbatim `mission propose` input (the WORDS) under a provenance-tagged
/// block. The 2026-06-12 dogfood showed the compiler compressing exact
/// strings + constraints out of the description — and since the description
/// IS the brief, the constraints never reached the coder. The tagged block
/// follows the model-facing prompt doctrine: AI-convention framing, with
/// the tag itself carrying the provenance a clean-context model needs.
fn coder_brief(
    sprint: &crew::types::Sprint,
    mission: &crew::types::Mission,
    lessons: &[String],
    prior_corrections: &[String],
    cautions: &[String],
) -> String {
    let base = match mission.source_input.as_deref().map(str::trim) {
        Some(src) if !src.is_empty() => format!(
            "{desc}\n\n<operator-source-input>\nThe user's original, unabridged request that \
             produced this sprint. The summary above is derived from it; where this text \
             adds constraints, exact strings, or scope limits beyond the summary, THIS \
             text is authoritative.\n\n{src}\n</operator-source-input>",
            desc = sprint.description,
        ),
        _ => sprint.description.clone(),
    };
    append_injected_blocks(base, lessons, prior_corrections, cautions)
}

/// (#994 / #1004) Append the three injected-context blocks — authored lessons
/// (FOLLOW), prior adjudication corrections (verify-then-apply), and detected
/// cautions (avoid-the-dead-end) — to `out`, each independent (any/all/none
/// appear). Extracted from [`coder_brief`] so the loop-lab A/B (#1004) can build
/// the SAME blocks, with the SAME wrapper framing, that a real dispatch injects
/// — one source of truth, so the bench measures the real thing.
fn append_injected_blocks(
    mut out: String,
    lessons: &[String],
    prior_corrections: &[String],
    cautions: &[String],
) -> String {
    // (#994) Operator-AUTHORED engagement lessons — the lessons store's read
    // side, the authored sibling of the auto-detected cautions below. FOLLOW
    // framing, not verify: these are the rules the team actually follows + the
    // why, authoritative unless clearly stale. Placed first after the task so
    // they're salient. Independent of the other blocks (any / all / none appear).
    if !lessons.is_empty() {
        let listed = lessons
            .iter()
            // Same XML-fence defense as the other blocks — a lesson body can
            // carry operator-written text; neutralize a literal closing tag.
            .map(|c| c.replace("</lessons>", ""))
            .collect::<Vec<_>>()
            .join("\n");
        out = format!(
            "{out}\n\n<lessons>\nThe user recorded these conventions and decisions for this \
             codebase — the rules the team actually follows and the reasoning behind them. Treat \
             them as authoritative: follow them, and prefer them over a generic default when they \
             conflict. If one is clearly stale against the current code, say so in your final \
             message rather than silently ignoring it:\n\n\
             {listed}\n</lessons>"
        );
    }

    if !prior_corrections.is_empty() {
        // (#849 half 1) Persist adjudication corrections into the brief — the
        // doom-loop fix: a correction the reviewer made once should never have to
        // be re-derived by the next dispatch. Injected as CONTEXT with provenance,
        // never a silent rule (operator sovereignty #44); the operator sees the
        // count logged at dispatch time and the block itself here.
        //
        // (#453) Framed as findings-to-verify, not directives. The wrong-diagnosis-
        // stuck failure mode (Beat 51) was a coder anchoring on a confident-but-wrong
        // verdict and looping to a watchdog timeout; "Honor them — do not re-make
        // these mistakes" was the anchoring framing. The reframe splits concrete
        // FACTS (safe to apply after a quick check) from DIAGNOSES (reproduce before
        // applying), so a wrong correction is re-checked against the live workspace
        // rather than entrenched. The #849 carry-forward is unchanged — corrections
        // are still injected, the count still logged; only the framing shifts.
        let corrections = prior_corrections
            .iter()
            // Defense-in-depth: a note must not break the XML fence around the
            // block. The adjudication channel is operator-only (no role/runtime
            // path emits `--source adjudication`), so this is a self-inflicted-only
            // vector — but neutralizing a literal closing tag is cheap.
            .map(|c| {
                format!(
                    "- {}",
                    c.trim().replace("</prior-adjudication-corrections>", "")
                )
            })
            .collect::<Vec<_>>()
            .join("\n");
        out = format!(
            "{out}\n\n<prior-adjudication-corrections>\nThe user's reviewer recorded these \
             corrections while reviewing earlier dispatches in this mission. Treat each as a \
             finding from an earlier context, not a fact about your current workspace. If a \
             correction names a concrete change (a renamed field, a config key, a command, an \
             exact string), check it against the code or by running the command it names, and \
             apply it if it holds. If it names a diagnosis (a race condition, a broken \
             invariant, a failing test), reproduce the specific claim before changing anything: \
             run the test or trace the code path it names. If a correction does not hold against \
             your current workspace, say so in your final message and re-diagnose; if \
             re-diagnosis does not converge quickly, surface the blocker and stop rather than \
             looping:\n\n\
             {corrections}\n</prior-adjudication-corrections>"
        );
    }

    // (#994 retrieve+inject) The auto-derived sibling of the corrections block:
    // loop pathologies darkmux's detectors flagged in earlier dispatches of this
    // mission (cycles, reasoning loops, tool-failure cascades), keyed where known
    // to the file they happened in (the #994 capture slice). Framed as findings-
    // to-verify (#453), not directives — a caution from an earlier context may be
    // irrelevant now; the value is "don't walk back into a known dead end," never
    // a required action. Independent of the corrections block (either, both, or
    // neither may be present).
    if !cautions.is_empty() {
        let listed = cautions
            .iter()
            // Same XML-fence defense-in-depth as the corrections block — `detail`
            // is container-written (a tool name flows into the cycle detail), so
            // neutralize a literal closing tag.
            .map(|c| c.replace("</detected-cautions>", ""))
            .collect::<Vec<_>>()
            .join("\n");
        out = format!(
            "{out}\n\n<detected-cautions>\ndarkmux's loop detectors flagged these patterns in \
             earlier dispatches in this mission — repeated tool calls, looping reasoning, \
             tool-failure cascades. They are signals from earlier contexts, not facts about \
             your current workspace: a pattern that fired earlier may be irrelevant now. Use \
             them to avoid walking back into a known dead end — if you notice yourself about to \
             repeat one, stop and change your approach. None of these is a required action:\n\n\
             {listed}\n</detected-cautions>"
        );
    }

    out
}

/// (#1004) Build the engagement-context blocks the loop-lab A/B injects — the
/// SAME `<lessons>` / `<prior-adjudication-corrections>` / `<detected-cautions>`
/// blocks a real coder dispatch gets, run through the SAME #1011 budget, so the
/// bench measures the real thing. With a `mission_id`, corrections + cautions
/// scope to that mission's sessions; without one, only the repo's authored
/// lessons inject. `workspace_root` is the tree the staleness check reads.
/// Returns the blocks alone (no task), empty when nothing applies. `profile` +
/// `profiles_file` size the #1011 budget against the SAME profile window the
/// dispatch will use (so the A/B truncates context the way the shipped config
/// would), matching the lab's `--profile` / `--profiles-file`.
pub(crate) fn injected_context_for_lab(
    mission_id: Option<&str>,
    workspace_root: &Path,
    profile: Option<&str>,
    profiles_file: Option<&str>,
) -> String {
    use crew::loader::load_sprints;
    // No sprint at lab time → no files-in-play; lessons/cautions still inject at
    // engagement scope.
    let intent = std::collections::HashSet::new();
    let (corrections, cautions) = match mission_id {
        Some(mid) => {
            let ids: std::collections::HashSet<String> = load_sprints()
                .unwrap_or_default()
                .iter()
                .filter(|s| s.mission_id.as_str() == mid)
                .map(|s| format!("mission-run-{}-{}", mid, s.id))
                .collect();
            (
                mission_adjudication_notes(&ids),
                mission_cautions(&ids, &intent, workspace_root),
            )
        }
        None => (Vec::new(), Vec::new()),
    };
    let lessons = engagement_lessons(&intent);
    let budget = injected_budget_chars(
        // (#1282) `Err` = the named/default profile is quarantined. The lab
        // dispatch itself hard-fails with this same error; here (bench-brief
        // sizing, a String-returning helper) degrade loudly to the
        // no-window budget default instead.
        crew::dispatch_internal::resolve_context_window_internal(profile, profiles_file)
            .unwrap_or_else(|e| {
                eprintln!("{e:#}");
                None
            }),
    );
    let (c, ca, l) = allocate_injected_context(corrections, cautions, lessons, budget);
    // append_injected_blocks prefixes each block with "\n\n"; drop the leading
    // blank lines so the result is a clean prepend for the lab prompt.
    append_injected_blocks(String::new(), &l, &c, &ca)
        .trim_start()
        .to_string()
}

/// (#849 half 1) The adjudication corrections recorded across this mission's
/// dispatches, for injection into the next coder brief. Scans the flow trail
/// for `action=note` + `source=adjudication` whose `session_id` is one of the
/// mission's EXACT dispatch session ids (`mission_session_ids`, built from the
/// mission's sprints as `mission-run-<mission>-<sprint>`). Exact-set match, NOT
/// a `mission-run-<mission>-` prefix — a prefix bleeds a sibling mission whose
/// id is a hyphen-extension (`auth` would swallow `auth-v2`'s notes, since
/// `mission-run-auth-v2-s1` starts with `mission-run-auth-`). Mission-scoped,
/// not sprint-scoped, by design — a correction like "don't rename that field"
/// applies mission-wide. Best-effort: any IO/parse problem reads as "no
/// corrections" (the loop just doesn't get the carry-forward, never errors).
/// Bounded: the most-recent `ADJUDICATION_LOOKBACK_DAYS` day-files. Returned
/// **newest-first** and NOT count-capped (#1011) — corrections are the highest-
/// authority block, so the proportional budget keeps the freshest from the front.
/// Mirrors `session_has_orchestrator_note`.
fn mission_adjudication_notes(mission_session_ids: &std::collections::HashSet<String>) -> Vec<String> {
    const ADJUDICATION_LOOKBACK_DAYS: usize = 7;
    if mission_session_ids.is_empty() {
        return Vec::new();
    }
    let flows_dir = darkmux_types::config_access::flows_dir();
    let Ok(entries) = std::fs::read_dir(&flows_dir) else {
        return Vec::new();
    };
    let mut days: Vec<PathBuf> = entries
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().and_then(|x| x.to_str()) == Some("jsonl"))
        .collect();
    days.sort();
    // Most-recent N day-files, oldest→newest within the window (so the cap
    // below keeps the freshest corrections).
    let recent: Vec<PathBuf> = days
        .iter()
        .rev()
        .take(ADJUDICATION_LOOKBACK_DAYS)
        .rev()
        .cloned()
        .collect();
    let mut notes: Vec<String> = Vec::new();
    for day in &recent {
        let Ok(raw) = std::fs::read_to_string(day) else {
            continue;
        };
        for line in raw.lines() {
            let Ok(r) = serde_json::from_str::<serde_json::Value>(line) else {
                continue;
            };
            if r.get("action").and_then(|v| v.as_str()) == Some("note")
                && r.get("source").and_then(|v| v.as_str()) == Some("adjudication")
                && r.get("session_id")
                    .and_then(|v| v.as_str())
                    .is_some_and(|s| mission_session_ids.contains(s))
            {
                if let Some(text) = r.get("handle").and_then(|v| v.as_str()) {
                    let t = text.trim();
                    // Skip empties + exact duplicates (a correction re-recorded
                    // verbatim shouldn't repeat in the brief).
                    if !t.is_empty() && !notes.iter().any(|n| n == t) {
                        notes.push(t.to_string());
                    }
                }
            }
        }
    }
    // Newest-first so the budget's front-take keeps the freshest corrections
    // (notes were collected oldest→newest across the day-files). Dedup keeps the
    // FIRST occurrence, so "newest" here means newest-first-seen — a re-stated
    // correction keeps its original position, not the restatement's.
    notes.reverse();
    notes
}

/// (#1002, ported from `runtime/src/loop_runner.rs` #471 per the no-cross-
/// workspace-dep convention) Lexically clean a path — drop `.` components, fold
/// `..` against the preceding normal component, drop trailing separators —
/// without touching the filesystem. Applied to BOTH the stored caution/lesson
/// `file` keys AND the dispatch-intent files at match time, so `./src/x`,
/// `src/x/`, and `src/../src/x` all compare equal.
fn normalize_path_lexical(p: &str) -> String {
    use std::path::Component;
    let mut out = PathBuf::new();
    for comp in Path::new(p).components() {
        match comp {
            Component::CurDir => {}
            Component::ParentDir => {
                if matches!(out.components().next_back(), Some(Component::Normal(_))) {
                    out.pop();
                } else {
                    out.push("..");
                }
            }
            Component::RootDir => out.push(std::path::MAIN_SEPARATOR.to_string()),
            Component::Prefix(pre) => out.push(pre.as_os_str()),
            Component::Normal(seg) => out.push(seg),
        }
    }
    out.to_string_lossy().into_owned()
}

/// (#1002) The files a dispatch is about to work on, derived from the sprint
/// description — the source knowable at brief-assembly time (a git diff is empty
/// here; precise files-in-play is the deferred #219-fed Half B). Conservative
/// tokenizer (no `regex` dep): a token is a candidate path when, after stripping
/// surrounding punctuation/backticks, it is made only of path-safe characters
/// AND either contains a `/` or ends in a known code extension — so `src/foo.rs`
/// and `Cargo.toml` match but prose like `e.g.` or `American.` does not. Returns
/// the NORMALIZED set for direct comparison against normalized stored keys.
fn intent_files(description: &str) -> std::collections::HashSet<String> {
    const CODE_EXTS: &[&str] = &[
        ".rs", ".toml", ".json", ".md", ".ts", ".js", ".py", ".html", ".css", ".sh", ".yaml",
        ".yml", ".sql", ".txt",
    ];
    let path_safe = |s: &str| {
        !s.is_empty() && s.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-' | '/'))
    };
    description
        .split(|c: char| c.is_whitespace() || matches!(c, '`' | '"' | '\'' | '(' | ')' | '[' | ']' | '{' | '}' | ',' | ';' | ':' | '<' | '>'))
        .map(|t| t.trim_matches(|c: char| matches!(c, '.' | '/' | '-')))
        .filter(|t| {
            path_safe(t)
                && (t.contains('/') || CODE_EXTS.iter().any(|e| t.to_ascii_lowercase().ends_with(e)))
        })
        .map(normalize_path_lexical)
        .filter(|t| !t.is_empty())
        .collect()
}

/// (#1002) BLAKE3 hex of `file` under `workspace_root` right now, for the
/// staleness check against a caution's firing-time `code_hash` (#1001). Same
/// algorithm + raw-bytes framing as the runtime captured with, so an unchanged
/// file matches. Best-effort: a missing/unreadable file → `None` (treated as
/// "unknown freshness", never stale — we don't bury what we can't verify).
fn current_file_blake3(workspace_root: &Path, file: &str) -> Option<String> {
    let bytes = std::fs::read(workspace_root.join(file)).ok()?;
    Some(blake3::hash(&bytes).to_hex().to_string())
}

/// (#1011) Char budget for the coder brief's injected-context blocks — a
/// fraction (operator-tunable, default 15%) of the dispatch model's context
/// window, in chars (≈ tokens × 4, so no tokenizer dependency). A fraction
/// auto-scales across profiles from one value: a large-window profile gets
/// proportionally more room than a small one, so the cap neither wastes a
/// `deep` profile's headroom nor over-feeds a `fast` one. Falls back to a
/// default window when the profile can't resolve, and floors at a minimum so a
/// pathologically small window still leaves the per-category floors meaningful.
fn injected_budget_chars(n_ctx: Option<u32>) -> usize {
    budget_chars_for(n_ctx, darkmux_types::config_access::injected_context_fraction())
}

/// Pure budget math (split out from [`injected_budget_chars`] so it's testable
/// without touching the config/env tier).
fn budget_chars_for(n_ctx: Option<u32>, frac: f64) -> usize {
    const DEFAULT_N_CTX: u32 = 32_768; // only when the profile can't resolve
    const CHARS_PER_TOKEN: usize = 4;
    const MIN_BUDGET_CHARS: usize = 2_000;
    let window = n_ctx.unwrap_or(DEFAULT_N_CTX) as f64;
    ((window * frac) as usize * CHARS_PER_TOKEN).max(MIN_BUDGET_CHARS)
}

/// (#1011, #994 decision #5) Distribute `budget` chars across the three
/// already-ranked injected-context blocks — replacing the old per-block hard
/// counts (the real constraint is the small model's window, not a flat number).
/// Three rules, each closing one failure mode. (1) Per-category floor: a
/// non-empty category is guaranteed a minimum slice before anything else fills,
/// so none starves (the doom-loop's named failure is a correction evaporating);
/// an empty category's floor returns to the pool. (2) Priority-ordered
/// remainder: leftover fills by authority (corrections, then lessons, then
/// cautions) — bulk to the highest authority, but only after every floor is
/// honored. (3) Cautions cap: the cheap, high-volume auto-source gets a ceiling
/// so a flood of firings can't dominate even the remainder.
/// Each list is returned truncated in rank order to its allowance. The
/// fractions are empirical starting points (tuned from real dispatches via the
/// loop lab #1004) — the mechanism is what ships; the exact numbers will move.
fn allocate_injected_context(
    corrections: Vec<String>,
    cautions: Vec<String>,
    lessons: Vec<String>,
    budget: usize,
) -> (Vec<String>, Vec<String>, Vec<String>) {
    const FLOOR_CORR: f64 = 0.40; // highest authority — strongest floor
    const FLOOR_LESS: f64 = 0.30;
    const FLOOR_CAUT: f64 = 0.15;
    const CAP_CAUT: f64 = 0.35; // ceiling on the high-volume auto-source

    // A block's char cost = its bullets joined by newlines.
    let demand = |v: &[String]| -> usize {
        if v.is_empty() {
            0
        } else {
            v.iter().map(String::len).sum::<usize>() + v.len() - 1
        }
    };
    let frac = |f: f64| ((budget as f64) * f) as usize;
    let (dc, dl, dca) = (demand(&corrections), demand(&lessons), demand(&cautions));

    // Floor only for a non-empty category (an empty one's slice returns to the
    // pool), and never reserve more than the category can actually use.
    let fc = if dc == 0 { 0 } else { dc.min(frac(FLOOR_CORR)) };
    let fl = if dl == 0 { 0 } else { dl.min(frac(FLOOR_LESS)) };
    let fca = if dca == 0 { 0 } else { dca.min(frac(FLOOR_CAUT)) };

    // Remainder by authority: corrections, then lessons, then cautions (capped).
    let mut pool = budget.saturating_sub(fc + fl + fca);
    let ec = (dc - fc).min(pool);
    pool -= ec;
    let el = (dl - fl).min(pool);
    pool -= el;
    let caut_ceiling = frac(CAP_CAUT).saturating_sub(fca);
    let eca = (dca - fca).min(pool).min(caut_ceiling);

    // Take bullets in rank order while they fit the allowance (stop at the first
    // that doesn't — preserve rank, don't skip ahead to a smaller later one).
    let take = |v: Vec<String>, allow: usize| -> Vec<String> {
        let mut used = 0usize;
        let mut out = Vec::new();
        for item in v {
            let add = if out.is_empty() { item.len() } else { item.len() + 1 };
            if used + add > allow {
                break;
            }
            used += add;
            out.push(item);
        }
        out
    };
    (
        take(corrections, fc + ec),
        take(cautions, fca + eca),
        take(lessons, fl + el),
    )
}

/// (#994 retrieve+inject) The loop pathologies darkmux's detectors flagged
/// across this mission's earlier dispatches, for injection into the next coder
/// brief — the doom-loop fix's AUTO-DERIVED half (sibling to the operator-
/// authored corrections from [`mission_adjudication_notes`]). A *caution* is a
/// detector telemetry flow record (`category=telemetry`, `source=detector`,
/// emitted by `crew::dispatch_internal`'s detector tailer): a repeated-tool-
/// call cycle, a reasoning loop, a tool-failure cascade. Each carries a human-
/// readable `detail` and, when the firing targeted a file (#994 capture slice),
/// a `payload.area.files[0]` the bullet names as the "where".
///
/// Reads the flow stream DIRECTLY — always fresh, no dependency on the SQLite
/// index's derive-on-rebuild freshness (the index serves the query/recall +
/// status surface; this hot per-dispatch path mirrors the corrections
/// collector). Scoped to the mission's EXACT dispatch session ids (exact-set,
/// not a `mission-run-<id>-` prefix — same sibling-bleed guard as #849), deduped,
/// over the most-recent `CAUTION_LOOKBACK_DAYS` day-files. Fully ranked but NOT
/// count-capped (#1011 — the proportional budget governs how many land).
/// (#1002) Ranked **file-in-play first** (a caution about a file this
/// dispatch will touch — per `intent` — outranks an engagement-level one), then
/// **fresh over stale** (a caution whose firing-time `code_hash` (#1001) no
/// longer matches the file's current content under `workspace_root` is
/// de-prioritized — the pathology may not survive the change), then
/// **severity** (a high-severity older cycle outranks a low-severity recent
/// stall — deliberate divergence from the recency-only corrections sibling),
/// then **recency**. Best-effort: any IO/parse problem reads as "no cautions"
/// (the loop just doesn't get the carry-forward, never errors).
///
/// (#994 QA / #1002) The `category=telemetry` + `source=detector` predicate
/// below is the Value-based twin of the typed `is_detector_caution` in
/// `darkmux_crew::index` — they classify the same records from different
/// representations. Keep them in sync: a change to `source`/`category` semantics
/// must update BOTH.
fn mission_cautions(
    mission_session_ids: &std::collections::HashSet<String>,
    intent: &std::collections::HashSet<String>,
    workspace_root: &Path,
) -> Vec<String> {
    const CAUTION_LOOKBACK_DAYS: usize = 7;
    if mission_session_ids.is_empty() {
        return Vec::new();
    }
    let flows_dir = darkmux_types::config_access::flows_dir();
    let Ok(entries) = std::fs::read_dir(&flows_dir) else {
        return Vec::new();
    };
    let mut days: Vec<PathBuf> = entries
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().and_then(|x| x.to_str()) == Some("jsonl"))
        .collect();
    days.sort();
    let recent: Vec<PathBuf> = days
        .iter()
        .rev()
        .take(CAUTION_LOOKBACK_DAYS)
        .rev()
        .cloned()
        .collect();

    // (match, fresh, severity_rank, ts, bullet) — sorted file-in-play-first,
    // then fresh-over-stale, then severity, then recency below; deduped (a
    // pathology that recurred verbatim shouldn't repeat) and capped.
    let mut found: Vec<(u8, u8, u8, String, String)> = Vec::new();
    for day in &recent {
        let Ok(raw) = std::fs::read_to_string(day) else {
            continue;
        };
        for line in raw.lines() {
            let Ok(r) = serde_json::from_str::<serde_json::Value>(line) else {
                continue;
            };
            // A caution = a detector telemetry record scoped to this mission.
            if r.get("category").and_then(|v| v.as_str()) != Some("telemetry") {
                continue;
            }
            if r.get("source").and_then(|v| v.as_str()) != Some("detector") {
                continue;
            }
            let in_mission = r
                .get("session_id")
                .and_then(|v| v.as_str())
                .is_some_and(|s| mission_session_ids.contains(s));
            if !in_mission {
                continue;
            }
            let payload = r.get("payload");
            let pstr = |k: &str| payload.and_then(|p| p.get(k)).and_then(|v| v.as_str());
            let detail = pstr("detail").unwrap_or("");
            if detail.is_empty() {
                continue;
            }
            let kind = pstr("kind").unwrap_or("caution");
            let severity = pstr("severity").unwrap_or("warn");
            let area = payload.and_then(|p| p.get("area"));
            let file = area
                .and_then(|a| a.get("files"))
                .and_then(|f| f.as_array())
                .and_then(|arr| arr.first())
                .and_then(|v| v.as_str());
            let code_hash = area.and_then(|a| a.get("code_hash")).and_then(|v| v.as_str());
            let ts = r.get("ts").and_then(|v| v.as_str()).unwrap_or("").to_string();
            let bullet = match file {
                Some(f) => format!("- [{kind}] {detail} (in `{f}`)"),
                None => format!("- [{kind}] {detail}"),
            };
            // (#1002) File-in-play boost: a caution about a file this dispatch
            // will touch (normalized-path match against `intent`) outranks an
            // engagement-level one.
            let norm = file.map(normalize_path_lexical);
            let is_match = norm.as_deref().is_some_and(|f| intent.contains(f)) as u8;
            // (#1002 + #1001) Freshness: stale only when the firing-time hash is
            // present AND the file is readable now AND the content differs.
            // "Unknown freshness" (no hash / unreadable) stays fresh — never
            // bury what we can't verify (exact-string compare, see #1002 note).
            let fresh = match (code_hash, norm.as_deref()) {
                (Some(h), Some(f)) => match current_file_blake3(workspace_root, f) {
                    Some(cur) => (cur == h) as u8,
                    None => 1,
                },
                _ => 1,
            };
            // `warn` outranks `info`; any other value (incl. a future severity
            // above `warn`) floors to 0 — today only `warn`/`info` are emitted,
            // so revisit this line if a higher severity is ever introduced.
            let rank = if severity == "warn" { 1u8 } else { 0u8 };
            found.push((is_match, fresh, rank, ts, bullet));
        }
    }
    // File-in-play first, then fresh-over-stale, then severity, then most recent
    // (ts is RFC3339 — lexicographic == chronological).
    found.sort_by(|a, b| {
        b.0.cmp(&a.0)
            .then_with(|| b.1.cmp(&a.1))
            .then_with(|| b.2.cmp(&a.2))
            .then_with(|| b.3.cmp(&a.3))
    });
    // (#1011) Deduped + fully ranked, but NOT count-capped here — the
    // proportional budget (`allocate_injected_context`) governs how many land,
    // so a large-window dispatch can carry more than the old flat 10.
    let mut seen = std::collections::HashSet::new();
    let mut out: Vec<String> = Vec::new();
    for (_, _, _, _, bullet) in found {
        if seen.insert(bullet.clone()) {
            out.push(bullet);
        }
    }
    out
}

/// (#994) The operator-authored lessons for this engagement, for the coder
/// brief — the lessons store's READ side (the authored sibling of the
/// auto-derived [`mission_cautions`]). Reads BOTH lessons tiers directly from
/// their SQLite stores (always fresh — the db IS the source, no rebuild): the
/// repo's `<repo>/.darkmux/lessons.db` + the user-global `~/.darkmux/lessons.db`.
/// A dispatch in repo X sees only X's lessons + global; repo Y's never.
///
/// The store is detection-driven (the distiller turns recurring detections into
/// durable lessons), so reads must tolerate concurrent writes — SQLite +
/// best-effort does: a missing/locked/unreadable store reads as "no
/// lessons" and never errors the dispatch (mirrors the cautions +
/// corrections collectors). Fully ranked (file-in-play first), NOT count-capped
/// — the proportional budget (#1011) governs how many land; formatted as bullets
/// (the "why" rides in the body).
fn engagement_lessons(intent: &std::collections::HashSet<String>) -> Vec<String> {
    use crew::lessons;
    let repo_path = lessons::repo_db_path();
    let global_path = lessons::global_db_path();
    let mut entries = lessons::load_entries_best_effort(&repo_path);
    // `$DARKMUX_HOME` collapses both tiers to one path — don't inject twice.
    if global_path != repo_path {
        entries.extend(lessons::load_entries_best_effort(&global_path));
    }
    // (#1002) File-in-play boost: a lesson scoped to a file this dispatch will
    // touch (normalized-path match against `intent`) sorts ahead of
    // engagement-level ones, so the budget keeps the most relevant. Stable sort
    // preserves the store's updated_ts-DESC order within each group. Lessons
    // carry no firing-time hash, so there's no staleness dimension here.
    entries.sort_by_key(|e| {
        let in_play = e
            .file
            .as_deref()
            .map(normalize_path_lexical)
            .is_some_and(|f| intent.contains(&f));
        // `false` < `true`, so negate to put matches first under ascending sort.
        !in_play
    });
    // (#1011) Fully ranked, NOT count-capped — the proportional budget governs
    // how many land.
    entries
        .into_iter()
        .map(|e| {
            let scope = e
                .file
                .as_deref()
                .map(|f| format!(" (in `{f}`)"))
                .unwrap_or_default();
            format!("- {}: {}{}", e.title, e.body, scope)
        })
        .collect()
}

fn sprint_status_label(s: crew::types::SprintStatus) -> &'static str {
    use crew::types::SprintStatus::*;
    match s {
        Planned => "planned",
        Running => "running",
        Complete => "complete",
        Abandoned => "abandoned",
    }
}

fn mission_status_label(s: crew::types::MissionStatus) -> &'static str {
    use crew::types::MissionStatus::*;
    match s {
        Active => "active",
        Closed => "closed",
        Paused => "paused",
    }
}

/// Print a bullet list, or a dim "(none)" when empty — the debrief's
/// section renderer. The collected strings are already bullet-formatted
/// (`- …`) by the caution / correction collectors.
fn print_bullets_or_none(items: &[String]) {
    if items.is_empty() {
        println!("  {}", style::dim("(none)"));
        return;
    }
    for it in items {
        for (i, line) in it.lines().enumerate() {
            // First line carries the collector's own `- ` bullet; indent it
            // two spaces. Continuation lines indent under the bullet.
            if i == 0 {
                println!("  {line}");
            } else {
                println!("    {line}");
            }
        }
    }
}

/// (#1000) The debrief ceremony's gathered raw material for one mission. Owned
/// (not borrowed from the loaded mission/sprint Vecs) so the gather + render are
/// cleanly separable and the gather is unit-testable without stdout capture.
struct DebriefReport {
    mission_id: String,
    mission_description: String,
    mission_status: &'static str,
    /// (sprint_id, first-line description, status label) per sprint.
    sprints: Vec<(String, String, &'static str)>,
    /// Already bullet-formatted by [`mission_cautions`].
    cautions: Vec<String>,
    /// The reviewer's adjudication notes (#849), as recorded.
    corrections: Vec<String>,
}

/// (#1000) Gather the debrief raw material for `mission_id`: the loop
/// pathologies darkmux's detectors flagged across the mission's runs (cautions),
/// the corrections the reviewer recorded (#849), and the mission's sprints + how
/// each ended. READ-ONLY.
///
/// The flow stream IS the mission's durable history (the #557 single-stream
/// doctrine); this reads it scoped to the mission's EXACT dispatch session ids
/// (same `mission-run-<id>-<sprint>` construction as the run path, so a sibling
/// mission whose id is a hyphen-extension never bleeds in). It does NOT assume a
/// coding mission — no git diffs are reconstructed here: for a coding mission
/// the `darkmux-mission-debrief` skill pulls the actual patch with `git show`,
/// and a non-coding mission simply has no coding activity.
fn gather_debrief(mission_id: &str) -> Result<DebriefReport> {
    use crew::loader::{load_missions, load_sprints};
    // (#1011) How many cautions/corrections the retrospective summary shows
    // (the collectors are uncapped now; the dispatch path budgets, the debrief
    // just truncates for readability).
    const DEBRIEF_DISPLAY: usize = 10;
    fleet::validate_identifier("mission_id", mission_id)?;

    let missions = load_missions()?;
    let mission = missions
        .iter()
        .find(|m| m.id == mission_id)
        .ok_or_else(|| {
            anyhow::anyhow!("mission `{mission_id}` not found (check `darkmux mission status`)")
        })?;

    let sprints = load_sprints()?;
    let mission_sprints: Vec<&crew::types::Sprint> = sprints
        .iter()
        .filter(|s| s.mission_id.as_str() == mission_id)
        .collect();

    // The mission's exact dispatch session ids — same construction as `run`,
    // so the collectors scope to THIS mission's sessions (no sibling bleed).
    let mission_session_ids: std::collections::HashSet<String> = mission_sprints
        .iter()
        .map(|s| format!("mission-run-{}-{}", mission_id, s.id))
        .collect();

    Ok(DebriefReport {
        mission_id: mission.id.clone(),
        mission_description: mission.description.clone(),
        mission_status: mission_status_label(mission.status),
        sprints: mission_sprints
            .iter()
            .map(|s| {
                (
                    s.id.clone(),
                    s.description.lines().next().unwrap_or("").trim().to_string(),
                    sprint_status_label(s.status),
                )
            })
            .collect(),
        // (#1002) A debrief is retrospective — no active dispatch, so no
        // files-in-play (empty `intent`); the operator's cwd is the natural
        // "current content" root for the staleness check (a caution whose file
        // has since changed sorts down, which reads fine in a summary too).
        // (#1011) The collectors are no longer self-capped (the dispatch path's
        // budget governs there); a debrief is a readable summary, so cap the
        // display here at the most-relevant `DEBRIEF_DISPLAY` of each.
        cautions: mission_cautions(
            &mission_session_ids,
            &std::collections::HashSet::new(),
            &std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
        )
        .into_iter()
        .take(DEBRIEF_DISPLAY)
        .collect(),
        corrections: mission_adjudication_notes(&mission_session_ids)
            .into_iter()
            .take(DEBRIEF_DISPLAY)
            .collect(),
    })
}

/// (#1000) `darkmux mission debrief <id>` — surface a completed mission's
/// debrief material (cautions + corrections + sprints) for the post-mission
/// review ceremony. `--json` feeds the `darkmux-mission-debrief` skill, which
/// distills durable `lessons` (with the why) for the next crew. The ceremony
/// that turns transient signal into durable lessons — NASA Lessons Learned,
/// applied locally.
pub fn debrief(mission_id: &str, json: bool) -> Result<i32> {
    let report = gather_debrief(mission_id)?;

    if json {
        let sprints_json: Vec<serde_json::Value> = report
            .sprints
            .iter()
            .map(|(id, desc, status)| {
                serde_json::json!({ "id": id, "description": desc, "status": status })
            })
            .collect();
        let out = serde_json::json!({
            "mission": {
                "id": report.mission_id,
                "description": report.mission_description,
                "status": report.mission_status,
            },
            "sprints": sprints_json,
            "cautions": report.cautions,
            "corrections": report.corrections,
        });
        println!("{}", serde_json::to_string_pretty(&out)?);
        return Ok(0);
    }

    println!(
        "{}",
        style::header(&format!("debrief — mission `{}`", report.mission_id))
    );
    let desc = report.mission_description.lines().next().unwrap_or("").trim();
    if !desc.is_empty() {
        println!("  {desc}");
    }
    println!();

    println!("{}", style::header("sprints"));
    if report.sprints.is_empty() {
        println!("  {}", style::dim("(none)"));
    } else {
        for (id, desc, status) in &report.sprints {
            println!(
                "  {} [{}] {}",
                style::accent(id),
                status,
                style::dim(desc)
            );
        }
    }
    println!();

    println!(
        "{}",
        style::header("detected cautions — the loop pathologies the runs got flagged doing")
    );
    print_bullets_or_none(&report.cautions);
    println!();

    println!(
        "{}",
        style::header("adjudication corrections — what the reviewer recorded")
    );
    print_bullets_or_none(&report.corrections);
    println!();

    println!(
        "{}",
        style::dim(
            "distill these into durable lessons (with the why) for the next crew:\n  \
             run the `darkmux-mission-debrief` skill, or:  darkmux lessons add --title <t> --body <b>"
        )
    );
    Ok(0)
}

/// (#1000) Soft nudge printed when a mission is closed — the natural reflection
/// point. Prompts the debrief ceremony so the mission's transient signal
/// (cautions + corrections, which evaporate once the work moves on) becomes
/// durable `lessons` for the next crew. Emits a `Stage::Debrief` flow record
/// marking the prompt in the mission's history. Prints, never blocks (a nudge,
/// not a gate — operator sovereignty #44). The within-mission learning already
/// happened live (corrections + cautions carried sprint→sprint at run time);
/// this is the cross-MISSION lesson-banking step.
pub fn nudge_mission_debrief(mission_id: &str) {
    let _ = flow::record(flow::FlowRecord {
        ts: flow::ts_utc_now(),
        level: flow::Level::Info,
        category: flow::Category::Review,
        tier: flow::Tier::Operator,
        stage: flow::Stage::Debrief,
        action: "mission.debrief.prompt".to_string(),
        handle: mission_id.to_string(),
        sprint_id: None,
        session_id: Some(format!("mission:{mission_id}")),
        source: Some("mission_debrief".to_string()),
        model: None,
        reasoning: None,
        mission_id: Some(mission_id.to_string()),
        machine_id: None,
        machine_uid: None,
        orchestrator: None,
        prev_hash: None,
        hash: None,
        payload: None,
        work_id: None,
        attempt: None,
    });
    println!(
        "{}",
        style::dim(&format!(
            "  mission closed — bank its lessons before the next crew:  \
             darkmux mission debrief {mission_id}"
        ))
    );
}

/// (#817) Does the run's flow trail carry an adjudication note? Scans the
/// TWO lexicographically-newest day files (UTC-rollover safe, same pattern
/// as the /diff endpoint's resolution) for `action=note` matching the
/// session id, with source `adjudication` (the audit-trail channel) OR
/// `orchestrator` (the dashboard channel — accepted so a session-scoped
/// dashboard note also satisfies the nudge). Best-effort: any IO/parse
/// problem reads as "no note" — this only feeds a soft nudge, never a gate.
fn session_has_orchestrator_note(session_id: &str) -> bool {
    let flows_dir = darkmux_types::config_access::flows_dir();
    let Ok(entries) = std::fs::read_dir(&flows_dir) else {
        return false;
    };
    let mut days: Vec<PathBuf> = entries
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().and_then(|x| x.to_str()) == Some("jsonl"))
        .collect();
    days.sort();
    for day in days.iter().rev().take(2) {
        let Ok(raw) = std::fs::read_to_string(day) else { continue };
        for line in raw.lines() {
            let Ok(r) = serde_json::from_str::<serde_json::Value>(line) else { continue };
            let src = r.get("source").and_then(|v| v.as_str());
            if r.get("action").and_then(|v| v.as_str()) == Some("note")
                && (src == Some("adjudication") || src == Some("orchestrator"))
                && r.get("session_id").and_then(|v| v.as_str()) == Some(session_id)
            {
                return true;
            }
        }
    }
    false
}

/// (#817) Soft nudge printed at ship time when a gated sprint is shipping
/// with zero adjudication notes in its trail — the gate is where judgment
/// calls happen, and without a note darkmux's own record of the mission has
/// a hole where the reasoning should be. Prints, never blocks (nudges, not
/// gates — operator sovereignty).
fn nudge_missing_adjudication_note(session_id: &str) {
    if session_has_orchestrator_note(session_id) {
        return;
    }
    println!(
        "{}",
        style::dim(&format!(
            "  no adjudication note in this run's trail — capture it:  darkmux flow note \
             --session-id {session_id} --text \"<verdict · what you overrode · why>\" \
             --source adjudication",
        ))
    );
}

/// (#799) A bash verifier command the runtime classified as FAILED TO RUN —
/// the binary was missing (exit 127), not executable (exit 126), or its
/// toolchain failed to load — so it never actually verified anything. Parsed
/// from the dispatch envelope's `failed_tool_invocations` (stamped by the
/// runtime in #799 part 1). A non-empty list means a coder SIGNOFF claiming
/// "tests pass" may rest on a command that never ran. SOFT signal end to end:
/// surfaced for the adjudicator, never an auto-fail (operator sovereignty #44).
#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
struct FailedVerifier {
    #[serde(default)]
    command: String,
    #[serde(default)]
    reason: String,
}

/// Best-effort parse of `failed_tool_invocations` from the internal runtime's
/// `--json` envelope (the dispatch's stdout). In `--json` mode the runtime
/// prints a single-line JSON envelope to stdout (status goes to stderr), so the
/// whole buffer is the envelope; the last-non-empty-line fallback is pure
/// defense against an unexpected leading line. Returns EMPTY on any parse miss
/// or absent field — a soft signal must never fire a FALSE alarm, so "couldn't
/// tell" reads as "nothing failed."
fn parse_failed_verifiers(envelope_stdout: &str) -> Vec<FailedVerifier> {
    let as_json = |s: &str| serde_json::from_str::<serde_json::Value>(s.trim()).ok();
    let Some(v) = as_json(envelope_stdout).or_else(|| {
        envelope_stdout
            .lines()
            .rev()
            .find(|l| !l.trim().is_empty())
            .and_then(as_json)
    }) else {
        return Vec::new();
    };
    v.get("failed_tool_invocations")
        .and_then(|a| a.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|e| serde_json::from_value::<FailedVerifier>(e.clone()).ok())
                .collect()
        })
        .unwrap_or_default()
}

/// (#799) Prominent gate banner naming the verifier commands that FAILED TO
/// RUN. No-op on an honest run (empty list). Soft — it informs the adjudicator
/// at the gate; it never blocks `mission run` (operator sovereignty #44). The
/// list is what lets the operator cross-check the coder's SIGNOFF: a "tests
/// pass" claim sitting next to "the test command never ran" is the
/// contradiction this exists to surface.
fn print_unverified_banner(failed: &[FailedVerifier]) {
    if failed.is_empty() {
        return;
    }
    println!(
        "\n{}",
        style::warn(&format!(
            "⚠ verification unproven — {} verifier command(s) FAILED TO RUN (never executed, so \
             they verified nothing). A SIGNOFF claiming these passed is contradicted by the \
             runtime's own record:",
            failed.len()
        ))
    );
    for f in failed {
        println!(
            "    {} {}",
            style::accent(&f.command),
            style::dim(&format!("— {}", f.reason))
        );
    }
    println!(
        "  {}",
        style::dim(
            "confirm verification independently before shipping — re-run once the toolchain is \
             fixed, or verify by hand. `mission ship --merge` will HOLD on this until you do."
        )
    );
}

/// (#799) The verifier commands the LATEST run's coder FAILED TO RUN, read back
/// from the flow trail by the run's deterministic session id
/// (`mission-run-<mission>-<sprint>`). `mission run` emits a
/// `mission.run.verification` record (payload `{ failed: [{command, reason}] }`)
/// on EVERY run — empty `failed` on an honest run — so `ship` reads the latest
/// run's status and HOLDs an auto-merge only when that run had failures. The
/// run is a separate process, so the flow trail is the durable handoff (the
/// runtime's out-dir is an ephemeral per-dispatch tempdir ship can't
/// reconstruct). Scans the last 2 days oldest→newest and OVERWRITES `latest` on
/// each match, so a clean re-run's empty record correctly clears a prior dirty
/// run's (latest-wins on a resumed sprint). Best effort: any IO/parse problem,
/// or no record in the recent window, reads as "none" — this soft backstop
/// fails OPEN (the run-time banner is the primary surface). Mirrors
/// `session_has_orchestrator_note`.
fn session_failed_verifiers(session_id: &str) -> Vec<FailedVerifier> {
    let flows_dir = darkmux_types::config_access::flows_dir();
    let Ok(entries) = std::fs::read_dir(&flows_dir) else {
        return Vec::new();
    };
    let mut days: Vec<PathBuf> = entries
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().and_then(|x| x.to_str()) == Some("jsonl"))
        .collect();
    days.sort();
    // Last 2 days, iterated oldest→newest, so a later record overwrites an
    // earlier one and the most recent `mission.run.unverified` for this
    // session wins.
    let recent: Vec<PathBuf> = days.iter().rev().take(2).rev().cloned().collect();
    let mut latest: Vec<FailedVerifier> = Vec::new();
    for day in &recent {
        let Ok(raw) = std::fs::read_to_string(day) else {
            continue;
        };
        for line in raw.lines() {
            let Ok(r) = serde_json::from_str::<serde_json::Value>(line) else {
                continue;
            };
            if r.get("action").and_then(|v| v.as_str()) == Some("mission.run.verification")
                && r.get("session_id").and_then(|v| v.as_str()) == Some(session_id)
            {
                if let Some(arr) = r
                    .get("payload")
                    .and_then(|p| p.get("failed"))
                    .and_then(|f| f.as_array())
                {
                    latest = arr
                        .iter()
                        .filter_map(|e| serde_json::from_value::<FailedVerifier>(e.clone()).ok())
                        .collect();
                }
            }
        }
    }
    latest
}

fn commit_subject(sprint: &crew::types::Sprint) -> String {
    let first = sprint.description.lines().next().unwrap_or("").trim();
    let s = if first.is_empty() {
        format!("darkmux sprint {}", sprint.id)
    } else {
        first.to_string()
    };
    if s.chars().count() > 72 {
        let truncated: String = s.chars().take(69).collect();
        format!("{truncated}...")
    } else {
        s
    }
}

/// PR body — sprint + mission provenance. Authored by the LOCAL coder via
/// `mission run`; the body says so (no frontier/Claude co-author claim — this
/// is local-AI work shipped through darkmux's loop).
fn pr_body(mission: &crew::types::Mission, sprint: &crew::types::Sprint) -> String {
    format!(
        "## {sprint_desc}\n\n\
         Shipped via `darkmux mission ship` — the local dispatch-to-PR loop.\n\n\
         - **Mission:** `{mission_id}` — {mission_desc}\n\
         - **Sprint:** `{sprint_id}`\n\n\
         The implementation was produced by the local-AI coder under \
         `darkmux mission run` and reviewed by the local `code-reviewer` before \
         sign-off. The frontier/operator adjudicated the QA findings at the gate.",
        sprint_desc = sprint.description.lines().next().unwrap_or("").trim(),
        mission_id = mission.id,
        mission_desc = mission.description.lines().next().unwrap_or("").trim(),
        sprint_id = sprint.id,
    )
}

/// (#816) The branch a worktree is actually on (`git rev-parse
/// --abbrev-ref HEAD`). None when the worktree is missing/unreadable or
/// detached — callers fall back to the conventions-computed name.
fn worktree_branch(wt_path: &Path) -> Option<String> {
    let out = Command::new("git")
        .current_dir(wt_path)
        .args(["rev-parse", "--abbrev-ref", "HEAD"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let b = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if b.is_empty() || b == "HEAD" { None } else { Some(b) }
}

/// (#816) Apply a conventions template to the default-computed subject.
/// `what` names the item in the fallback warning. Falls back to the
/// default subject when there's no template, the template needs a ticket
/// the mission doesn't have, or it expands empty.
fn conventioned(
    template: Option<&str>,
    sprint: &crew::types::Sprint,
    mission: &crew::types::Mission,
    what: &str,
) -> String {
    let default = commit_subject(sprint);
    let Some(t) = template else { return default };
    let vars = crate::conventions::Vars {
        ticket: mission.ticket.as_deref(),
        sprint: &sprint.id,
        mission: &mission.id,
        subject: &default,
    };
    match crate::conventions::expand(t, &vars) {
        Some(out) if !out.trim().is_empty() => out,
        _ => {
            eprintln!(
                "darkmux: warning — conventions {what} template couldn't expand for mission `{}` (missing ticket or empty result); using the default",
                mission.id
            );
            default
        }
    }
}

/// (#816) PR body honoring the repo's `pr_body_template` (repo-relative
/// path): the file's content with `{summary}` replaced by the generated
/// darkmux summary, or the summary appended when the placeholder is
/// absent. Missing/unreadable template file warns + falls back.
fn conventioned_pr_body(
    mission: &crew::types::Mission,
    sprint: &crew::types::Sprint,
    conv: Option<&crate::conventions::Conventions>,
    repo_root: &Path,
) -> String {
    let summary = pr_body(mission, sprint);
    let Some(rel) = conv.and_then(|c| c.pr_body_template.as_deref()) else {
        return summary;
    };
    let path = repo_root.join(rel);
    match std::fs::read_to_string(&path) {
        Ok(tpl) if tpl.contains("{summary}") => tpl.replace("{summary}", &summary),
        Ok(tpl) => format!("{tpl}\n\n{summary}"),
        Err(e) => {
            eprintln!(
                "darkmux: warning — conventions pr_body_template {} unreadable ({e}); using the generated body",
                path.display()
            );
            summary
        }
    }
}

/// The **OPEN** PR's URL for `branch`, or `None` when there's no open PR.
///
/// `gh pr view <branch>` falls back to the most-recent CLOSED/MERGED PR when no
/// OPEN one exists. On a deterministic, reusable branch name
/// (`darkmux/<sprint-id>`) that could hand back a STALE merged PR; the ship
/// path would then skip `gh pr create` and later verify merge-state against
/// that stale URL (#844), wrongly OK-ing a teardown of un-merged work. The
/// `select(.state=="OPEN")` jq filter closes that seam: a recycled branch whose
/// only PR is merged/closed yields `None`, so ship falls through to
/// `gh pr create` and gets a FRESH PR identity to verify against.
fn existing_pr_url(dir: &Path, branch: &str) -> Option<String> {
    let out = Command::new("gh")
        .current_dir(dir)
        .args([
            "pr", "view", branch, "--json", "url,state", "-q",
            "select(.state==\"OPEN\") | .url",
        ])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let url = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if url.is_empty() { None } else { Some(url) }
}

/// Verify the branch's CI is GREEN — every check `conclusion == SUCCESS`, not
/// merely completed (per the merge-gate discipline). `--watch` blocks until
/// checks finish; we then re-read the rollup and require all-SUCCESS.
///
/// Two deliberate behaviors: (1) `gh pr checks --watch` has no timeout — a
/// hung check blocks `ship` until it resolves or the operator interrupts
/// (Ctrl-C leaves the PR open, nothing merged). (2) an EMPTY rollup (no checks
/// configured) is treated as NOT green — a merge gate shouldn't wave work
/// through just because a repo has no CI. So `--merge` requires CI to be both
/// configured AND passing.
fn ci_is_green(dir: &Path, branch: &str) -> Result<bool> {
    // Block until checks complete (ignore the exit code — a red check makes
    // `gh pr checks --watch` exit non-zero; we judge from the rollup below).
    let _ = Command::new("gh")
        .current_dir(dir)
        .args(["pr", "checks", branch, "--watch", "--interval", "30"])
        .status();
    let out = Command::new("gh")
        .current_dir(dir)
        .args([
            "pr",
            "view",
            branch,
            "--json",
            "statusCheckRollup",
            "-q",
            ".statusCheckRollup[].conclusion",
        ])
        .output()
        .context("reading CI rollup via gh")?;
    if !out.status.success() {
        bail!(
            "could not read CI status: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    let conclusions: Vec<String> = String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(|l| l.trim().to_string())
        .filter(|l| !l.is_empty())
        .collect();
    // All checks must be SUCCESS. An empty list (no checks configured) is
    // treated as NOT green — refuse to claim green when nothing ran.
    Ok(!conclusions.is_empty() && conclusions.iter().all(|c| c == "SUCCESS"))
}

/// `darkmux mission ship` — the post-sign-off completion of the dispatch-to-PR
/// loop. Commits the worktree's work, pushes the branch, opens (or reuses) the
/// PR, and STOPS at the PR by default. `--wait-ci` blocks on CI; `--merge`
/// (opt-in, green-gated) squash-merges, flips the sprint to Complete, and
/// tears the worktree down. **Never auto-merges** — `--merge` is the operator/
/// frontier's explicit sign-off act. Returns `0` on success, `1` on a refused
/// merge (CI not green), `2` when the PR merged but the sprint couldn't be
/// marked Complete (inconsistent state — needs manual reconcile), `3` when
/// `--merge` is HELD because the run had verifier commands that failed to run
/// (#799 — review the SIGNOFF, then merge manually or re-run after fixing).
pub fn ship(
    mission_id: &str,
    sprint_id: Option<&str>,
    base: &str,
    wait_ci: bool,
    merge: bool,
) -> Result<i32> {
    use crew::loader::{load_missions, load_sprints};

    fleet::validate_identifier("mission_id", mission_id)?;
    if let Some(s) = sprint_id {
        fleet::validate_identifier("--sprint", s)?;
    }

    let missions = load_missions()?;
    let mission = missions
        .iter()
        .find(|m| m.id == mission_id)
        .ok_or_else(|| anyhow::anyhow!("mission `{mission_id}` not found"))?
        .clone();
    let sprints = load_sprints()?;
    let sprint = resolve_sprint(&sprints, mission_id, sprint_id)?;

    // A Complete sprint is terminal — a prior `--merge` already shipped it
    // (and tore down its worktree). Re-shipping would duplicate-PR or churn;
    // refuse rather than confuse.
    if matches!(sprint.status, crew::types::SprintStatus::Complete) {
        bail!(
            "sprint `{}` is already Complete (terminal) — nothing to ship.",
            sprint.id
        );
    }

    let root = repo_root()?;
    let wt_path = worktree_path(&root, &sprint.id);
    let conv = crate::conventions::load(&root);
    // (#816) Ship pushes the branch the worktree is ACTUALLY on — created at
    // `mission run` time — not a recomputation. If conventions.json changed
    // between run and ship, recomputing would target a branch that doesn't
    // exist (QA drift finding). The computed name is only the fallback for
    // a worktree whose HEAD can't be read.
    let branch = worktree_branch(&wt_path)
        .unwrap_or_else(|| conventions_branch(&sprint, &mission, conv.as_ref()));
    let session_id = format!("mission-run-{}-{}", mission_id, sprint.id);

    if !wt_path.exists() {
        bail!(
            "no worktree at {} — run `darkmux mission run {mission_id} --sprint {}` first.",
            wt_path.display(),
            sprint.id
        );
    }

    println!(
        "{}",
        style::header(&format!("▶ mission ship — {} · sprint {}", mission_id, sprint.id))
    );

    // 1. Commit the worktree's work (the coder's changes + any operator edits
    //    made while resolving findings). Stage everything, commit only if
    //    there's something staged; if nothing's staged but the branch is
    //    already ahead of base, proceed to push the existing commits.
    git_in(&wt_path, &["add", "-A"])?;
    let nothing_staged = git_in(&wt_path, &["diff", "--cached", "--quiet"])?
        .status
        .success();
    let ahead = git_in(&wt_path, &["rev-list", "--count", &format!("{base}..HEAD")])?;
    let commits_ahead: u32 = String::from_utf8_lossy(&ahead.stdout)
        .trim()
        .parse()
        .unwrap_or(0);
    if nothing_staged && commits_ahead == 0 {
        bail!(
            "nothing to ship — worktree at {} has no changes vs `{base}` and no commits ahead.",
            wt_path.display()
        );
    }
    if !nothing_staged {
        let subject = conventioned(
        conv.as_ref().and_then(|c| c.commit_subject_template.as_deref()),
        &sprint, &mission, "commit subject",
    );
        let msg = format!(
            "{subject}\n\nAuthored via `darkmux mission run` (local-AI coder, sprint {}).",
            sprint.id
        );
        // (#834) Commit under the declared bot identity when the repo's
        // conventions name one (sets author AND committer). Without it, a repo
        // lacking a local git identity commits under the operator's GLOBAL
        // name — silently breaking bot-authorship/SoD. When the repo IS managed
        // (a conventions FILE exists, parse or not) but no identity resolves,
        // surface what the commit will land under + how to pin it. Surface,
        // never block (operator-sovereignty).
        let author = conv
            .as_ref()
            .and_then(|c| c.commit_author.as_ref())
            .filter(|a| a.config_args().is_some());
        let out = if let Some(a) = author {
            commit_with_identity(&wt_path, a, &msg)?
        } else {
            if crate::conventions::file_present(&root) {
                println!(
                    "{}",
                    style::dim(&format!(
                        "  committing as {} — declare commit_author in .darkmux/conventions.json \
                         if this repo needs a bot identity",
                        resolved_git_identity(&wt_path)
                    ))
                );
            }
            git_in(&wt_path, &["commit", "-m", &msg])?
        };
        if !out.status.success() {
            bail!("git commit failed: {}", String::from_utf8_lossy(&out.stderr).trim());
        }
        let as_who = author.map(|a| format!(" as {}", a.name.trim())).unwrap_or_default();
        println!("{}", style::success(&format!("✓ committed{as_who}: {subject}")));
    } else {
        println!(
            "{}",
            style::dim(&format!("{commits_ahead} commit(s) already ahead of {base} — nothing new to commit"))
        );
    }

    // 2. Push the branch (sets upstream).
    let out = git_in(&wt_path, &["push", "-u", "origin", &branch])?;
    if !out.status.success() {
        bail!(
            "git push failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    println!("{}", style::success(&format!("✓ pushed {branch}")));

    // 3. Open (or reuse) the PR.
    let pr_url = match existing_pr_url(&wt_path, &branch) {
        Some(url) => {
            println!("{}", style::dim(&format!("PR already open: {url}")));
            url
        }
        None => {
            let title = conventioned(
                conv.as_ref().and_then(|c| c.pr_title_template.as_deref()),
                &sprint, &mission, "PR title",
            );
            let body = conventioned_pr_body(&mission, &sprint, conv.as_ref(), &root);
            let mut args: Vec<&str> = vec![
                "pr", "create", "--base", base, "--head", &branch, "--title", &title,
                "--body", &body,
            ];
            // (#816) Repo-declared labels — each must exist in the repo
            // (gh errors otherwise; surfaced verbatim below).
            let labels: Vec<&str> = conv
                .as_ref()
                .map(|c| c.pr_labels.iter().map(String::as_str).collect())
                .unwrap_or_default();
            for l in &labels {
                // (#1111) Argument-injection guard on the gh subprocess: a label
                // that's empty or starts with `-` would be parsed by gh as a flag
                // (e.g. `--label --config` injecting a gh option). Branch names are
                // already validated; labels were the gap. Skip + warn rather than
                // fail the whole dispatch over a bad label.
                if !crate::conventions::valid_label(l) {
                    eprintln!(
                        "darkmux mission run: skipping unsafe pr label {l:?} \
                         (empty or starts with `-` — would parse as a gh flag)"
                    );
                    continue;
                }
                args.push("--label");
                args.push(l);
            }
            let out = Command::new("gh")
                .current_dir(&wt_path)
                .args(&args)
                .output()
                .context("running `gh pr create` — is `gh` on PATH?")?;
            if !out.status.success() {
                bail!(
                    "`gh pr create` failed: {}",
                    String::from_utf8_lossy(&out.stderr).trim()
                );
            }
            let url = String::from_utf8_lossy(&out.stdout).trim().to_string();
            println!("{}", style::success(&format!("✓ opened PR: {url}")));
            url
        }
    };

    emit_run_record(
        flow::Level::Info,
        "mission.run.ship",
        mission_id,
        &sprint.id,
        &session_id,
        serde_json::json!({ "branch": branch, "pr_url": pr_url, "base": base }),
    );

    // (#799 part 2) Verifier-fabrication backstop — checked BEFORE the CI gate
    // so a held merge doesn't first sit through `ci_is_green`'s blocking watch.
    // If the run's coder had bash verifier commands that FAILED TO RUN, a
    // SIGNOFF's "tests pass" claim may rest on a command that never executed.
    // HOLD the auto-merge for human review: don't merge, don't tear down, never
    // auto-fail (operator sovereignty #44). The PR is already open and the
    // worktree intact — nothing is discarded; the operator reviews + merges
    // manually after confirming verification, or re-runs once the toolchain is
    // fixed. Soft by design: `--merge` is the ONLY path this gates (a default
    // stop-at-PR `ship` is untouched), and the run-time banner surfaced it once
    // already.
    if merge {
        let unverified = session_failed_verifiers(&session_id);
        if !unverified.is_empty() {
            eprintln!(
                "{}",
                style::error(&format!(
                    "✗ holding the auto-merge of {branch} — {} verifier command(s) FAILED TO RUN \
                     during the run, so the coder's SIGNOFF may claim verification that never \
                     happened:",
                    unverified.len()
                ))
            );
            for f in &unverified {
                eprintln!(
                    "    {} {}",
                    style::accent(&f.command),
                    style::dim(&format!("— {}", f.reason))
                );
            }
            eprintln!(
                "{}",
                style::warn(&format!(
                    "  the PR is open ({pr_url}) and the worktree is intact — nothing was \
                     discarded. Review the diff + SIGNOFF; if verification really is sound, merge \
                     manually (`gh pr merge {branch} --squash`). If the toolchain was broken, fix \
                     it and re-run `darkmux mission run {mission_id} --sprint {}`.",
                    sprint.id
                ))
            );
            emit_run_record(
                flow::Level::Warn,
                "mission.run.ship.held",
                mission_id,
                &sprint.id,
                &session_id,
                serde_json::json!({
                    "reason": "verification-unproven",
                    "failed": unverified,
                    "count": unverified.len(),
                    "pr_url": pr_url,
                }),
            );
            return Ok(3);
        }
    }

    // 4. CI gate (for --wait-ci or --merge).
    let mut green = false;
    if wait_ci || merge {
        println!("\n{}", style::header("▶ watching CI…"));
        green = ci_is_green(&wt_path, &branch)?;
        if green {
            println!("{}", style::success("✓ CI green"));
        } else {
            eprintln!("{}", style::warn("⚠ CI is not green (or no checks ran)"));
        }
    }

    // 5. Merge — opt-in, green-gated. NEVER automatic.
    if merge {
        if !green {
            // `green` is false both when a check failed AND when no checks
            // ran at all (ci_is_green treats empty as not-green — the safe
            // default for a merge gate). Name both so the operator knows
            // which it is.
            eprintln!(
                "{}",
                style::error(&format!(
                    "✗ refusing to merge {branch} — CI is not green (a check failed, or no \
                     checks ran; `--merge` requires configured + passing CI). Resolve, re-push, \
                     then re-run `darkmux mission ship {mission_id} --sprint {} --merge`.",
                    sprint.id
                ))
            );
            return Ok(1);
        }
        let out = Command::new("gh")
            .current_dir(&wt_path)
            .args(["pr", "merge", &branch, "--squash", "--delete-branch"])
            .output()
            .context("running `gh pr merge`")?;
        if !out.status.success() {
            // gh performs the squash-merge + remote-branch deletion via the API
            // FIRST, then runs local post-merge git ops (checkout base + delete
            // the local branch). In a mission worktree the base (`main`) is
            // checked out in the primary worktree, so gh's local `git checkout
            // main` fatals — and gh exits non-zero even though the REMOTE merge
            // already landed (#844). Treating that as a total failure used to
            // skip sprint-complete + teardown → silent drift (merged PR, sprint
            // stuck Running, orphaned worktree). So verify the PR's ACTUAL
            // state: only bail if it truly didn't merge.
            match pr_merge_state(&root, &pr_url) {
                MergeState::Merged => {
                    eprintln!(
                        "{}",
                        style::warn(&format!(
                            "gh exited non-zero after merging, but the PR is merged on the remote — \
                             gh's local post-merge sync conflicts with the worktree layout (harmless; \
                             continuing teardown). gh stderr: {}",
                            String::from_utf8_lossy(&out.stderr).trim()
                        ))
                    );
                }
                MergeState::NotMerged => {
                    bail!(
                        "`gh pr merge` failed and the PR is not merged: {}",
                        String::from_utf8_lossy(&out.stderr).trim()
                    );
                }
                MergeState::Unknown => {
                    // The merge MAY have landed (the worktree-layout local-sync
                    // failure looks identical to this), but the verifying view
                    // couldn't confirm it. Don't assert "not merged" — point the
                    // operator at the PR and the one-command reconcile (#844).
                    bail!(
                        "`gh pr merge` exited non-zero and the PR's merge state could not be \
                         confirmed — check {pr_url}. If it DID merge, reconcile with \
                         `darkmux sprint complete {}` and `git worktree remove --force {}`. \
                         gh stderr: {}",
                        sprint.id,
                        wt_path.display(),
                        String::from_utf8_lossy(&out.stderr).trim()
                    );
                }
            }
        }
        println!("{}", style::success(&format!("✓ merged {branch} (squash)")));

        // Flip the sprint Complete + tear down the worktree. The merge is
        // already irreversible, so a sprint_complete failure can't roll it
        // back — but it leaves merged-PR-but-Running-sprint, so we must NOT
        // claim a clean "loop closed". Track the outcome and exit non-zero
        // with a reconcile pointer if completion didn't take.
        let complete_ok = match crew::lifecycle::sprint_complete(&sprint.id) {
            Ok(_) => {
                println!("{}", style::success(&format!("✓ sprint {} → Complete", sprint.id)));
                true
            }
            Err(e) => {
                eprintln!(
                    "{}",
                    style::error(&format!("✗ sprint_complete({}) failed: {e:#}", sprint.id))
                );
                false
            }
        };
        // The branch was deleted by --delete-branch; remove the now-orphaned
        // worktree (force — its branch ref is gone). Warn on failure so
        // orphaned files don't linger silently.
        match git_in(&root, &["worktree", "remove", "--force", &wt_path.to_string_lossy()]) {
            Ok(o) if o.status.success() => {
                println!("{}", style::dim(&format!("worktree {} removed", wt_path.display())))
            }
            Ok(o) => eprintln!(
                "{}",
                style::warn(&format!(
                    "worktree removal failed ({}) — run `git worktree prune` / remove {} manually.",
                    String::from_utf8_lossy(&o.stderr).trim(),
                    wt_path.display()
                ))
            ),
            Err(e) => eprintln!(
                "{}",
                style::warn(&format!("worktree removal errored: {e:#} — remove {} manually.", wt_path.display()))
            ),
        }
        // gh's `--delete-branch` removed the REMOTE branch via API, but its
        // local-branch deletion rode the same post-merge sync that fails under
        // the worktree layout (#844). With the worktree (which pinned the
        // branch) now gone, reap the local branch ourselves so shipped sprints
        // don't accrete dead `darkmux/<sprint>` refs. Safe unconditionally:
        // if gh already deleted it, `-D` exits 1 (swallowed); if the worktree
        // removal above FAILED, the branch is still pinned and git `-D` refuses
        // outright — so this never orphan-kills a branch holding live work.
        let _ = git_in(&root, &["branch", "-D", &branch]);

        emit_run_record(
            flow::Level::Info,
            "mission.run.ship.merged",
            mission_id,
            &sprint.id,
            &session_id,
            serde_json::json!({ "pr_url": pr_url, "sprint_completed": complete_ok }),
        );
        if !complete_ok {
            eprintln!(
                "{}",
                style::error(&format!(
                    "PR was MERGED but sprint `{}` could not be marked Complete — state is \
                     inconsistent. Reconcile with `darkmux sprint complete {}`.",
                    sprint.id, sprint.id
                ))
            );
            return Ok(2);
        }
        println!("\n{}", style::success("✓ sprint shipped + merged. Loop closed."));
        // (#807/#817) The arc just concluded — soft-nudge if the run's trail
        // has no adjudication note (session-id pre-filled scaffold), then cue
        // the DASHBOARD note: the operator-facing card line. The placeholder
        // is the style brief — positive, plain-language, easy to digest
        // (operator-specified voice; this is encouragement infrastructure,
        // not a changelog).
        nudge_missing_adjudication_note(&session_id);
        println!(
            "{}",
            style::dim(
                "  then a line for the operator's dashboard:  darkmux flow note \
                 --text \"<1-2 upbeat plain-language lines: what the crew got done + \
                 keep-going energy. no jargon, no file paths>\" --source orchestrator",
            )
        );
        return Ok(0);
    }

    // Default: stop at the PR. Merge stays the operator/frontier's explicit act.
    println!(
        "\n{}",
        style::success(&format!("✓ PR ready: {pr_url}"))
    );
    println!(
        "  {}",
        style::dim(&format!(
            "review CI, then merge. To finish via darkmux after green: \
             darkmux mission ship {mission_id} --sprint {} --merge",
            sprint.id
        ))
    );
    // (#817) Stop-at-PR exit gets the same soft nudge as the merge exit —
    // the adjudication happened at the gate either way.
    nudge_missing_adjudication_note(&session_id);
    Ok(0)
}

/// One-line tokens-off-meter readout. Tokens only — the operator multiplies
/// by their own per-token rate (no currency in product, by design).
fn print_token_line(t: &crew::dispatch_internal::TokenTotals) {
    println!(
        "  {} {} {}",
        style::dim("tokens off-meter:"),
        style::accent(&format!("{}", t.total())),
        style::dim(&format!("({} prompt + {} completion)", t.prompt, t.completion))
    );
}

/// Render the QA verdict + findings with severity coloring.
fn print_review_summary(review: &crate::sprint_cli::SprintReviewOutput) {
    let verdict_styled = match review.verdict.as_str() {
        "clean" => style::success("clean"),
        "flags-only" => style::warn("flags-only"),
        "blockers" => style::error("blockers"),
        other => other.to_string(),
    };
    println!(
        "  {} {}  {} files changed · {} findings (block {}, flag {}, nit {})",
        style::dim("verdict:"),
        verdict_styled,
        review.diff_files_changed,
        review.total_findings,
        review.by_severity.block,
        review.by_severity.flag,
        review.by_severity.nit,
    );
    for f in &review.findings {
        let marker = match f.severity.as_str() {
            "BLOCK" | "block" => style::error("✗ BLOCK"),
            "FLAG" | "flag" => style::warn("⚠ FLAG"),
            _ => style::dim("· nit"),
        };
        println!("    {} {}", marker, f.text);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crew::types::{Sprint, SprintStatus};

    /// (#816) conventions_branch: template + ticket → conventioned ref;
    /// ticketless mission or invalid expansion → darkmux default (soft
    /// fallback, never an error).
    #[test]
    fn conventions_branch_expands_and_falls_back() {
        let s = sprint("s1-fix", "m1", &[], SprintStatus::Planned);
        let mut m = mission("m1", "desc");
        let conv: crate::conventions::Conventions =
            serde_json::from_str(r#"{"branch_template":"{ticket}/{sprint}"}"#).unwrap();
        // ticketless → default
        assert_eq!(conventions_branch(&s, &m, Some(&conv)), "darkmux/s1-fix");
        // ticketed → conventioned
        m.ticket = Some("SYS-2598".into());
        assert_eq!(conventions_branch(&s, &m, Some(&conv)), "SYS-2598/s1-fix");
        // no conventions at all → default
        assert_eq!(conventions_branch(&s, &m, None), "darkmux/s1-fix");
        // template expanding to an invalid ref → default
        let bad: crate::conventions::Conventions =
            serde_json::from_str(r#"{"branch_template":"-{sprint}"}"#).unwrap();
        assert_eq!(conventions_branch(&s, &m, Some(&bad)), "darkmux/s1-fix");
    }

    /// (#815) With a mission-level source_input, the coder brief carries the
    /// compiled description AND the verbatim operator prose under the
    /// provenance-tagged block; without one (hand-authored / pre-#815
    /// missions) the brief is the bare description, unchanged.
    #[test]
    fn coder_brief_appends_verbatim_source_when_present() {
        let s = sprint("s1", "m1", &[], SprintStatus::Planned);
        let mut m = mission("m1", "compiled summary");
        m.source_input = Some("EXACT placeholder: 'APIM Key Name'. Do NOT rename fields.".into());
        let brief = coder_brief(&s, &m, &[], &[], &[]);
        assert!(brief.starts_with("desc s1"), "compiled description leads");
        assert!(brief.contains("<operator-source-input>"), "provenance tag present");
        assert!(brief.contains("Do NOT rename fields."), "verbatim constraint survives");
        assert!(brief.contains("THIS text is authoritative"), "authority statement present");
        // The preamble must read as clean prose — no literal space-runs from
        // string-continuation mistakes (QA caught exactly this on the first
        // cut; the model-facing text is the product here).
        assert!(
            !brief.contains("  "),
            "brief preamble contains a literal space-run: {brief:?}"
        );
        assert!(brief.contains("unabridged request that produced this sprint"));
    }

    #[test]
    fn coder_brief_is_bare_description_without_source() {
        let s = sprint("s1", "m1", &[], SprintStatus::Planned);
        let m = mission("m1", "compiled summary");
        assert_eq!(coder_brief(&s, &m, &[], &[], &[]), "desc s1");
        // Whitespace-only source_input behaves as absent.
        let mut m2 = mission("m1", "compiled summary");
        m2.source_input = Some("   \n ".into());
        assert_eq!(coder_brief(&s, &m2, &[], &[], &[]), "desc s1");
    }

    #[test]
    fn coder_brief_injects_prior_corrections() {
        let s = sprint("s1", "m1", &[], SprintStatus::Planned);
        let m = mission("m1", "compiled summary");
        let corrections = vec![
            "Do NOT rename the APIM key field.".to_string(),
            "The verify command is `cargo test -p foo`, not the workspace default.".to_string(),
        ];
        let brief = coder_brief(&s, &m, &[], &corrections, &[]);
        assert!(brief.starts_with("desc s1"), "base description leads: {brief:?}");
        assert!(
            brief.contains("<prior-adjudication-corrections>"),
            "corrections block present: {brief:?}"
        );
        assert!(brief.contains("- Do NOT rename the APIM key field."), "{brief:?}");
        assert!(brief.contains("- The verify command is `cargo test -p foo`"), "{brief:?}");
        // (#453) Corrections are framed as findings-to-verify, not directives —
        // the reframe of the prior "Honor them — do not re-make these mistakes"
        // anchoring framing. Assert the verify-against-workspace framing is present.
        assert!(
            brief.contains("not a fact about your current workspace"),
            "hypothesis-to-verify framing present: {brief:?}"
        );
        assert!(
            !brief.contains("Honor them"),
            "old anchoring framing must be gone: {brief:?}"
        );
        // Injected preamble prose must read clean — no literal space-runs from
        // string-continuation slips (the source-input block has the same guard;
        // the test notes here are space-run-free, so this covers the framing).
        assert!(!brief.contains("  "), "injected block has a space-run: {brief:?}");
        // Empty corrections leave the brief unchanged — the no-op the dispatch
        // hits on an honest first run with no prior adjudication.
        assert_eq!(coder_brief(&s, &m, &[], &[], &[]), "desc s1");
    }

    /// (#994 retrieve+inject) The detected-cautions block injects independently
    /// of the corrections block (either / both / neither), names the firing's
    /// file as the "where", carries the findings-to-verify framing (#453), and
    /// orders authored corrections before auto-detected cautions.
    #[test]
    fn coder_brief_injects_detected_cautions() {
        let s = sprint("s1", "m1", &[], SprintStatus::Planned);
        let m = mission("m1", "compiled summary");
        let cautions = vec![
            "- [cycle] `edit` called 3× in the last 10 tool calls (in `src/x.rs`)".to_string(),
            "- [reasoning-loop] same reasoning repeated 3× in 6 turns".to_string(),
        ];

        // Cautions alone (no corrections).
        let brief = coder_brief(&s, &m, &[], &[], &cautions);
        assert!(brief.starts_with("desc s1"), "base description leads: {brief:?}");
        assert!(brief.contains("<detected-cautions>"), "cautions block present: {brief:?}");
        assert!(brief.contains("- [cycle] `edit` called 3×"), "{brief:?}");
        assert!(brief.contains("(in `src/x.rs`)"), "the file 'where' survives: {brief:?}");
        assert!(
            brief.contains("not facts about your current workspace"),
            "findings-to-verify framing present: {brief:?}"
        );
        assert!(
            !brief.contains("<prior-adjudication-corrections>"),
            "no corrections block when corrections are empty: {brief:?}"
        );
        // Model-facing prose must read clean — no string-continuation space-runs.
        assert!(!brief.contains("  "), "cautions block has a space-run: {brief:?}");

        // Both blocks coexist; authored corrections precede auto-detected cautions.
        let corrections = vec!["Do not rename the field.".to_string()];
        let both = coder_brief(&s, &m, &[], &corrections, &cautions);
        let corr_at = both.find("<prior-adjudication-corrections>").unwrap();
        let caut_at = both.find("<detected-cautions>").unwrap();
        assert!(corr_at < caut_at, "corrections (authored) precede detected cautions: {both:?}");

        // Neither → bare description.
        assert_eq!(coder_brief(&s, &m, &[], &[], &[]), "desc s1");
    }

    /// (#994) The lessons block injects independently, carries the FOLLOW
    /// framing (authoritative, not verify — distinct from cautions), and orders
    /// base → lessons → corrections → cautions.
    #[test]
    fn coder_brief_injects_lessons_authoritative_and_first() {
        let s = sprint("s1", "m1", &[], SprintStatus::Planned);
        let m = mission("m1", "compiled summary");
        let lessons =
            vec!["- American English: house style across all work, no British spellings.".to_string()];
        let corrections = vec!["Do not rename the field.".to_string()];
        let cautions = vec!["- [cycle] looped on src/x.rs".to_string()];

        // Lessons alone.
        let brief = coder_brief(&s, &m, &lessons, &[], &[]);
        assert!(brief.starts_with("desc s1"), "task leads: {brief:?}");
        assert!(brief.contains("<lessons>"), "lessons block present: {brief:?}");
        assert!(brief.contains("American English"), "{brief:?}");
        assert!(
            brief.contains("Treat them as authoritative"),
            "FOLLOW framing present (distinct from cautions' verify framing): {brief:?}"
        );
        assert!(!brief.contains("  "), "lessons block has a space-run: {brief:?}");

        // All three present: base → lessons → corrections → cautions.
        let all = coder_brief(&s, &m, &lessons, &corrections, &cautions);
        let less_at = all.find("<lessons>").unwrap();
        let corr_at = all.find("<prior-adjudication-corrections>").unwrap();
        let caut_at = all.find("<detected-cautions>").unwrap();
        assert!(
            less_at < corr_at && corr_at < caut_at,
            "authored lessons lead, then corrections, then auto-detected cautions: {all:?}"
        );

        // Empty lessons → no block.
        assert!(!coder_brief(&s, &m, &[], &corrections, &cautions).contains("<lessons>"));
    }

    /// (#994) `engagement_lessons` reads the lessons.db store and formats
    /// entries as bullets — the file scope rendered as the "where", the why in
    /// the body. `#[serial]` — mutates DARKMUX_HOME (which collapses both
    /// lessons tiers to one db, so the read is exercised single-store).
    #[test]
    #[serial_test::serial]
    fn engagement_lessons_reads_lessons_db() {
        let tmp = tempfile::TempDir::new().unwrap();
        let prev = std::env::var("DARKMUX_HOME").ok();
        // SAFETY: serialized via #[serial]; restored below.
        unsafe { std::env::set_var("DARKMUX_HOME", tmp.path()) };

        {
            let conn = crew::lessons::open_at(&crew::lessons::repo_db_path()).unwrap();
            crew::lessons::add(&conn, "American English", "house style across all work", None, None)
                .unwrap();
            crew::lessons::add(
                &conn,
                "Bound retries",
                "the loop entrenches its first answer",
                Some("loop.rs"),
                None,
            )
            .unwrap();
        }
        let conv = engagement_lessons(&std::collections::HashSet::new());

        unsafe {
            match prev {
                Some(v) => std::env::set_var("DARKMUX_HOME", v),
                None => std::env::remove_var("DARKMUX_HOME"),
            }
        }

        assert_eq!(conv.len(), 2, "both entries injected (tiers collapse under DARKMUX_HOME): {conv:?}");
        assert!(
            conv.iter().any(|c| c.contains("American English") && c.contains("house style")),
            "{conv:?}"
        );
        assert!(
            conv.iter().any(|c| c.contains("Bound retries") && c.contains("(in `loop.rs`)")),
            "file scope rendered as the 'where': {conv:?}"
        );
    }

    /// (#1002) A lesson scoped to a file this dispatch will touch sorts ahead of
    /// an engagement-level one, even when the engagement-level lesson is newer
    /// (so the boost — not mere recency — is what flips the order). `#[serial]`
    /// — mutates DARKMUX_HOME.
    #[test]
    #[serial_test::serial]
    fn engagement_lessons_boosts_file_in_play() {
        let tmp = tempfile::TempDir::new().unwrap();
        let prev = std::env::var("DARKMUX_HOME").ok();
        unsafe { std::env::set_var("DARKMUX_HOME", tmp.path()) };
        {
            let conn = crew::lessons::open_at(&crew::lessons::repo_db_path()).unwrap();
            // file-scoped lesson added FIRST (older updated_ts)...
            crew::lessons::add(&conn, "Target rule", "why", Some("./src/target.rs"), None).unwrap();
            // ...engagement-level lesson added SECOND (newer) — by recency this
            // would come first; the file-in-play boost must put the target first.
            crew::lessons::add(&conn, "House style", "applies everywhere", None, None).unwrap();
        }
        let intent: std::collections::HashSet<String> =
            ["src/target.rs"].iter().map(|s| s.to_string()).collect();
        let got = engagement_lessons(&intent);
        unsafe {
            match prev {
                Some(v) => std::env::set_var("DARKMUX_HOME", v),
                None => std::env::remove_var("DARKMUX_HOME"),
            }
        }
        assert_eq!(got.len(), 2);
        assert!(
            got[0].contains("Target rule"),
            "file-in-play lesson ranks first despite being older: {got:?}"
        );
    }

    /// (#1004) The loop-lab A/B context builder wraps the repo's authored
    /// lessons in the real `<lessons>` block (no mission → no cautions). The
    /// block is the same one a coder dispatch would inject (shared
    /// `append_injected_blocks`). `#[serial]` — mutates DARKMUX_HOME.
    #[test]
    #[serial_test::serial]
    fn injected_context_for_lab_builds_the_lessons_block() {
        let tmp = tempfile::TempDir::new().unwrap();
        let prev = std::env::var("DARKMUX_HOME").ok();
        unsafe { std::env::set_var("DARKMUX_HOME", tmp.path()) };
        {
            let conn = crew::lessons::open_at(&crew::lessons::repo_db_path()).unwrap();
            crew::lessons::add(&conn, "American English", "house style across all work", None, None)
                .unwrap();
        }
        let ctx = injected_context_for_lab(None, tmp.path(), None, None);
        unsafe {
            match prev {
                Some(v) => std::env::set_var("DARKMUX_HOME", v),
                None => std::env::remove_var("DARKMUX_HOME"),
            }
        }
        assert!(ctx.starts_with("<lessons>"), "real lessons block, no leading blanks: {ctx:?}");
        assert!(ctx.contains("American English") && ctx.contains("house style"), "{ctx:?}");
        assert!(!ctx.contains("<detected-cautions>"), "no mission → no cautions block: {ctx:?}");
    }

    /// (#994 retrieve+inject) The caution reader: collects detector telemetry
    /// firings for a mission's EXACT dispatch session ids, deduped + ranked
    /// severity-then-recency, naming the firing's file. Excludes non-detector
    /// telemetry, non-telemetry categories, and sibling missions (same
    /// exact-set scope + sibling-bleed guard as the adjudication notes).
    /// `#[serial]` — mutates the shared DARKMUX_FLOWS_DIR.
    #[test]
    #[serial_test::serial]
    fn mission_cautions_filters_scopes_and_ranks() {
        let tmp = tempfile::TempDir::new().unwrap();
        std::fs::write(
            tmp.path().join("2026-06-22.jsonl"),
            concat!(
                // mission `auth`, s1 — a file-keyed cycle (warn)
                r#"{"ts":"2026-06-22T10:00:00Z","category":"telemetry","source":"detector","session_id":"mission-run-auth-s1","handle":"coder","payload":{"kind":"cycle","severity":"warn","detail":"`edit` called 3×","area":{"files":["src/x.rs"]}}}"#, "\n",
                // `auth`, s2 — an info-severity firing (must rank below warn)
                r#"{"ts":"2026-06-22T11:00:00Z","category":"telemetry","source":"detector","session_id":"mission-run-auth-s2","handle":"coder","payload":{"kind":"intra-turn-stall","severity":"info","detail":"runaway turn recovered"}}"#, "\n",
                // exact duplicate of the cycle — must not repeat
                r#"{"ts":"2026-06-22T11:30:00Z","category":"telemetry","source":"detector","session_id":"mission-run-auth-s1","handle":"coder","payload":{"kind":"cycle","severity":"warn","detail":"`edit` called 3×","area":{"files":["src/x.rs"]}}}"#, "\n",
                // SIBLING mission `auth-v2` — exact-set scope must NOT bleed it
                r#"{"ts":"2026-06-22T11:45:00Z","category":"telemetry","source":"detector","session_id":"mission-run-auth-v2-s1","handle":"coder","payload":{"kind":"cycle","severity":"warn","detail":"belongs to auth-v2"}}"#, "\n",
                // non-detector telemetry (source=runtime) — skip
                r#"{"ts":"2026-06-22T12:00:00Z","category":"telemetry","source":"runtime","session_id":"mission-run-auth-s1","handle":"coder","payload":{"kind":"context","detail":"context fill 40%"}}"#, "\n",
                // non-telemetry category, even with source=detector — skip
                r#"{"ts":"2026-06-22T12:05:00Z","category":"work","source":"detector","session_id":"mission-run-auth-s1","handle":"coder","payload":{"kind":"cycle","severity":"warn","detail":"wrong category"}}"#, "\n",
            ),
        )
        .unwrap();
        let prev = std::env::var("DARKMUX_FLOWS_DIR").ok();
        // SAFETY: serialized via #[serial]; restored below.
        unsafe { std::env::set_var("DARKMUX_FLOWS_DIR", tmp.path()) };

        let auth_ids: std::collections::HashSet<String> =
            ["mission-run-auth-s1", "mission-run-auth-s2"]
                .iter()
                .map(|s| s.to_string())
                .collect();
        // Empty intent + a root with no matching files: no file-in-play boost
        // and (no code_hash on these records) no staleness reorder, so this
        // exercises the severity-then-recency fallthrough unchanged.
        let no_intent = std::collections::HashSet::new();
        let cautions = mission_cautions(&auth_ids, &no_intent, tmp.path());
        let unknown = mission_cautions(&std::collections::HashSet::new(), &no_intent, tmp.path());

        unsafe {
            match prev {
                Some(v) => std::env::set_var("DARKMUX_FLOWS_DIR", v),
                None => std::env::remove_var("DARKMUX_FLOWS_DIR"),
            }
        }

        assert_eq!(cautions.len(), 2, "two unique in-mission cautions: {cautions:?}");
        assert!(cautions[0].contains("[cycle]"), "warn outranks info: {cautions:?}");
        assert!(
            cautions[0].contains("(in `src/x.rs`)"),
            "the firing's file is named as the 'where': {cautions:?}"
        );
        assert!(cautions[1].contains("[intra-turn-stall]"), "info ranks last: {cautions:?}");
        assert!(
            !cautions.iter().any(|c| c.contains("auth-v2")),
            "sibling mission auth-v2 must not bleed: {cautions:?}"
        );
        assert!(
            !cautions.iter().any(|c| c.contains("context fill")),
            "non-detector telemetry excluded: {cautions:?}"
        );
        assert!(
            !cautions.iter().any(|c| c.contains("wrong category")),
            "non-telemetry category excluded: {cautions:?}"
        );
        assert!(unknown.is_empty(), "an empty session-id set reads as none");
    }

    // ─── (#1002) intent extraction + file-in-play / staleness ranking ────

    #[test]
    fn intent_files_extracts_path_like_tokens() {
        let got = intent_files(
            "Refactor the parser in `crates/darkmux-crew/src/lessons.rs` and bump Cargo.toml; \
             e.g. tidy up. Touch ./src/main.rs too.",
        );
        assert!(got.contains("crates/darkmux-crew/src/lessons.rs"));
        assert!(got.contains("Cargo.toml"));
        assert!(got.contains("src/main.rs"), "normalized ./src/main.rs: {got:?}");
        // Prose with a trailing dot is not a path.
        assert!(!got.iter().any(|f| f == "e.g" || f == "g"));
    }

    #[test]
    fn normalize_path_lexical_folds_equivalent_paths() {
        assert_eq!(normalize_path_lexical("./src/x.rs"), "src/x.rs");
        assert_eq!(normalize_path_lexical("src/../src/x.rs"), "src/x.rs");
        assert_eq!(normalize_path_lexical("src/x.rs/"), "src/x.rs");
    }

    /// (#1002) A caution about a file this dispatch will touch (intent match)
    /// outranks an engagement-level / other-file one, even when the other is
    /// newer. `#[serial]` — mutates the shared DARKMUX_FLOWS_DIR.
    #[test]
    #[serial_test::serial]
    fn mission_cautions_ranks_file_in_play_first() {
        let tmp = tempfile::TempDir::new().unwrap();
        std::fs::write(
            tmp.path().join("2026-06-22.jsonl"),
            concat!(
                // older caution on the file in play (normalized match for `./src/target.rs`)
                r#"{"ts":"2026-06-22T10:00:00Z","category":"telemetry","source":"detector","session_id":"mission-run-m-s1","payload":{"kind":"cycle","severity":"warn","detail":"on the target","area":{"files":["src/target.rs"]}}}"#, "\n",
                // NEWER, same-severity caution on an unrelated file
                r#"{"ts":"2026-06-22T12:00:00Z","category":"telemetry","source":"detector","session_id":"mission-run-m-s1","payload":{"kind":"cycle","severity":"warn","detail":"on something else","area":{"files":["src/other.rs"]}}}"#, "\n",
            ),
        )
        .unwrap();
        let prev = std::env::var("DARKMUX_FLOWS_DIR").ok();
        unsafe { std::env::set_var("DARKMUX_FLOWS_DIR", tmp.path()) };

        let ids: std::collections::HashSet<String> =
            ["mission-run-m-s1"].iter().map(|s| s.to_string()).collect();
        let intent: std::collections::HashSet<String> =
            ["src/target.rs"].iter().map(|s| s.to_string()).collect();
        // workspace_root has no such files → no code_hash on records anyway → no
        // staleness reorder; this isolates the file-in-play boost.
        let cautions = mission_cautions(&ids, &intent, tmp.path());

        unsafe {
            match prev {
                Some(v) => std::env::set_var("DARKMUX_FLOWS_DIR", v),
                None => std::env::remove_var("DARKMUX_FLOWS_DIR"),
            }
        }
        assert_eq!(cautions.len(), 2);
        assert!(
            cautions[0].contains("on the target"),
            "file-in-play caution ranks first despite being older: {cautions:?}"
        );
    }

    /// (#1002 + #1001) A caution whose firing-time `code_hash` no longer matches
    /// the file's current content (stale) is de-prioritized below a fresh one.
    /// `#[serial]` — mutates the shared DARKMUX_FLOWS_DIR.
    #[test]
    #[serial_test::serial]
    fn mission_cautions_ranks_fresh_over_stale() {
        let ws = tempfile::TempDir::new().unwrap();
        // The "fresh" file: its current content hashes to what we'll record.
        std::fs::write(ws.path().join("fresh.rs"), b"fn fresh() {}").unwrap();
        let fresh_hash = blake3::hash(b"fn fresh() {}").to_hex().to_string();
        // The "stale" file exists but its content differs from the recorded hash.
        std::fs::write(ws.path().join("stale.rs"), b"fn changed() {}").unwrap();
        let stale_recorded_hash = blake3::hash(b"fn ORIGINAL() {}").to_hex().to_string();

        let flows = tempfile::TempDir::new().unwrap();
        std::fs::write(
            flows.path().join("2026-06-22.jsonl"),
            format!(
                concat!(
                    // stale caution (recorded hash != current content), NEWER
                    r#"{{"ts":"2026-06-22T12:00:00Z","category":"telemetry","source":"detector","session_id":"mission-run-m-s1","payload":{{"kind":"cycle","severity":"warn","detail":"stale one","area":{{"files":["stale.rs"],"code_hash":"{stale}"}}}}}}"#, "\n",
                    // fresh caution (recorded hash == current content), older
                    r#"{{"ts":"2026-06-22T10:00:00Z","category":"telemetry","source":"detector","session_id":"mission-run-m-s1","payload":{{"kind":"cycle","severity":"warn","detail":"fresh one","area":{{"files":["fresh.rs"],"code_hash":"{fresh}"}}}}}}"#, "\n",
                ),
                stale = stale_recorded_hash,
                fresh = fresh_hash,
            ),
        )
        .unwrap();
        let prev = std::env::var("DARKMUX_FLOWS_DIR").ok();
        unsafe { std::env::set_var("DARKMUX_FLOWS_DIR", flows.path()) };

        let ids: std::collections::HashSet<String> =
            ["mission-run-m-s1"].iter().map(|s| s.to_string()).collect();
        // No intent match for either (neither file is in play) so the only
        // discriminator is freshness.
        let cautions = mission_cautions(&ids, &std::collections::HashSet::new(), ws.path());

        unsafe {
            match prev {
                Some(v) => std::env::set_var("DARKMUX_FLOWS_DIR", v),
                None => std::env::remove_var("DARKMUX_FLOWS_DIR"),
            }
        }
        assert_eq!(cautions.len(), 2);
        assert!(
            cautions[0].contains("fresh one"),
            "fresh caution outranks the newer-but-stale one: {cautions:?}"
        );
    }

    // ─── (#1011) proportional injected-context budget + distribution ─────

    #[test]
    fn budget_chars_scales_with_window_and_floors_at_min() {
        // fraction × window × 4 chars/token.
        assert_eq!(budget_chars_for(Some(100_000), 0.15), (100_000f64 * 0.15) as usize * 4);
        // A bigger window → a bigger budget (auto-scaling from one fraction).
        assert!(budget_chars_for(Some(100_000), 0.15) > budget_chars_for(Some(8_000), 0.15));
        // A pathologically small window still floors at the minimum.
        assert_eq!(budget_chars_for(Some(10), 0.15), 2_000);
        // Unresolved window → the default fallback (non-zero, above the floor).
        assert!(budget_chars_for(None, 0.15) > 2_000);
    }

    fn bullets(prefix: &str, n: usize, len: usize) -> Vec<String> {
        // Each bullet is exactly `len` chars (prefix + padding) for predictable
        // budget math.
        (0..n)
            .map(|i| {
                let head = format!("{prefix}{i}:");
                format!("{head}{}", "x".repeat(len.saturating_sub(head.len())))
            })
            .collect()
    }

    #[test]
    fn allocate_gives_each_nonempty_category_its_floor() {
        // Generous budget: everything fits, nothing is dropped.
        let corr = bullets("c", 3, 50);
        let caut = bullets("a", 3, 50);
        let less = bullets("l", 3, 50);
        let (rc, rca, rl) =
            allocate_injected_context(corr.clone(), caut.clone(), less.clone(), 100_000);
        assert_eq!((rc.len(), rca.len(), rl.len()), (3, 3, 3), "all fit under a big budget");
    }

    #[test]
    fn allocate_caps_cautions_so_they_cannot_flood() {
        // Only cautions have content, and a LOT of it. The cautions cap (35% of
        // budget) bounds them even though the whole pool is otherwise free.
        let caut = bullets("a", 100, 100); // 100 bullets × 100 chars ≈ 10 100 chars demand
        let budget = 10_000;
        let (_, rca, _) = allocate_injected_context(Vec::new(), caut, Vec::new(), budget);
        let used: usize = rca.iter().map(|s| s.len() + 1).sum::<usize>().saturating_sub(1);
        // ≤ 35% of budget (+ one bullet's slack for the boundary item).
        assert!(used <= (budget * 35 / 100) + 101, "cautions capped near 35%: used={used}");
        assert!(!rca.is_empty(), "but still get their floor");
    }

    #[test]
    fn allocate_reallocates_empty_floors_and_prioritizes_corrections() {
        // No lessons, no cautions → their floors return to the pool, so the
        // high-authority corrections can use the whole budget.
        let corr = bullets("c", 50, 100); // 50 × 100 ≈ 5 049 chars demand
        let budget = 6_000;
        let (rc, rca, rl) =
            allocate_injected_context(corr, Vec::new(), Vec::new(), budget);
        assert!(rca.is_empty() && rl.is_empty());
        assert!(rc.len() >= 40, "corrections claim the freed pool: got {}", rc.len());
    }

    #[test]
    fn allocate_floor_protects_corrections_from_a_caution_flood() {
        // Cautions demand far exceeds budget, but corrections' floor guarantees
        // it lands regardless (the doom-loop's named failure: a correction
        // evaporating under a flood).
        let corr = bullets("c", 2, 100);
        let caut = bullets("a", 200, 100);
        let (rc, _, _) = allocate_injected_context(corr, caut, Vec::new(), 8_000);
        assert_eq!(rc.len(), 2, "both corrections survive the caution flood");
    }

    /// (#849 half 1) The brief-injection reader: collects adjudication notes
    /// for a mission's EXACT dispatch session ids, dedups, and excludes other
    /// sources + sibling missions. The load-bearing case is the sibling-mission
    /// regression QA caught: `auth-v2`'s notes must NOT bleed into `auth` (a
    /// prefix match would, since `mission-run-auth-v2-s1` starts with
    /// `mission-run-auth-`). `#[serial]` — mutates the shared DARKMUX_FLOWS_DIR.
    #[test]
    #[serial_test::serial]
    fn mission_adjudication_notes_reads_family_and_filters() {
        let tmp = tempfile::TempDir::new().unwrap();
        std::fs::write(
            tmp.path().join("2026-06-21.jsonl"),
            concat!(
                // mission `auth`, sprint s1 — an adjudication correction
                r#"{"ts":"2026-06-21T10:00:00Z","action":"note","source":"adjudication","session_id":"mission-run-auth-s1","handle":"Do not rename the field."}"#, "\n",
                // `auth`, a LATER sprint — same family, must be carried forward
                r#"{"ts":"2026-06-21T11:00:00Z","action":"note","source":"adjudication","session_id":"mission-run-auth-s2","handle":"Use cargo test -p foo."}"#, "\n",
                // exact duplicate of the first — must not repeat
                r#"{"ts":"2026-06-21T11:30:00Z","action":"note","source":"adjudication","session_id":"mission-run-auth-s1","handle":"Do not rename the field."}"#, "\n",
                // SIBLING mission `auth-v2` (id is a hyphen-extension of `auth`)
                // — a prefix match would bleed this in; the exact-set match must
                // NOT (the #849 QA regression).
                r#"{"ts":"2026-06-21T11:45:00Z","action":"note","source":"adjudication","session_id":"mission-run-auth-v2-s1","handle":"Belongs to auth-v2 ONLY."}"#, "\n",
                // an ORCHESTRATOR (dashboard) note for `auth` — wrong source, skip
                r#"{"ts":"2026-06-21T12:00:00Z","action":"note","source":"orchestrator","session_id":"mission-run-auth-s1","handle":"crew shipped it!"}"#, "\n",
            ),
        )
        .unwrap();
        let prev = std::env::var("DARKMUX_FLOWS_DIR").ok();
        // SAFETY: serialized via #[serial]; restored below.
        unsafe { std::env::set_var("DARKMUX_FLOWS_DIR", tmp.path()) };

        // `auth`'s EXACT dispatch session ids — as run() builds them from the
        // mission's sprints. Note `auth-v2`'s session id is deliberately absent.
        let auth_ids: std::collections::HashSet<String> =
            ["mission-run-auth-s1", "mission-run-auth-s2"]
                .iter()
                .map(|s| s.to_string())
                .collect();
        let notes = mission_adjudication_notes(&auth_ids);
        let unknown = mission_adjudication_notes(&std::collections::HashSet::new());

        unsafe {
            match prev {
                Some(v) => std::env::set_var("DARKMUX_FLOWS_DIR", v),
                None => std::env::remove_var("DARKMUX_FLOWS_DIR"),
            }
        }
        assert_eq!(notes.len(), 2, "two unique adjudication notes across the auth family: {notes:?}");
        assert!(notes.contains(&"Do not rename the field.".to_string()), "{notes:?}");
        assert!(notes.contains(&"Use cargo test -p foo.".to_string()), "{notes:?}");
        assert!(
            !notes.iter().any(|n| n.contains("auth-v2")),
            "sibling mission auth-v2 must NOT bleed into auth (the #849 prefix-bleed regression): {notes:?}"
        );
        assert!(!notes.iter().any(|n| n.contains("crew shipped")), "orchestrator note excluded: {notes:?}");
        assert!(unknown.is_empty(), "an empty session-id set reads as none");
    }

    /// (#1000) The debrief gather assembles a mission's review material from
    /// on-disk mission/sprint state + the flow stream: the mission identity, its
    /// sprints + how each ended, the detector cautions, and the reviewer's
    /// corrections — all scoped to THIS mission's exact dispatch sessions.
    /// `#[serial]` — mutates DARKMUX_HOME (mission/sprint loaders) + the
    /// DARKMUX_FLOWS_DIR (the collectors read it live per-access).
    #[test]
    #[serial_test::serial]
    fn gather_debrief_assembles_mission_material() {
        let home = tempfile::TempDir::new().unwrap();
        let flows = tempfile::TempDir::new().unwrap();
        let mid = "m-debrief";
        let sprints_dir = home.path().join("missions").join(mid).join("sprints");
        std::fs::create_dir_all(&sprints_dir).unwrap();
        std::fs::write(
            home.path().join("missions").join(mid).join("mission.json"),
            format!(
                r#"{{"id":"{mid}","description":"close the doom loop","status":"closed","sprint_ids":["s1","s2"],"created_ts":1700000000}}"#
            ),
        )
        .unwrap();
        std::fs::write(
            sprints_dir.join("s1.json"),
            format!(
                r#"{{"id":"s1","mission_id":"{mid}","description":"capture slice","status":"complete","depends_on":[],"created_ts":1700000200}}"#
            ),
        )
        .unwrap();
        std::fs::write(
            sprints_dir.join("s2.json"),
            format!(
                r#"{{"id":"s2","mission_id":"{mid}","description":"index slice","status":"abandoned","depends_on":[],"created_ts":1700000300}}"#
            ),
        )
        .unwrap();
        // A detector caution + an adjudication correction, scoped to s1's session.
        std::fs::write(
            flows.path().join("2026-06-22.jsonl"),
            concat!(
                r#"{"ts":"2026-06-22T10:00:00Z","category":"telemetry","source":"detector","session_id":"mission-run-m-debrief-s1","handle":"coder","payload":{"kind":"cycle","severity":"warn","detail":"edit called 3x","area":{"files":["src/index.rs"]}}}"#, "\n",
                r#"{"ts":"2026-06-22T10:30:00Z","action":"note","source":"adjudication","session_id":"mission-run-m-debrief-s1","handle":"overrode SIGNOFF — verify never ran"}"#, "\n",
                // SIBLING mission session must NOT bleed in.
                r#"{"ts":"2026-06-22T10:45:00Z","category":"telemetry","source":"detector","session_id":"mission-run-m-debrief-v2-s1","handle":"coder","payload":{"kind":"cycle","severity":"warn","detail":"belongs to a sibling"}}"#, "\n",
            ),
        )
        .unwrap();

        let prev_home = std::env::var("DARKMUX_HOME").ok();
        let prev_flows = std::env::var("DARKMUX_FLOWS_DIR").ok();
        // SAFETY: serialized via #[serial]; restored below.
        unsafe {
            std::env::set_var("DARKMUX_HOME", home.path());
            std::env::set_var("DARKMUX_FLOWS_DIR", flows.path());
        }

        let report = gather_debrief(mid);
        let missing = gather_debrief("does-not-exist");

        unsafe {
            match prev_home {
                Some(v) => std::env::set_var("DARKMUX_HOME", v),
                None => std::env::remove_var("DARKMUX_HOME"),
            }
            match prev_flows {
                Some(v) => std::env::set_var("DARKMUX_FLOWS_DIR", v),
                None => std::env::remove_var("DARKMUX_FLOWS_DIR"),
            }
        }

        let report = report.expect("mission found");
        assert_eq!(report.mission_id, mid);
        assert_eq!(report.mission_status, "closed");
        assert_eq!(report.sprints.len(), 2, "both sprints surfaced: {:?}", report.sprints);
        assert!(
            report.sprints.iter().any(|(id, _, st)| id == "s1" && *st == "complete"),
            "{:?}",
            report.sprints
        );
        assert!(
            report.sprints.iter().any(|(id, _, st)| id == "s2" && *st == "abandoned"),
            "{:?}",
            report.sprints
        );
        assert_eq!(report.cautions.len(), 1, "one in-mission caution (sibling excluded): {:?}", report.cautions);
        assert!(report.cautions[0].contains("src/index.rs"), "{:?}", report.cautions);
        assert_eq!(report.corrections, vec!["overrode SIGNOFF — verify never ran".to_string()]);
        assert!(missing.is_err(), "an unknown mission errors");
    }

    /// (#1000) Closing a mission nudges the debrief — and that nudge emits a
    /// `Stage::Debrief` flow record (the variant's first real emission; #999
    /// added it unemitted). `#[serial]` — mutates DARKMUX_FLOWS_DIR.
    #[test]
    #[serial_test::serial]
    fn nudge_mission_debrief_emits_debrief_stage_record() {
        let flows = tempfile::TempDir::new().unwrap();
        let prev = std::env::var("DARKMUX_FLOWS_DIR").ok();
        // SAFETY: serialized via #[serial]; restored below.
        unsafe { std::env::set_var("DARKMUX_FLOWS_DIR", flows.path()) };

        nudge_mission_debrief("m-x");

        unsafe {
            match prev {
                Some(v) => std::env::set_var("DARKMUX_FLOWS_DIR", v),
                None => std::env::remove_var("DARKMUX_FLOWS_DIR"),
            }
        }

        let mut found = false;
        for entry in std::fs::read_dir(flows.path()).unwrap() {
            let p = entry.unwrap().path();
            if p.extension().and_then(|x| x.to_str()) != Some("jsonl") {
                continue;
            }
            for line in std::fs::read_to_string(&p).unwrap().lines() {
                let r: serde_json::Value = serde_json::from_str(line).unwrap();
                if r.get("stage").and_then(|v| v.as_str()) == Some("debrief")
                    && r.get("action").and_then(|v| v.as_str()) == Some("mission.debrief.prompt")
                    && r.get("mission_id").and_then(|v| v.as_str()) == Some("m-x")
                {
                    found = true;
                }
            }
        }
        assert!(found, "the close nudge must emit a stage=debrief mission.debrief.prompt record");
    }

    /// (#817) The note-trail scan finds a session-scoped orchestrator note in
    /// the newest day files, and reads "no note" for other sessions, other
    /// sources, and a missing dir. `#[serial_test::serial]` — mutates the
    /// shared DARKMUX_FLOWS_DIR env (config_access reads it live per-access).
    #[test]
    #[serial_test::serial]
    fn session_note_scan_matches_session_and_source() {
        let tmp = tempfile::TempDir::new().unwrap();
        std::fs::write(
            tmp.path().join("2026-06-12.jsonl"),
            concat!(
                r#"{"ts":"2026-06-12T10:00:00Z","action":"note","source":"orchestrator","session_id":"mission-run-m1-s1","handle":"adjudicated"}"#, "\n",
                r#"{"ts":"2026-06-12T10:01:00Z","action":"note","source":"operator","session_id":"mission-run-m1-s2","handle":"not orchestrator"}"#, "\n",
                r#"{"ts":"2026-06-12T10:02:00Z","action":"note","source":"adjudication","session_id":"mission-run-m1-s3","handle":"audit-trail channel"}"#, "\n",
            ),
        )
        .unwrap();
        let prev = std::env::var("DARKMUX_FLOWS_DIR").ok();
        // SAFETY: serialized via #[serial]; restored below.
        unsafe { std::env::set_var("DARKMUX_FLOWS_DIR", tmp.path()) };

        let hit = session_has_orchestrator_note("mission-run-m1-s1");
        let wrong_session = session_has_orchestrator_note("mission-run-m1-sX");
        let wrong_source = session_has_orchestrator_note("mission-run-m1-s2");
        let adjudication_tag = session_has_orchestrator_note("mission-run-m1-s3");

        unsafe {
            match prev {
                Some(v) => std::env::set_var("DARKMUX_FLOWS_DIR", v),
                None => std::env::remove_var("DARKMUX_FLOWS_DIR"),
            }
        }
        assert!(hit, "session-scoped orchestrator note must be found");
        assert!(!wrong_session, "other sessions' notes must not match");
        assert!(!wrong_source, "non-adjudication/orchestrator notes must not match");
        assert!(adjudication_tag, "the adjudication audit-trail tag must satisfy the scan");
    }

    fn sprint(id: &str, mission: &str, deps: &[&str], status: SprintStatus) -> Sprint {
        Sprint {
            id: id.to_string(),
            mission_id: mission.to_string(),
            description: format!("desc {id}"),
            status,
            depends_on: deps.iter().map(|s| s.to_string()).collect(),
            created_ts: 0,
            started_ts: None,
            completed_ts: None,
            abandoned_ts: None,
        }
    }

    #[test]
    fn select_sprint_explicit_must_belong_to_mission() {
        let sprints = vec![sprint("s1", "m1", &[], SprintStatus::Planned)];
        let err = select_sprint(&sprints, "m2", Some("s1")).unwrap_err();
        assert!(err.to_string().contains("belongs to mission"), "{err}");
    }

    #[test]
    fn select_sprint_explicit_rejects_complete() {
        let sprints = vec![sprint("s1", "m1", &[], SprintStatus::Complete)];
        let err = select_sprint(&sprints, "m1", Some("s1")).unwrap_err();
        assert!(err.to_string().contains("already Complete"), "{err}");
    }

    #[test]
    fn select_sprint_auto_picks_single_ready() {
        let sprints = vec![
            sprint("s1", "m1", &[], SprintStatus::Planned),
            sprint("s2", "m1", &["s1"], SprintStatus::Planned), // has unmet dep
            sprint("s3", "m2", &[], SprintStatus::Planned),     // other mission
        ];
        let chosen = select_sprint(&sprints, "m1", None).unwrap();
        assert_eq!(chosen.id, "s1");
    }

    #[test]
    fn select_sprint_auto_bails_on_zero_ready() {
        let sprints = vec![sprint("s1", "m1", &[], SprintStatus::Running)];
        let err = select_sprint(&sprints, "m1", None).unwrap_err();
        assert!(err.to_string().contains("no ready sprint"), "{err}");
    }

    #[test]
    fn select_sprint_auto_bails_ambiguous() {
        let sprints = vec![
            sprint("s1", "m1", &[], SprintStatus::Planned),
            sprint("s2", "m1", &[], SprintStatus::Planned),
        ];
        let err = select_sprint(&sprints, "m1", None).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("2 ready sprints"), "{msg}");
        assert!(msg.contains("--sprint"), "{msg}");
    }

    #[test]
    fn branch_name_is_namespaced() {
        assert_eq!(branch_name("s1"), "darkmux/s1");
    }

    #[test]
    fn worktree_path_is_deterministic_under_repo_name() {
        let p = worktree_path(Path::new("/home/k/proj/darkmux-public"), "s1");
        assert!(p.ends_with("darkmux-public/s1"), "{}", p.display());
        // Recomputable: same inputs → same path.
        assert_eq!(p, worktree_path(Path::new("/home/k/proj/darkmux-public"), "s1"));
    }

    #[test]
    fn parse_main_worktree_picks_first_entry() {
        // The first `worktree` line is the main working tree; a linked
        // worktree follows. #846: ship from inside the linked one must still
        // resolve the repo name from the FIRST entry, not the current tree.
        let porcelain = "worktree /home/k/proj/darkmux-public\n\
                         HEAD 1111111111111111111111111111111111111111\n\
                         branch refs/heads/main\n\
                         \n\
                         worktree /home/k/.darkmux/worktrees/darkmux-public/s2-foo\n\
                         HEAD 2222222222222222222222222222222222222222\n\
                         branch refs/heads/darkmux/s2-foo\n";
        assert_eq!(
            parse_main_worktree(porcelain),
            Some(PathBuf::from("/home/k/proj/darkmux-public"))
        );
        // The repo-name component derived from it is stable regardless of which
        // tree `mission ship` was invoked from.
        let root = parse_main_worktree(porcelain).unwrap();
        assert!(worktree_path(&root, "s2-foo").ends_with("darkmux-public/s2-foo"));
    }

    #[test]
    fn parse_main_worktree_handles_empty_and_blank() {
        assert_eq!(parse_main_worktree(""), None);
        assert_eq!(parse_main_worktree("worktree \nHEAD abc\n"), None);
        assert_eq!(parse_main_worktree("HEAD abc\nbranch refs/heads/main\n"), None);
    }

    #[test]
    fn parse_main_worktree_unquoted_path_roundtrips_verbatim() {
        // No special chars → git emits the path unquoted; trailing space kept.
        assert_eq!(
            parse_main_worktree("worktree /home/me/repo \nHEAD abc\n"),
            Some(PathBuf::from("/home/me/repo "))
        );
    }

    #[test]
    fn parse_main_worktree_decodes_c_quoted_path() {
        // (#907) git C-quotes paths with special chars: a space-containing or
        // non-ASCII path is wrapped in quotes with escapes. The leading `"`
        // signals the quoted form.
        assert_eq!(
            parse_main_worktree("worktree \"/home/me/my repo\"\nHEAD abc\n"),
            Some(PathBuf::from("/home/me/my repo"))
        );
        // Escaped tab + backslash + quote.
        assert_eq!(
            parse_main_worktree("worktree \"/tmp/a\\tb\\\\c\\\"d\"\n"),
            Some(PathBuf::from("/tmp/a\tb\\c\"d"))
        );
        // Octal-escaped UTF-8 (é = 0xC3 0xA9 = \303\251).
        assert_eq!(
            parse_main_worktree("worktree \"/tmp/caf\\303\\251\"\n"),
            Some(PathBuf::from("/tmp/café"))
        );
    }

    #[test]
    fn git_lists_main_worktree_first_from_inside_a_linked_worktree() {
        // Locks the load-bearing #846 contract against REAL git: invoked from
        // INSIDE a linked worktree, `git worktree list --porcelain` still lists
        // the MAIN working tree first — so repo_root() (= this command +
        // parse_main_worktree) resolves the repo, not the sprint dir. A future
        // git change or an output-ordering refactor that broke this is caught
        // here. No process-cwd mutation: git is invoked with `current_dir`.
        let tmp = tempfile::TempDir::new().unwrap();
        let main_repo = tmp.path().join("mainrepo");
        let linked = tmp.path().join("linked-sprint");
        std::fs::create_dir_all(&main_repo).unwrap();

        let git = |dir: &Path, args: &[&str]| {
            let o = Command::new("git").current_dir(dir).args(args).output().unwrap();
            assert!(
                o.status.success(),
                "git {args:?} failed: {}",
                String::from_utf8_lossy(&o.stderr)
            );
        };
        git(&main_repo, &["init", "-q"]);
        git(&main_repo, &["config", "user.email", "t@example.com"]);
        git(&main_repo, &["config", "user.name", "t"]);
        git(&main_repo, &["commit", "-q", "--allow-empty", "-m", "init"]);
        git(&main_repo, &["worktree", "add", "-q", linked.to_str().unwrap(), "-b", "sprint-x"]);

        // Invoked FROM the linked worktree — the exact #846 scenario.
        let out = Command::new("git")
            .current_dir(&linked)
            .args(["worktree", "list", "--porcelain"])
            .output()
            .unwrap();
        assert!(out.status.success(), "git worktree list failed");
        let parsed = parse_main_worktree(&String::from_utf8_lossy(&out.stdout))
            .expect("parse a main worktree from porcelain");
        assert_eq!(
            parsed.canonicalize().unwrap(),
            main_repo.canonicalize().unwrap(),
            "expected the MAIN tree, got {}",
            parsed.display()
        );
    }

    fn mission(id: &str, desc: &str) -> crew::types::Mission {
        crew::types::Mission {
            id: id.to_string(),
            description: desc.to_string(),
            status: crew::types::MissionStatus::Active,
            sprint_ids: vec![],
            created_ts: 0,
            started_ts: None,
            closed_ts: None,
            paused_ts: None,
            source_input: None,
            ticket: None,
        }
    }

    #[test]
    fn commit_subject_takes_first_line() {
        let s = sprint("s1", "m1", &[], SprintStatus::Running);
        // sprint() sets description = "desc s1"
        assert_eq!(commit_subject(&s), "desc s1");
    }

    #[test]
    fn commit_subject_truncates_long_and_takes_only_first_line() {
        let mut s = sprint("s1", "m1", &[], SprintStatus::Running);
        s.description = format!("{}\nsecond line ignored", "x".repeat(100));
        let subj = commit_subject(&s);
        assert!(subj.chars().count() <= 72, "len {}", subj.chars().count());
        assert!(subj.ends_with("..."), "{subj}");
        assert!(!subj.contains("second line"), "only first line: {subj}");
    }

    #[test]
    fn commit_subject_falls_back_on_empty_description() {
        let mut s = sprint("s1", "m1", &[], SprintStatus::Running);
        s.description = String::new();
        assert_eq!(commit_subject(&s), "darkmux sprint s1");
    }

    #[test]
    fn pr_body_names_mission_and_sprint_no_currency() {
        let m = mission("m1", "ship the thing");
        let s = sprint("s1", "m1", &[], SprintStatus::Running);
        let body = pr_body(&m, &s);
        assert!(body.contains("`m1`"), "{body}");
        assert!(body.contains("`s1`"), "{body}");
        assert!(body.contains("mission ship"), "{body}");
        // Tokens-only doctrine: no currency leaks into shipped PR copy.
        assert!(!body.contains('$'), "no currency in PR body: {body}");
    }

    // (#799 part 2) parse_failed_verifiers — the verifier-fabrication backstop's
    // consumer-side parse. The governing discipline is FALSE-ALARM avoidance:
    // anything unparseable or absent must read as "nothing failed", never as a
    // failure — a soft signal that cries wolf is worse than one that stays quiet.
    fn envelope_with(failed: &str) -> String {
        format!(
            r#"{{"result":"stop","final_assistant":"done","metrics":{{}},"trajectory_path":"/x","failed_tool_invocations":{failed}}}"#
        )
    }

    #[test]
    fn parse_failed_verifiers_extracts_entries() {
        let env = envelope_with(
            r#"[{"command":"cargo test","reason":"command not found (exit 127) — the verifier never ran"}]"#,
        );
        let got = parse_failed_verifiers(&env);
        assert_eq!(got.len(), 1, "{got:?}");
        assert_eq!(got[0].command, "cargo test");
        assert!(got[0].reason.contains("exit 127"), "{:?}", got[0].reason);
    }

    #[test]
    fn parse_failed_verifiers_empty_array_is_empty() {
        // An honest run stamps an empty array — the no-op case.
        assert!(parse_failed_verifiers(&envelope_with("[]")).is_empty());
    }

    #[test]
    fn parse_failed_verifiers_missing_field_is_empty() {
        // A pre-#799 runtime (or a non-success envelope) omits the field
        // entirely — must NOT be read as a failure.
        let env = r#"{"result":"stop","final_assistant":"done","metrics":{}}"#;
        assert!(parse_failed_verifiers(env).is_empty());
    }

    #[test]
    fn parse_failed_verifiers_malformed_json_is_empty() {
        // Garbage on stdout must fail OPEN to "nothing failed" — never a false
        // alarm that would hold a clean run's merge.
        assert!(parse_failed_verifiers("not json at all").is_empty());
        assert!(parse_failed_verifiers("").is_empty());
    }

    #[test]
    fn parse_failed_verifiers_last_line_fallback() {
        // Defense: if an unexpected leading line precedes the envelope, the
        // last-non-empty-line fallback still recovers the stamp.
        let env = envelope_with(r#"[{"command":"pytest","reason":"toolchain failed to load"}]"#);
        let stdout = format!("some stray log line\n{env}\n");
        let got = parse_failed_verifiers(&stdout);
        assert_eq!(got.len(), 1, "{got:?}");
        assert_eq!(got[0].command, "pytest");
    }

    /// (#799 part 2) The ship-side reader round-trip — the run→ship handoff. The
    /// load-bearing case is RESUMED-SPRINT latest-wins: a clean re-run's empty
    /// `mission.run.verification` record must OVERWRITE a prior dirty run's for
    /// the same session, so the documented fix-and-retry actually clears the
    /// hold. Also: a dirty-only session stays held, and other sessions don't
    /// bleed in. `#[serial]` — mutates the shared DARKMUX_FLOWS_DIR (read live
    /// per-access by config_access).
    #[test]
    #[serial_test::serial]
    fn session_failed_verifiers_latest_run_wins() {
        let tmp = tempfile::TempDir::new().unwrap();
        std::fs::write(
            tmp.path().join("2026-06-21.jsonl"),
            concat!(
                // session A: dirty run #1, then a clean re-run #2 (later line wins).
                r#"{"ts":"2026-06-21T10:00:00Z","action":"mission.run.verification","session_id":"mission-run-mA-s1","payload":{"failed":[{"command":"cargo test","reason":"command not found (exit 127) — the verifier never ran"}],"count":1}}"#, "\n",
                r#"{"ts":"2026-06-21T10:30:00Z","action":"mission.run.verification","session_id":"mission-run-mA-s1","payload":{"failed":[],"count":0}}"#, "\n",
                // session B: a single dirty run — stays held.
                r#"{"ts":"2026-06-21T11:00:00Z","action":"mission.run.verification","session_id":"mission-run-mB-s1","payload":{"failed":[{"command":"pytest","reason":"toolchain failed to load"}],"count":1}}"#, "\n",
            ),
        )
        .unwrap();
        let prev = std::env::var("DARKMUX_FLOWS_DIR").ok();
        // SAFETY: serialized via #[serial]; restored below.
        unsafe { std::env::set_var("DARKMUX_FLOWS_DIR", tmp.path()) };

        let session_a = session_failed_verifiers("mission-run-mA-s1");
        let session_b = session_failed_verifiers("mission-run-mB-s1");
        let unknown = session_failed_verifiers("mission-run-mZ-s9");

        unsafe {
            match prev {
                Some(v) => std::env::set_var("DARKMUX_FLOWS_DIR", v),
                None => std::env::remove_var("DARKMUX_FLOWS_DIR"),
            }
        }
        assert!(
            session_a.is_empty(),
            "a clean re-run must clear a prior dirty record (latest-wins): {session_a:?}"
        );
        assert_eq!(session_b.len(), 1, "a dirty-only session stays held: {session_b:?}");
        assert_eq!(session_b[0].command, "pytest");
        assert!(unknown.is_empty(), "an unknown session reads as none");
    }
}
