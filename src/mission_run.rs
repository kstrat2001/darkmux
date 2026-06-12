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

/// The git repository root of the current directory (`git rev-parse
/// --show-toplevel`). `mission run` operates on the repo the operator
/// invoked it from — the worktree branches off this repo's `--base`.
fn repo_root() -> Result<PathBuf> {
    let out = Command::new("git")
        .args(["rev-parse", "--show-toplevel"])
        .output()
        .context("running `git rev-parse --show-toplevel`")?;
    if !out.status.success() {
        bail!(
            "`darkmux mission run` must be invoked from inside a git repository \
             (git rev-parse --show-toplevel failed). cd into the engagement's repo first."
        );
    }
    let root = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if root.is_empty() {
        bail!("git reported an empty repository root");
    }
    Ok(PathBuf::from(root))
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
    let branch = branch_name(&sprint.id);

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
    let opts = crew::dispatch::DispatchOpts {
        role_id: role.to_string(),
        message: sprint.description.clone(),
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
        image: image.map(String::from),
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
                "✗ coder dispatch exited {} — see stderr above. Worktree left at {} \
                 for inspection (or `darkmux mission abort {mission_id} --sprint {}`).",
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
    // (#807) Cue the frontier orchestrator at the decision moment — tool
    // output is the one hint channel every harness reads. The note follows
    // the MERGE (the arc's conclusion), so the gate print sequences it.
    println!(
        "{}",
        style::dim(
            "  after the merge, conclude on the dashboard:  darkmux flow note \
             --text \"<what ran · where the tokens stayed · your call>\" --source orchestrator",
        )
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
    if !missions.iter().any(|m| m.id == mission_id) {
        bail!("mission `{mission_id}` not found");
    }
    let sprints = load_sprints()?;
    // Explicit `--sprint` resolves by id (any status — a Running sprint, the
    // common abort case after a `run`, resolves); auto-path requires a ready
    // Planned sprint. So to abort a Running sprint, pass `--sprint`.
    let sprint = resolve_sprint(&sprints, mission_id, sprint_id)?;

    let root = repo_root()?;
    let wt_path = worktree_path(&root, &sprint.id);
    let branch = branch_name(&sprint.id);

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

/// Commit subject for a shipped sprint: the sprint description's first line,
/// trimmed to a conventional ~72-char subject.
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

/// PR title for a shipped sprint.
fn pr_title(sprint: &crew::types::Sprint) -> String {
    commit_subject(sprint)
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

/// `gh pr view <branch> --json url -q .url` — returns the existing PR URL for
/// the branch, or `None` if there's no open PR (or `gh` reports none).
fn existing_pr_url(dir: &Path, branch: &str) -> Option<String> {
    let out = Command::new("gh")
        .current_dir(dir)
        .args(["pr", "view", branch, "--json", "url", "-q", ".url"])
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
/// marked Complete (inconsistent state — needs manual reconcile).
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
    let branch = branch_name(&sprint.id);
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
        let subject = commit_subject(&sprint);
        let msg = format!(
            "{subject}\n\nAuthored via `darkmux mission run` (local-AI coder, sprint {}).",
            sprint.id
        );
        let out = git_in(&wt_path, &["commit", "-m", &msg])?;
        if !out.status.success() {
            bail!("git commit failed: {}", String::from_utf8_lossy(&out.stderr).trim());
        }
        println!("{}", style::success(&format!("✓ committed: {subject}")));
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
            let title = pr_title(&sprint);
            let body = pr_body(&mission, &sprint);
            let out = Command::new("gh")
                .current_dir(&wt_path)
                .args([
                    "pr", "create", "--base", base, "--head", &branch, "--title", &title,
                    "--body", &body,
                ])
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
            bail!(
                "`gh pr merge` failed: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            );
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
        // (#807) The arc just concluded — cue the orchestrator note HERE,
        // where the conclusion is freshest. Tool output is the hint channel
        // every frontier harness reads at the moment of action.
        println!(
            "{}",
            style::dim(
                "  next: leave the orchestrator note →  darkmux flow note \
                 --text \"<what ran · where the tokens stayed · your call>\" --source orchestrator",
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
}
