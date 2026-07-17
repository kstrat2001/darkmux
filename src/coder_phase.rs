//! Coder-phase execution — the launch-owned worktree → coder → QA loop.
//!
//! (#1426, ship-4) The `mission run` verb retired: the coder pipeline now
//! runs EXCLUSIVELY through `mission launch coder-phase`, which materializes
//! the same three-Task/three-Step graph as data (`mission-configs/
//! coder-phase.json`) and executes it through the generic scheduler. This
//! module is what the launch path OWNS for that pipeline — per #1352's
//! physical StepKind tiering, the bespoke Tier 3 kinds live with the mission
//! module that drives them:
//!
//!   * [`MissionWorktreeStepKind`] — create an isolated git worktree,
//!   * [`MissionCoderStepKind`] — dispatch the coder role into it,
//!   * [`MissionVerifyStepKind`] — run local `code-reviewer` QA on the diff,
//!
//! plus the injected-context brief-building the coder step feeds on
//! ([`coder_brief`] + the cautions/lessons/corrections allocation), and the
//! two operator-facing lifecycle verbs that finish or back out a gate-held
//! run: [`ship`] (commit → PR → CI → merge → teardown) and [`abort`]
//! (worktree + branch teardown, phase Abandoned). [`debrief`] reports on how
//! a mission's phases ended.
//!
//! Why the pipeline stops at a gate: adjudicating the QA findings and
//! deciding to merge are judgment steps that belong to the frontier
//! orchestrator + operator, never to a CLI verb (operator sovereignty, #44;
//! never-auto-merge). `mission launch coder-phase` tees everything up so
//! sign-off is one follow-on step — `darkmux mission ship <id> --phase
//! <phase-id>` does the commit → PR → CI → merge → teardown after the
//! operator/frontier signs off (#782).

use anyhow::{bail, Context, Result};
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::crew;
use crate::fleet;
use crate::flow;
use darkmux_types::style;

/// Emit a mission-run lifecycle flow record for the mission-level events
/// OUTSIDE the worktree→coder→verify Task/Step graph — `mission abort`'s
/// teardown and `mission ship`'s commit→PR→merge sequence (#1230 Packet 4:
/// the graph's own three steps retired their bespoke `mission.run.*`
/// vocabulary in favor of the scheduler's generic step-lifecycle bookends
/// plus [`emit_step_result`]'s companion payload — see that function's doc).
/// Best-effort (observability, never loop-failing).
fn emit_run_record(
    level: flow::Level,
    action: &str,
    mission_id: &str,
    phase_id: &str,
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
        Some(phase_id),
        Some(payload),
    ));
}

/// (#1230 Packet 4) Emit a `"step result"` flow record — the rich,
/// kind-specific companion to the scheduler's generic step-lifecycle
/// bookends (`"step start"`/`"step complete"`/`"step error"`, emitted for
/// free by `run_step_graph` itself). Retires the bespoke per-purpose
/// `mission.run.start`/`mission.run.error`/`mission.run.verification`/
/// `mission.run.qa-unavailable`/`mission.run.blocked`/`mission.run.gate`
/// vocabulary those three step kinds and `run()`'s own post-graph gate
/// logic used to emit directly — ONE generic action name now, with `kind`
/// (`"mission.worktree"` | `"mission.coder"` | `"mission.verify"`) plus a
/// free-form `payload` object distinguishing WHICH step and WHAT happened,
/// mirroring the review module's own `review.step`/`review.ruling`
/// generic-action-plus-payload convention. Consumers: the viewer's
/// `cycleStage()`, `darkmux-serve`'s `resolve_session` (the `/diff`
/// endpoint), and `session_failed_verifiers` below all filter on `kind`
/// rather than a per-purpose action string now.
// (#1284 Packet 4a) `pub(crate)` — `mission_launch.rs` reuses this exact
// helper for the SAME `"step result"` flow-record shape when it runs the
// coder-phase graph through the config-launched path, rather than
// reinventing a second vocabulary for the same event.
pub(crate) fn emit_step_result(
    level: flow::Level,
    kind: &str,
    step_id: &str,
    mission_id: &str,
    phase_id: &str,
    session_id: &str,
    payload: serde_json::Value,
) {
    let mut full = serde_json::json!({ "step_id": step_id, "kind": kind });
    if let (serde_json::Value::Object(extra), serde_json::Value::Object(base)) =
        (payload, &mut full)
    {
        base.extend(extra);
    }
    let _ = flow::record(crew::dispatch::build_dispatch_record_with_payload(
        level,
        "step result",
        "mission-run",
        session_id,
        None,
        Some(mission_id),
        Some(phase_id),
        Some(full),
    ));
}

/// The MAIN working tree of the current repository, resolved identically
/// whether invoked from the main checkout or from inside a linked worktree.
///
/// `mission run` creates the phase worktree off this repo and `mission ship`
/// recomputes that worktree's path from `(repo-name, phase)` — both must
/// agree on the repo name. `git rev-parse --show-toplevel` returns the
/// *current* working tree, which inside a mission's linked worktree is the
/// phase dir (basename = phase id, NOT the repo name); using it made
/// `mission ship` from inside a worktree recompute a different (wrong) path
/// than `mission run` created (#846). The first `worktree` entry of
/// `git worktree list --porcelain` is always the main working tree, so it
/// yields the stable repo name AND a valid dir to run worktree teardown from
/// (git refuses to remove the worktree you are standing in).
// (#1284 Packet 4a) `pub(crate)` — `mission_launch.rs`'s coder-phase
// execution path resolves the SAME repo root `mission run` does.
pub(crate) fn repo_root() -> Result<PathBuf> {
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

/// Deterministic worktree path for a phase: `<base>/<repo-name>/<phase-id>`.
/// Recomputable by `mission ship` from the same (repo, phase) inputs. The
/// base (`darkmux_types::workdir::worktrees_base_dir`) is outside the main
/// working tree by design — git refuses a worktree nested inside another,
/// and a stable, discoverable location lets `mission ship` recompute the
/// path without recording it in mission state.
fn worktree_path(repo_root: &Path, phase_id: &str) -> PathBuf {
    let repo_name = repo_root
        .file_name()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| "repo".to_string());
    darkmux_types::workdir::worktrees_base_dir()
        .join(repo_name)
        .join(phase_id)
}

/// Branch name for a phase's worktree. The phase id is already charset-
/// validated (`fleet::validate_identifier`) before this is called, so it's a
/// safe git ref component; we prefix `darkmux/` to namespace the branch and
/// keep it recognizable as a darkmux-managed worktree branch.
fn branch_name(phase_id: &str) -> String {
    format!("darkmux/{phase_id}")
}

/// (#816) Conventions-aware branch name: the repo's `branch_template`
/// (expanded + validated as a safe git ref) when present, else the
/// darkmux default. ALL THREE verbs (run / abort / ship) must resolve the
/// branch through this one fn so they always agree on the name. A
/// template that can't expand (ticketless mission) or expands to an
/// invalid ref falls back loudly-but-softly to the default.
fn conventions_branch(
    phase: &crew::types::Phase,
    mission: &crew::types::Mission,
    conv: Option<&crate::conventions::Conventions>,
) -> String {
    let default = branch_name(&phase.id);
    let Some(template) = conv.and_then(|c| c.branch_template.as_deref()) else {
        return default;
    };
    let vars = crate::conventions::Vars {
        ticket: mission.ticket.as_deref(),
        phase: &phase.id,
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

/// Choose which phase to run. Explicit `--phase` wins (validated to belong
/// to the mission and not be terminal). Otherwise auto-select the mission's
/// single next-runnable phase. (#1341) Phases are strictly linear — ordered
/// purely by position in `Mission.phase_ids`, no `depends_on` of their own
/// — so "runnable" is a linear scan: the FIRST phase in that order whose
/// status is `Planned` AND every phase before it is `Complete` (replaces
/// the historical `crew::scheduler::is_ready`/`PhaseNode` graph check,
/// itself a replacement for the even older flat `depends_on.is_empty()`
/// filter). A strictly-linear list has at most ONE next-runnable phase, so
/// the "0 or >1 ready is ambiguous" bail below only ever fires on the
/// EMPTY case now (kept as a real branch for defensiveness — a
/// hand-edited mission JSON could still produce an unexpected shape).
fn select_phase(
    phases: &[crew::types::Phase],
    mission_phase_ids: &[String],
    mission_id: &str,
    explicit: Option<&str>,
) -> Result<crew::types::Phase> {
    use crew::types::PhaseStatus;

    if let Some(id) = explicit {
        let s = phases
            .iter()
            .find(|s| s.id == id)
            .ok_or_else(|| anyhow::anyhow!("phase `{id}` not found"))?;
        if s.mission_id != mission_id {
            bail!(
                "phase `{id}` belongs to mission `{}`, not `{mission_id}`",
                s.mission_id
            );
        }
        if matches!(s.status, PhaseStatus::Complete) {
            bail!("phase `{id}` is already Complete (terminal) — nothing to run");
        }
        return Ok(s.clone());
    }

    let phase_by_id: std::collections::BTreeMap<&str, &crew::types::Phase> =
        phases.iter().map(|p| (p.id.as_str(), p)).collect();

    let mut ready: Vec<&crew::types::Phase> = Vec::new();
    let mut all_prior_complete = true;
    for phase_id in mission_phase_ids {
        let Some(phase) = phase_by_id.get(phase_id.as_str()) else { continue };
        if all_prior_complete && phase.status == PhaseStatus::Planned {
            ready.push(phase);
            break;
        }
        if phase.status != PhaseStatus::Complete {
            all_prior_complete = false;
        }
    }

    match ready.as_slice() {
        [] => bail!(
            "mission `{mission_id}` has no ready phase to run (need a Planned phase whose \
             predecessor in mission order is Complete). Pass `--phase <id>` to target one \
             explicitly, or check `darkmux mission show {mission_id}`."
        ),
        [one] => Ok((*one).clone()),
        many => {
            let ids: Vec<&str> = many.iter().map(|s| s.id.as_str()).collect();
            bail!(
                "mission `{mission_id}` has {} ready phases ({}) — unexpected for a strictly \
                 linear mission (hand-edited JSON?). `mission run` does one phase at a time — \
                 pass `--phase <id>` to choose.",
                many.len(),
                ids.join(", ")
            )
        }
    }
}

/// Create the git worktree for this phase, branching off `base`. If the
/// worktree path already exists (a prior `mission run` for the same phase
/// that wasn't shipped/torn down), bail with a pointer rather than clobbering
/// — the operator decides whether to resume, ship, or `git worktree remove`.
fn add_worktree(repo_root: &Path, wt_path: &Path, branch: &str, base: &str) -> Result<()> {
    if wt_path.exists() {
        bail!(
            "worktree already exists at {} — a previous `mission run` for this phase hasn't \
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

// ─── (#1230 Packet 3) Task/Step graph — `run()`'s old hand-written
// worktree → coder-dispatch → mechanical-verify sequence, now expressed as
// data (a 3-Task/3-Step graph) and executed through Packet 2's
// `scheduler::run_step_graph` instead of a hand-written function body. The
// GATE (blocker check, sign-off printing) and `ship()` (commit → PR →
// merge) stay outside the graph — genuinely Phase-level land/gate steps
// in #1240's vocabulary, not Steps.
//
// Each step kind below does its OWN printing + `emit_run_record` calls
// (rather than routing through the scheduler's generic `emit` hook) so the
// operator's live console narration keeps its pre-migration ordering: the
// graph is a strict linear chain (worktree → coder → verify, each
// `depends_on` the last), so no step ever runs concurrently with another
// in this specific graph shape, and interleaving each kind's own prints
// inside `run` reproduces `run()`'s old inline ordering exactly.
//
// Rich per-step results (the full `DispatchResult`, the full
// `PhaseReviewOutput`) don't fit the generic `StepOutcome.output: String`
// contract, so each kind stashes its structured result into a side-channel
// `Arc<Mutex<Option<T>>>` slot the caller reads after `run_step_graph`
// returns — `Step.output` still carries a plain-text summary for
// consistency with every other step kind's convention.

use crew::step_kinds::{resolve_local_placement, StepKind, StepOutcome};
use std::sync::{Arc, Mutex};

/// Wraps the worktree-creation half of the old hand-written sequence
/// (moved here verbatim from the pre-migration `add_worktree` free
/// function, deleted below — this was its only caller). Printing + the
/// `"step result"` flow record (`kind: "mission.worktree"`) happen HERE, at
/// the same point in the sequence `run()` always emitted them (right after
/// worktree creation succeeds). `darkmux-serve`'s `resolve_session` (the
/// `/diff` endpoint) reads this exact record for its worktree/base/branch
/// payload — see [`emit_step_result`]'s doc.
///
/// **Tier 3 audit (#1352).** `add_worktree` is a `git worktree` shell-out,
/// which on its face looks like it could collapse into Tier 1's
/// `procedural.shell`. Audited: NO — `darkmux-serve`'s `/diff` endpoint
/// depends on the EXACT `kind: "mission.worktree"` flow-record payload
/// shape emitted here (worktree/base/branch/role fields), which
/// `procedural.shell`'s generic stdout-only output doesn't produce, and the
/// CLI success styling (`style::success`) is mission-run-specific
/// presentation. Collapsing would change a downstream consumer's contract
/// — stays Tier 3, physically co-located with the mission module that owns
/// it (see `darkmux-crew`'s `step_kinds::patterns` module doc for the
/// three-tier picture).
// (#1284 Packet 4a) `pub(crate)` (struct + every field) — `mission_launch.rs`
// registers this SAME Tier 3 kind (#1352) when it runs the `coder-phase`
// config through the config-launched path, the proving case for the
// instance-model collapse. Reusing the type directly (rather than a second
// near-identical struct) keeps the `mission.worktree` flow-record shape and
// `darkmux-serve`'s `/diff` contract (see this struct's own doc above)
// byte-identical across both launchers.
pub(crate) struct MissionWorktreeStepKind {
    pub(crate) repo_root: PathBuf,
    pub(crate) wt_path: PathBuf,
    pub(crate) branch: String,
    pub(crate) base: String,
    pub(crate) mission_id: String,
    pub(crate) phase_id: String,
    pub(crate) session_id: String,
    pub(crate) role: String,
}

impl StepKind for MissionWorktreeStepKind {
    fn id(&self) -> &'static str {
        "mission.worktree"
    }

    fn display_name(&self) -> &'static str {
        "Worktree"
    }

    fn run(
        &self,
        step: &crew::types::Step,
        _task: &crew::types::Task,
        _input: &std::collections::BTreeMap<String, String>,
    ) -> Result<StepOutcome> {
        add_worktree(&self.repo_root, &self.wt_path, &self.branch, &self.base)?;

        println!(
            "{}",
            style::success(&format!("✓ worktree ready at {}", self.wt_path.display()))
        );
        emit_step_result(
            flow::Level::Info,
            "mission.worktree",
            &step.id,
            &self.mission_id,
            &self.phase_id,
            &self.session_id,
            serde_json::json!({
                "role": self.role,
                "base": self.base,
                "branch": self.branch,
                "worktree": self.wt_path.display().to_string(),
            }),
        );

        Ok(StepOutcome {
            output: self.wt_path.display().to_string(),
            flow_records: Vec::new(),
        })
    }
}

/// The coder-dispatch step's rich result — stashed into
/// `MissionCoderStepKind::result_slot` since it doesn't fit the generic
/// `StepOutcome.output: String` contract. `run()` reads it after
/// `run_step_graph` returns to reconstruct the exact detail the
/// pre-migration inline code had at hand.
// (#1284 Packet 4a) `pub(crate)` — read back by `mission_launch.rs` after
// `run_step_graph` returns, same as `run()` does below, to build its own
// `MissionEnvelope` summary.
pub(crate) struct CoderStepResult {
    pub(crate) failed_verifiers: Vec<crew::step_kinds::FailedVerifier>,
    pub(crate) tokens_total: u32,
}

/// Wraps the coder-dispatch half of the old hand-written sequence
/// (`fleet::dispatch_routed` → token tally → exit-code branch → verifier
/// parsing). `opts.machine` is always `None` for `mission run` (no
/// `--machine` flag on this verb), and `fleet::dispatch_routed` with
/// `machine: None` falls straight through to `crew::dispatch::dispatch` —
/// so calling `dispatch::dispatch` directly here is behavior-identical and
/// avoids a `darkmux-crew` → `darkmux-fleet` dependency cycle (fleet
/// depends on crew, not the reverse — see `crates/darkmux-fleet/src/
/// routing.rs`'s module doc).
///
/// **A non-zero exit code is a Step-level `Err`, not a `Complete`.**
/// `dispatch::dispatch` itself returns `Ok(DispatchResult)` even when the
/// dispatched coder exited non-cleanly (the container ran; the coder's OWN
/// run didn't finish cleanly) — treating that as `NodeStatus::Complete`
/// would let the scheduler mark `verify` ready even though `run()` never
/// wanted QA to run against a failed coder dispatch. Returning `Err` here
/// maps that onto `NodeStatus::Error`, which correctly makes `verify`
/// unreachable (see `scheduler::reachable`) — the same "coder failed, skip
/// QA entirely" behavior the pre-migration early-`return Ok(1)` had.
///
/// **Tier 3 audit finding (#1352).** This kind wraps THE SAME
/// `crew::dispatch::dispatch` primitive Tier 1's `dispatch.internal` wraps
/// — a genuine follow-up candidate for collapsing into `dispatch.internal`
/// config someday. Not done in this packet: the CLI printing
/// (`style::success`/`style::error` with mission-specific remediation
/// text), the `mission.coder` flow-record vocabulary + `mission_id`/
/// `phase_id`/`session_id` fields (a DIFFERENT shape from
/// `dispatch.internal`'s own `"step result"`/`kind: "dispatch.internal"`
/// record), and the `result_slot` mechanism `run()` reads back rich detail
/// through are all real behavior/envelope differences a collapse would
/// have to change or drop — outside this packet's pure-refactor scope.
/// Left documented, not forced.
// (#1284 Packet 4a) `pub(crate)` — see `MissionWorktreeStepKind`'s doc.
pub(crate) struct MissionCoderStepKind {
    pub(crate) opts: Mutex<Option<crew::dispatch::DispatchOpts>>,
    pub(crate) wt_path: PathBuf,
    pub(crate) mission_id: String,
    pub(crate) phase_id: String,
    pub(crate) session_id: String,
    pub(crate) role_id: String,
    pub(crate) result_slot: Arc<Mutex<Option<CoderStepResult>>>,
}

impl StepKind for MissionCoderStepKind {
    fn id(&self) -> &'static str {
        "mission.coder"
    }

    fn display_name(&self) -> &'static str {
        "Coder"
    }

    fn run(
        &self,
        step: &crew::types::Step,
        _task: &crew::types::Task,
        _input: &std::collections::BTreeMap<String, String>,
    ) -> Result<StepOutcome> {
        let opts = self
            .opts
            .lock()
            .expect("mission.coder opts mutex poisoned")
            .take()
            .ok_or_else(|| anyhow::anyhow!("mission.coder step ran more than once"))?;
        let result = crew::dispatch::dispatch(opts)?;
        eprintln!(
            "{}",
            style::dim(&format!("darkmux coder-phase: session id `{}`", self.session_id))
        );

        let tokens = result
            .out_dir
            .as_deref()
            .map(crew::dispatch_internal::read_token_totals)
            .unwrap_or_default();

        if result.exit_code != 0 {
            let exit_code = result.exit_code;
            eprintln!(
                "{}",
                style::error(&format!(
                    "✗ coder dispatch exited {exit_code} — see stderr above. The phase stays \
                     Running and the worktree is left at {} for inspection. Re-running `darkmux \
                     mission run` will refuse until you tear it down: `darkmux mission abort {} \
                     --phase {}`.",
                    self.wt_path.display(),
                    self.mission_id,
                    self.phase_id,
                ))
            );
            print_token_line(&tokens);
            emit_step_result(
                flow::Level::Error,
                "mission.coder",
                &step.id,
                &self.mission_id,
                &self.phase_id,
                &self.session_id,
                serde_json::json!({ "exit_code": exit_code, "total_tokens": tokens.total() }),
            );
            *self.result_slot.lock().expect("mission.coder result mutex poisoned") = Some(CoderStepResult {
                failed_verifiers: Vec::new(),
                tokens_total: tokens.total(),
            });
            anyhow::bail!("coder dispatch exited {exit_code}");
        }

        println!("{}", style::success("✓ coder dispatch complete"));
        print_token_line(&tokens);

        let failed_verifiers = parse_failed_verifiers(&result.stdout);
        emit_step_result(
            if failed_verifiers.is_empty() {
                flow::Level::Info
            } else {
                flow::Level::Warn
            },
            "mission.coder",
            &step.id,
            &self.mission_id,
            &self.phase_id,
            &self.session_id,
            serde_json::json!({
                "failed_verifiers": failed_verifiers,
                "count": failed_verifiers.len(),
                "total_tokens": tokens.total(),
            }),
        );

        let stdout = result.stdout.clone();
        let tokens_total = tokens.total();
        *self.result_slot.lock().expect("mission.coder result mutex poisoned") = Some(CoderStepResult {
            failed_verifiers,
            tokens_total,
        });

        Ok(StepOutcome {
            output: stdout,
            flow_records: Vec::new(),
        })
    }

    fn residency(&self, _step: &crew::types::Step, _task: &crew::types::Task) -> Option<crew::step_kinds::Placement> {
        resolve_local_placement(&self.role_id, None, None, &format!("mission-coder:{}", self.phase_id))
    }
}

/// Wraps the mechanical-verify half of the old hand-written sequence
/// (`phase_cli::phase_review_output_at` against the worktree diff). Its
/// `run()` role is ALWAYS `"code-reviewer"` — that's hardcoded inside
/// `phase_review_output_at` itself (mirrors the standalone `darkmux
/// phase review` verb), not something `mission run` overrides.
///
/// **Tier 3 (#1352), on purpose.** Wraps the whole `phase_cli`
/// mechanical-review pipeline (a multi-step process of its own, not a
/// single dispatch), with a hardcoded role and mission-run-specific
/// CLI/result-slot plumbing. No second consumer visible today — stays
/// physically co-located with the mission module that owns it.
// (#1284 Packet 4a) `pub(crate)` — see `MissionWorktreeStepKind`'s doc.
pub(crate) struct MissionVerifyStepKind {
    pub(crate) wt_path: PathBuf,
    pub(crate) base: String,
    pub(crate) phase_id: String,
    pub(crate) result_slot: Arc<Mutex<Option<std::result::Result<crate::phase_cli::PhaseReviewOutput, String>>>>,
}

impl StepKind for MissionVerifyStepKind {
    fn id(&self) -> &'static str {
        "mission.verify"
    }

    fn display_name(&self) -> &'static str {
        "Verify (QA)"
    }

    fn run(
        &self,
        _step: &crew::types::Step,
        _task: &crew::types::Task,
        _input: &std::collections::BTreeMap<String, String>,
    ) -> Result<StepOutcome> {
        println!(
            "\n{}",
            style::header("▶ local QA — dispatching `code-reviewer` against the worktree diff…")
        );

        match crate::phase_cli::phase_review_output_at(&self.wt_path, Some(&self.base), Some(&self.phase_id)) {
            Ok(review) => {
                print_review_summary(&review);
                let verdict = review.verdict.clone();
                *self.result_slot.lock().expect("mission.verify result mutex poisoned") = Some(Ok(review));
                Ok(StepOutcome {
                    output: verdict,
                    flow_records: Vec::new(),
                })
            }
            Err(e) => {
                let msg = format!("{e:#}");
                eprintln!(
                    "{}",
                    style::warn(&format!(
                        "⚠ QA could not run ({msg}). The coder's work is in the worktree — review \
                         the diff manually before shipping."
                    ))
                );
                *self.result_slot.lock().expect("mission.verify result mutex poisoned") = Some(Err(msg.clone()));
                Err(anyhow::anyhow!(msg))
            }
        }
    }

    fn residency(&self, _step: &crew::types::Step, _task: &crew::types::Task) -> Option<crew::step_kinds::Placement> {
        resolve_local_placement("code-reviewer", None, None, &format!("mission-verify:{}", self.phase_id))
    }
}


/// (#1426 ship-4) Resolve the on-disk worktree of a gate-held coder-phase run
/// for `mission ship`/`mission abort`. `mission launch coder-phase` sets the
/// worktree from its operator-supplied `workdir` input and persists it on the
/// phase's tasks (`Task.workdir`); read it back so ship/abort target the ACTUAL
/// worktree even when the operator launched into a non-default location. Falls
/// back to the derived `worktree_path(root, phase_id)` (the retired `mission
/// run`'s own convention) when no task records a workdir — an old on-disk run,
/// or a phase whose task records predate the workdir plumbing.
fn resolve_run_workdir(mission_id: &str, phase_id: &str, root: &Path) -> PathBuf {
    crew::lifecycle::load_tasks_for_phase(mission_id, phase_id)
        .ok()
        .into_iter()
        .flatten()
        .find_map(|t| t.workdir)
        .unwrap_or_else(|| worktree_path(root, phase_id))
}

/// (#1426 ship-4 / #1433) After `ship`/`abort` drives a phase to a terminal
/// status, bring the MISSION to an honest terminal state too — but only when
/// EVERY phase is terminal (Complete/Abandoned). A mission with a still-open
/// phase elsewhere stays Active so the operator finishes the remaining phases.
/// The envelope's per-phase outcomes come from each phase's ACTUAL on-disk
/// status (#1433's honest-finalize discipline — never a uniform status stamped
/// across phases): a phase `ship` completed reads `Complete`, one `abort`
/// abandoned reads `Abandoned`. Best-effort — a load hiccup leaves the mission
/// as-is (reconcilable via `darkmux mission close`), never fails ship/abort.
fn finalize_mission_if_complete(mission_id: &str) {
    use crew::envelope::{MissionEnvelope, MissionOutcomeStatus, PhaseOutcome, PhaseOutcomeKind};
    use crew::types::PhaseStatus;

    let phases = match crew::loader::load_phases() {
        Ok(p) => p,
        Err(_) => return,
    };
    let mine: Vec<&crew::types::Phase> =
        phases.iter().filter(|p| p.mission_id == mission_id).collect();
    if mine.is_empty() {
        return;
    }
    let all_terminal = mine
        .iter()
        .all(|p| matches!(p.status, PhaseStatus::Complete | PhaseStatus::Abandoned));
    if !all_terminal {
        return;
    }
    let outcomes: Vec<PhaseOutcome> = mine
        .iter()
        .map(|p| {
            let outcome = match p.status {
                PhaseStatus::Complete => PhaseOutcomeKind::Complete,
                _ => PhaseOutcomeKind::Abandoned,
            };
            PhaseOutcome { phase_id: p.id.clone(), outcome, reason: None }
        })
        .collect();
    let all_complete = outcomes.iter().all(|o| o.outcome == PhaseOutcomeKind::Complete);
    let status = if all_complete {
        MissionOutcomeStatus::Clean
    } else {
        // A mix (or all abandoned) — real work happened but not every phase
        // completed cleanly; Degraded reads honestly on the board.
        MissionOutcomeStatus::Degraded
    };
    let mut envelope = MissionEnvelope::new(mission_id, status, &[]);
    envelope.phases = outcomes;
    // `finalize_mission` re-applies each (already-terminal) phase outcome as a
    // benign idempotent no-op, closes the mission, and persists envelope.json.
    crew::envelope::finalize_mission(&envelope);
}

/// `darkmux mission abort` — the explicit teardown half of the hybrid
/// contract. Removes the gate-held coder-phase run's worktree + its branch and
/// flips the phase `Running → Abandoned`, so a frontier/operator who decides
/// mid-loop that the run is going nowhere can cleanly back it out (vs. leaving
/// an orphan worktree). When abandoning the phase leaves EVERY phase terminal,
/// the mission is finalized honestly too (#1426 ship-4). Idempotent-ish: a
/// missing worktree/branch is reported, not fatal. Returns `0` on a clean
/// teardown.
pub fn abort(mission_id: &str, phase_id: Option<&str>) -> Result<i32> {
    use crew::loader::{load_missions, load_phases};

    fleet::validate_identifier("mission_id", mission_id)?;
    if let Some(s) = phase_id {
        fleet::validate_identifier("--phase", s)?;
    }

    let missions = load_missions()?;
    let mission = missions
        .iter()
        .find(|m| m.id == mission_id)
        .ok_or_else(|| anyhow::anyhow!("mission `{mission_id}` not found"))?;
    let phases = load_phases()?;
    // Explicit `--phase` resolves by id (any status — a Running phase, the
    // common abort case after a `run`, resolves); auto-path requires a ready
    // Planned phase. So to abort a Running phase, pass `--phase`.
    let phase = resolve_phase(&phases, &mission.phase_ids, mission_id, phase_id)?;

    let root = repo_root()?;
    // (#1426 ship-4) Target the ACTUAL launch worktree (Task.workdir), and
    // delete the branch the worktree is actually on — a launch may have used a
    // non-default workdir/branch. Fall back to the derived path/name when the
    // worktree is already gone or a task didn't record one.
    let wt_path = resolve_run_workdir(mission_id, &phase.id, &root);
    let conv = crate::conventions::load(&root);
    let branch =
        worktree_branch(&wt_path).unwrap_or_else(|| conventions_branch(&phase, mission, conv.as_ref()));

    println!(
        "{}",
        style::header(&format!(
            "▶ mission abort — {} · phase {}",
            mission_id, phase.id
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

    // Flip the phase Running/Planned → Abandoned (legal restart later).
    match crew::lifecycle::phase_abandon(&phase.id) {
        Ok(_) => println!("{}", style::success(&format!("✓ phase {} → Abandoned", phase.id))),
        Err(e) => eprintln!(
            "{}",
            style::warn(&format!(
                "phase_abandon({}) failed: {e:#} — reconcile with `darkmux phase` verbs.",
                phase.id
            ))
        ),
    }

    let session_id = darkmux_types::session_id::mission_run(mission_id, &phase.id);
    emit_run_record(
        flow::Level::Info,
        "mission.run.abort",
        mission_id,
        &phase.id,
        &session_id,
        serde_json::json!({ "branch": branch, "worktree": wt_path.display().to_string() }),
    );

    // (#1426 ship-4 / #1433) Honest finalize: abandoning this phase may have
    // left every phase terminal — if so, close the mission with an honest
    // envelope instead of stranding it Active with all-terminal phases.
    finalize_mission_if_complete(mission_id);
    Ok(0)
}

/// Resolve the phase a post-run verb (`ship` / `abort`) targets. An explicit
/// `--phase` is looked up by id directly (no status filter — so a Running
/// phase, the common post-`run` case, resolves); otherwise fall back to
/// `select_phase`'s ready-Planned auto-pick.
fn resolve_phase(
    phases: &[crew::types::Phase],
    mission_phase_ids: &[String],
    mission_id: &str,
    explicit: Option<&str>,
) -> Result<crew::types::Phase> {
    match explicit {
        Some(id) => phases
            .iter()
            .find(|s| s.id == id && s.mission_id == mission_id)
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("phase `{id}` not found in mission `{mission_id}`")),
        None => select_phase(phases, mission_phase_ids, mission_id, None),
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

/// Commit subject for a shipped phase: the phase description's first line,
/// trimmed to a conventional ~72-char subject.
/// (#815) The coder's dispatch brief: the phase's compiled description
/// (the STRUCTURE) plus, when the mission carries it, the operator's
/// verbatim `mission propose` input (the WORDS) under a provenance-tagged
/// block. The 2026-06-12 dogfood showed the compiler compressing exact
/// strings + constraints out of the description — and since the description
/// IS the brief, the constraints never reached the coder. The tagged block
/// follows the model-facing prompt doctrine: AI-convention framing, with
/// the tag itself carrying the provenance a clean-context model needs.
// (#1284 Packet 4a) `pub(crate)` — `mission_launch.rs`'s coder-phase
// execution path builds its dispatch brief through the SAME function
// (called with empty lessons/corrections/cautions slices — a freshly
// launched instance has no prior-dispatch history to carry forward).
pub(crate) fn coder_brief(
    phase: &crew::types::Phase,
    mission: &crew::types::Mission,
    lessons: &[String],
    prior_corrections: &[String],
    cautions: &[String],
) -> String {
    let base = match mission.source_input.as_deref().map(str::trim) {
        Some(src) if !src.is_empty() => format!(
            "{desc}\n\n<operator-source-input>\nThe user's original, unabridged request that \
             produced this phase. The summary above is derived from it; where this text \
             adds constraints, exact strings, or scope limits beyond the summary, THIS \
             text is authoritative.\n\n{src}\n</operator-source-input>",
            desc = phase.description,
        ),
        _ => phase.description.clone(),
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

/// (#1426 ship-4) Build the coder brief for `phase` WITH the injected context
/// the retired `mission run` gathered before every coder dispatch: prior
/// adjudication corrections (#849), detector cautions (#994), and engagement
/// lessons, ranked and budgeted against the dispatch model's context window
/// (#1011). Prints the SAME operator-facing provenance lines `mission run`
/// printed — what's carried into the brief, and why — then returns the
/// assembled brief. Moved out of the deleted `run()` in the ship-4 collapse so
/// `mission launch coder-phase` (now the ONLY coder path) keeps the doom-loop
/// fix instead of dispatching a bare brief. `wt_path` is the phase worktree the
/// staleness check reads.
pub(crate) fn coder_brief_with_injected_context(
    mission_id: &str,
    mission: &crew::types::Mission,
    phase: &crew::types::Phase,
    wt_path: &Path,
) -> Result<String> {
    // The mission's EXACT dispatch session ids (built from its real phase
    // ids), so the collectors scope to THIS mission's sessions — an exact-set
    // match, never a `mission-run-<id>-` prefix that would bleed a sibling
    // mission whose id is a hyphen-extension (see `mission_adjudication_notes`).
    let mission_session_ids: std::collections::HashSet<String> = mission
        .phase_ids
        .iter()
        .map(|pid| darkmux_types::session_id::mission_run(mission_id, pid))
        .collect();
    // (#1002) Files this dispatch is about to work on (from the phase
    // description) — used to rank file-in-play cautions + lessons above
    // engagement-level ones, and to staleness-check cautions against the
    // worktree's current content.
    let intent = intent_files(&phase.description);

    // (#994) The three injected-context sources, each fully ranked but UNCAPPED
    // here — the proportional budget (#1011) decides how much of each lands.
    // Authority order: corrections > lessons > cautions.
    let corrections = mission_adjudication_notes(&mission_session_ids);
    let cautions = mission_cautions(&mission_session_ids, &intent, wt_path);
    let authored = engagement_lessons(&intent);

    // (#1011) Distribute a single budget — a fraction of THIS dispatch model's
    // context window — across the blocks with per-authority floors.
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

    Ok(coder_brief(
        phase,
        mission,
        &lessons,
        &prior_corrections,
        &detected_cautions,
    ))
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
    use crew::loader::load_phases;
    // No phase at lab time → no files-in-play; lessons/cautions still inject at
    // engagement scope.
    let intent = std::collections::HashSet::new();
    let (corrections, cautions) = match mission_id {
        Some(mid) => {
            let ids: std::collections::HashSet<String> = load_phases()
                .unwrap_or_default()
                .iter()
                .filter(|s| s.mission_id.as_str() == mid)
                .map(|s| darkmux_types::session_id::mission_run(mid, &s.id))
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
/// mission's phases as `mission-run-<mission>-<phase>`). Exact-set match, NOT
/// a `mission-run-<mission>-` prefix — a prefix bleeds a sibling mission whose
/// id is a hyphen-extension (`auth` would swallow `auth-v2`'s notes, since
/// `mission-run-auth-v2-s1` starts with `mission-run-auth-`). Mission-scoped,
/// not phase-scoped, by design — a correction like "don't rename that field"
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

/// (#1002) The files a dispatch is about to work on, derived from the phase
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

fn phase_status_label(s: crew::types::PhaseStatus) -> &'static str {
    use crew::types::PhaseStatus::*;
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
/// (not borrowed from the loaded mission/phase Vecs) so the gather + render are
/// cleanly separable and the gather is unit-testable without stdout capture.
struct DebriefReport {
    mission_id: String,
    mission_description: String,
    mission_status: &'static str,
    /// (phase_id, first-line description, status label) per phase.
    phases: Vec<(String, String, &'static str)>,
    /// Already bullet-formatted by [`mission_cautions`].
    cautions: Vec<String>,
    /// The reviewer's adjudication notes (#849), as recorded.
    corrections: Vec<String>,
}

/// (#1000) Gather the debrief raw material for `mission_id`: the loop
/// pathologies darkmux's detectors flagged across the mission's runs (cautions),
/// the corrections the reviewer recorded (#849), and the mission's phases + how
/// each ended. READ-ONLY.
///
/// The flow stream IS the mission's durable history (the #557 single-stream
/// doctrine); this reads it scoped to the mission's EXACT dispatch session ids
/// (same `mission-run-<id>-<phase>` construction as the run path, so a sibling
/// mission whose id is a hyphen-extension never bleeds in). It does NOT assume a
/// coding mission — no git diffs are reconstructed here: for a coding mission
/// the `darkmux-mission-debrief` skill pulls the actual patch with `git show`,
/// and a non-coding mission simply has no coding activity.
fn gather_debrief(mission_id: &str) -> Result<DebriefReport> {
    use crew::loader::{load_missions, load_phases};
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

    let phases = load_phases()?;
    let mission_phases: Vec<&crew::types::Phase> = phases
        .iter()
        .filter(|s| s.mission_id.as_str() == mission_id)
        .collect();

    // The mission's exact dispatch session ids — the coder-phase dispatch id
    // for each phase, so the collectors scope to THIS mission's sessions (no
    // sibling bleed).
    let mission_session_ids: std::collections::HashSet<String> = mission_phases
        .iter()
        .map(|s| darkmux_types::session_id::mission_run(mission_id, &s.id))
        .collect();

    Ok(DebriefReport {
        mission_id: mission.id.clone(),
        mission_description: mission.description.clone(),
        mission_status: mission_status_label(mission.status),
        phases: mission_phases
            .iter()
            .map(|s| {
                (
                    s.id.clone(),
                    s.description.lines().next().unwrap_or("").trim().to_string(),
                    phase_status_label(s.status),
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
/// debrief material (cautions + corrections + phases) for the post-mission
/// review ceremony. `--json` feeds the `darkmux-mission-debrief` skill, which
/// distills durable `lessons` (with the why) for the next crew. The ceremony
/// that turns transient signal into durable lessons — NASA Lessons Learned,
/// applied locally.
pub fn debrief(mission_id: &str, json: bool) -> Result<i32> {
    let report = gather_debrief(mission_id)?;

    if json {
        let phases_json: Vec<serde_json::Value> = report
            .phases
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
            "phases": phases_json,
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

    println!("{}", style::header("phases"));
    if report.phases.is_empty() {
        println!("  {}", style::dim("(none)"));
    } else {
        for (id, desc, status) in &report.phases {
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
/// happened live (corrections + cautions carried phase→phase at run time);
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
        phase_id: None,
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

/// (#817) Soft nudge printed at ship time when a gated phase is shipping
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
/// toolchain failed to load — so it never actually verified anything. A
/// non-empty list means a coder SIGNOFF claiming "tests pass" may rest on a
/// command that never ran. SOFT signal end to end: surfaced for the
/// adjudicator, never an auto-fail (operator sovereignty #44).
///
/// (#1230 Packet 4 DRY pass) `FailedVerifier`/`parse_failed_verifiers`
/// moved to `crew::step_kinds::builtins` so ANY `dispatch.internal`-shaped
/// step can opt into this parse (`config.parse_verifiers: true`), not just
/// `mission.coder` — re-exported here under their original names via `use`
/// so every call site below is unchanged.
use crew::step_kinds::{parse_failed_verifiers, FailedVerifier};

/// (#799) Prominent gate banner naming the verifier commands that FAILED TO
/// RUN. No-op on an honest run (empty list). Soft — it informs the adjudicator
/// at the gate; it never blocks `mission run` (operator sovereignty #44). The
/// list is what lets the operator cross-check the coder's SIGNOFF: a "tests
/// pass" claim sitting next to "the test command never ran" is the
/// contradiction this exists to surface.
// (#1284 Packet 4a) `pub(crate)` — `mission_launch.rs`'s coder-phase gate
// prints the SAME banner at the same decision point.
pub(crate) fn print_unverified_banner(failed: &[FailedVerifier]) {
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

/// (#799) The verifier commands the LATEST run's coder FAILED TO RUN, read
/// back from the flow trail by the run's deterministic session id
/// (`mission-run-<mission>-<phase>`). The coder step emits a `"step
/// result"` record (`payload.kind: "mission.coder"`,
/// `payload.failed_verifiers: [{command, reason}]`) on EVERY run — empty on
/// an honest run — so `ship` reads the latest run's status and HOLDs an
/// auto-merge only when that run had failures (#1230 Packet 4: migrated
/// off the retired `mission.run.verification` action — see
/// [`emit_step_result`]'s doc). The run is a separate process, so the flow
/// trail is the durable handoff (the runtime's out-dir is an ephemeral
/// per-dispatch tempdir ship can't reconstruct). Scans the last 2 days
/// oldest→newest and OVERWRITES `latest` on each match, so a clean re-run's
/// empty record correctly clears a prior dirty run's (latest-wins on a
/// resumed phase). Best effort: any IO/parse problem, or no record in the
/// recent window, reads as "none" — this soft backstop fails OPEN (the
/// run-time banner is the primary surface). Mirrors
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
    // earlier one and the most recent coder `"step result"` for this
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
            let payload = r.get("payload");
            let kind = payload.and_then(|p| p.get("kind")).and_then(|k| k.as_str());
            if r.get("action").and_then(|v| v.as_str()) == Some("step result")
                && kind == Some("mission.coder")
                && r.get("session_id").and_then(|v| v.as_str()) == Some(session_id)
            {
                if let Some(arr) = payload
                    .and_then(|p| p.get("failed_verifiers"))
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

fn commit_subject(phase: &crew::types::Phase) -> String {
    let first = phase.description.lines().next().unwrap_or("").trim();
    let s = if first.is_empty() {
        format!("darkmux phase {}", phase.id)
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

/// PR body — phase + mission provenance. Authored by the LOCAL coder via
/// `mission run`; the body says so (no frontier/Claude co-author claim — this
/// is local-AI work shipped through darkmux's loop).
fn pr_body(mission: &crew::types::Mission, phase: &crew::types::Phase) -> String {
    format!(
        "## {phase_desc}\n\n\
         Shipped via `darkmux mission ship` — the local dispatch-to-PR loop.\n\n\
         - **Mission:** `{mission_id}` — {mission_desc}\n\
         - **Phase:** `{phase_id}`\n\n\
         The implementation was produced by the local-AI coder under \
         `darkmux mission run` and reviewed by the local `code-reviewer` before \
         sign-off. The frontier/operator adjudicated the QA findings at the gate.",
        phase_desc = phase.description.lines().next().unwrap_or("").trim(),
        mission_id = mission.id,
        mission_desc = mission.description.lines().next().unwrap_or("").trim(),
        phase_id = phase.id,
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
    phase: &crew::types::Phase,
    mission: &crew::types::Mission,
    what: &str,
) -> String {
    let default = commit_subject(phase);
    let Some(t) = template else { return default };
    let vars = crate::conventions::Vars {
        ticket: mission.ticket.as_deref(),
        phase: &phase.id,
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
    phase: &crew::types::Phase,
    conv: Option<&crate::conventions::Conventions>,
    repo_root: &Path,
) -> String {
    let summary = pr_body(mission, phase);
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
/// (`darkmux/<phase-id>`) that could hand back a STALE merged PR; the ship
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
/// (opt-in, green-gated) squash-merges, flips the phase to Complete, and
/// tears the worktree down. **Never auto-merges** — `--merge` is the operator/
/// frontier's explicit sign-off act. Returns `0` on success, `1` on a refused
/// merge (CI not green), `2` when the PR merged but the phase couldn't be
/// marked Complete (inconsistent state — needs manual reconcile), `3` when
/// `--merge` is HELD because the run had verifier commands that failed to run
/// (#799 — review the SIGNOFF, then merge manually or re-run after fixing).
pub fn ship(
    mission_id: &str,
    phase_id: Option<&str>,
    base: &str,
    wait_ci: bool,
    merge: bool,
) -> Result<i32> {
    use crew::loader::{load_missions, load_phases};

    fleet::validate_identifier("mission_id", mission_id)?;
    if let Some(s) = phase_id {
        fleet::validate_identifier("--phase", s)?;
    }

    let missions = load_missions()?;
    let mission = missions
        .iter()
        .find(|m| m.id == mission_id)
        .ok_or_else(|| anyhow::anyhow!("mission `{mission_id}` not found"))?
        .clone();
    let phases = load_phases()?;
    let phase = resolve_phase(&phases, &mission.phase_ids, mission_id, phase_id)?;

    // A Complete phase is terminal — a prior `--merge` already shipped it
    // (and tore down its worktree). Re-shipping would duplicate-PR or churn;
    // refuse rather than confuse.
    if matches!(phase.status, crew::types::PhaseStatus::Complete) {
        bail!(
            "phase `{}` is already Complete (terminal) — nothing to ship.",
            phase.id
        );
    }

    let root = repo_root()?;
    // (#1426 ship-4) Target the ACTUAL launch worktree (Task.workdir), falling
    // back to the derived path for an old on-disk run.
    let wt_path = resolve_run_workdir(mission_id, &phase.id, &root);
    let conv = crate::conventions::load(&root);
    // (#816) Ship pushes the branch the worktree is ACTUALLY on — created at
    // launch time — not a recomputation. If conventions.json changed
    // between launch and ship, recomputing would target a branch that doesn't
    // exist (QA drift finding). The computed name is only the fallback for
    // a worktree whose HEAD can't be read.
    let branch = worktree_branch(&wt_path)
        .unwrap_or_else(|| conventions_branch(&phase, &mission, conv.as_ref()));
    let session_id = darkmux_types::session_id::mission_run(mission_id, &phase.id);

    if !wt_path.exists() {
        bail!(
            "no worktree at {} — run `darkmux mission run {mission_id} --phase {}` first.",
            wt_path.display(),
            phase.id
        );
    }

    println!(
        "{}",
        style::header(&format!("▶ mission ship — {} · phase {}", mission_id, phase.id))
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
        &phase, &mission, "commit subject",
    );
        let msg = format!(
            "{subject}\n\nAuthored via `darkmux mission run` (local-AI coder, phase {}).",
            phase.id
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
                &phase, &mission, "PR title",
            );
            let body = conventioned_pr_body(&mission, &phase, conv.as_ref(), &root);
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
        &phase.id,
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
                     it and re-run `darkmux mission run {mission_id} --phase {}`.",
                    phase.id
                ))
            );
            emit_run_record(
                flow::Level::Warn,
                "mission.run.ship.held",
                mission_id,
                &phase.id,
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
                     then re-run `darkmux mission ship {mission_id} --phase {} --merge`.",
                    phase.id
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
            // skip phase-complete + teardown → silent drift (merged PR, phase
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
                         `darkmux phase complete {}` and `git worktree remove --force {}`. \
                         gh stderr: {}",
                        phase.id,
                        wt_path.display(),
                        String::from_utf8_lossy(&out.stderr).trim()
                    );
                }
            }
        }
        println!("{}", style::success(&format!("✓ merged {branch} (squash)")));

        // Flip the phase Complete + tear down the worktree. The merge is
        // already irreversible, so a phase_complete failure can't roll it
        // back — but it leaves merged-PR-but-Running-phase, so we must NOT
        // claim a clean "loop closed". Track the outcome and exit non-zero
        // with a reconcile pointer if completion didn't take.
        let complete_ok = match crew::lifecycle::phase_complete(&phase.id) {
            Ok(_) => {
                println!("{}", style::success(&format!("✓ phase {} → Complete", phase.id)));
                true
            }
            Err(e) => {
                eprintln!(
                    "{}",
                    style::error(&format!("✗ phase_complete({}) failed: {e:#}", phase.id))
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
        // branch) now gone, reap the local branch ourselves so shipped phases
        // don't accrete dead `darkmux/<phase>` refs. Safe unconditionally:
        // if gh already deleted it, `-D` exits 1 (swallowed); if the worktree
        // removal above FAILED, the branch is still pinned and git `-D` refuses
        // outright — so this never orphan-kills a branch holding live work.
        let _ = git_in(&root, &["branch", "-D", &branch]);

        emit_run_record(
            flow::Level::Info,
            "mission.run.ship.merged",
            mission_id,
            &phase.id,
            &session_id,
            serde_json::json!({ "pr_url": pr_url, "phase_completed": complete_ok }),
        );
        if !complete_ok {
            eprintln!(
                "{}",
                style::error(&format!(
                    "PR was MERGED but phase `{}` could not be marked Complete — state is \
                     inconsistent. Reconcile with `darkmux phase complete {}`.",
                    phase.id, phase.id
                ))
            );
            return Ok(2);
        }
        println!("\n{}", style::success("✓ phase shipped + merged. Loop closed."));
        // (#1426 ship-4 / #1433) Honest finalize: completing this phase may have
        // left every phase terminal — if so, close the mission with an honest
        // envelope instead of stranding it Active with a shipped-out phase.
        finalize_mission_if_complete(mission_id);
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
             darkmux mission ship {mission_id} --phase {} --merge",
            phase.id
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
fn print_review_summary(review: &crate::phase_cli::PhaseReviewOutput) {
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
#[path = "coder_phase_tests.rs"]
mod tests;
