//! Registry-advertised ACP panel commands + ephemeral procedural launches
//! (#1684, Packet 1 of the #1684/#1685 arc).
//!
//! `src/acp.rs`'s spike hardcoded a single `/review` slash command. This
//! module replaces that: it enumerates the SAME merged mission-config
//! registry `darkmux mission launch`/`darkmux mission status` already use
//! (`crew::mission_config::list_ids` + `crew::mission_config::load` — the
//! user tier `~/.darkmux/mission-configs/` over the built-ins), filters to
//! configs that declare a `panel` block ([`crew::mission_config::PanelConfig`]),
//! and decides — per the operator-ratified design — how an invoked command
//! should run:
//!
//! - A config whose graph contains ZERO model-dispatching steps (every
//!   step kind is `procedural.*`) runs EPHEMERAL: [`run_ephemeral`]
//!   interprets the config's graph and drives it through
//!   `darkmux_crew::scheduler::run_step_graph` directly, in-process, with
//!   NO mission instance minted and NO lifecycle records — "instances for
//!   work you'd revisit, flow records for acts you'd audit." Steps still
//!   emit their own flow records through the ordinary flow sink
//!   (`crate::flow::record`), each stamped with a per-invocation
//!   correlation id (see `run_ephemeral`'s doc) so concurrent runs of the
//!   same config don't collide in the viewer.
//! - Anything else (at least one model-seated step) launches as a normal
//!   `darkmux mission launch <id>` subprocess through `acp.rs`'s generic
//!   `run_launch_command`. A panel invocation types no params, so a
//!   config that declares a required `diff_file` input (`review` is the
//!   only shipped one) needs its diff/workspace/`head_sha` params filled
//!   from somewhere else: [`synthesize_diff_launch_inputs`] derives them
//!   from the session's `cwd`. `run_launch_command` calls it directly;
//!   `radio_cli.rs`'s headless launch path calls the same function for
//!   the same reason.
//!
//! **Advertised id vs. document id.** A [`PanelCommand`]'s `id` is always
//! the REGISTRY-RESOLVABLE key (what `list_ids()`/`load()` key on — an
//! on-disk filename stem, effectively), never the JSON body's own
//! `MissionConfig.id` field — those two strings can differ on an
//! operator's hand-edited config, and advertising the wrong one would show
//! Zed a command that can never actually resolve. See [`PanelCommand`]'s
//! own doc.
//!
//! This module owns the REGISTRY ENUMERATION, the ROUTING DECISION, and
//! the EPHEMERAL RUNNER. `acp.rs` owns the ACP wire-protocol plumbing
//! (session/new advertising, session/prompt dispatch, subprocess spawning
//! for the `Launch`/`Review` routes) — split so each stays independently
//! readable.

use crate::crew::mission_config::{self, LaunchParams, MissionConfig};
use crate::crew::scheduler::SchedulerReport;
use crate::crew::step_kinds::{Facts, FixedEstimator, StepKindRegistry};
use crate::crew::types::{NodeStatus, Step, Task};
use anyhow::{Context, Result};
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

/// One entry `darkmux acp` advertises as a slash command in the editor's
/// agent panel.
///
/// `id` is the REGISTRY-RESOLVABLE key — the same string
/// `crew::mission_config::list_ids()` returned it under, and therefore the
/// same string `crew::mission_config::load(id)` resolves — NEVER the
/// document body's own `MissionConfig.id` field (#1684 QA finding: those
/// two can differ when an operator's on-disk filename and the JSON body's
/// `id` field drift, e.g. a copy-pasted config nobody renamed internally;
/// advertising the body id would show a command in Zed that
/// [`route_command`]/`mission_config::load` can never actually resolve).
///
/// `description` is [`PanelConfig::description`] (a short UI label),
/// falling back to `MissionConfig.name` — NEVER `MissionConfig.description`
/// (#1684 QA finding: that field is deliberately long-form developer
/// provenance prose, unsuitable for a command-palette entry; the built-in
/// `review` config's is ~2KB).
#[derive(Debug, Clone)]
pub struct PanelCommand {
    pub id: String,
    pub description: String,
    pub hint: Option<String>,
}

/// Enumerate every mission config in the merged registry (built-ins +
/// `~/.darkmux/mission-configs/`) that declares a `panel` block — the
/// advertising filter. Reuses `crew::mission_config::list_ids` +
/// `crew::mission_config::load` (the SAME resolution `darkmux mission
/// launch`/`darkmux mission status` already use) rather than
/// re-implementing discovery. A config that fails to load (malformed JSON,
/// e.g. from a hand-edited operator override) is skipped with a stderr
/// note — one broken config must never take down the whole command list.
pub fn list_panel_commands() -> Vec<PanelCommand> {
    let mut out = Vec::new();
    for id in mission_config::list_ids() {
        let loaded = match mission_config::load(&id) {
            Ok(l) => l,
            Err(e) => {
                eprintln!(
                    "[darkmux-acp] skipping mission config \"{id}\" while listing panel \
                     commands: {e:#}"
                );
                continue;
            }
        };
        let Some(panel) = &loaded.config.panel else { continue };
        out.push(PanelCommand {
            // The RESOLVABLE key (`id`, from `list_ids()`), never
            // `loaded.config.id` — see `PanelCommand`'s own doc.
            id: id.clone(),
            description: panel
                .description
                .clone()
                .unwrap_or_else(|| loaded.config.name.clone()),
            hint: panel.hint.clone(),
        });
    }
    out.sort_by(|a, b| a.id.cmp(&b.id));
    out
}

/// A human-readable fallback for a prompt that didn't match any currently
/// advertised command — lists the live command set instead of hardcoding
/// `/review` the way the pre-#1684 spike did.
pub fn not_a_command_message(commands: &[PanelCommand]) -> String {
    if commands.is_empty() {
        return "darkmux acp has no commands to advertise right now — no mission config in \
                the merged registry (built-ins + ~/.darkmux/mission-configs/) declares a \
                `panel` block."
            .to_string();
    }
    format!("darkmux acp doesn't recognize that as a command. {}", command_listing(commands))
}

/// The bare "Available commands: …" listing, WITHOUT the didn't-recognize-
/// that preamble (#1698 Packet B2 gate). The answering seat appends a
/// listing after an answer that names a `/command`, where the full
/// `not_a_command_message` would contradict itself: RADIO answers the
/// question, then the appended text tells the operator their message wasn't
/// recognized. Same list, no refusal framing. Empty in ⇒ empty out, so a
/// caller appending this to an answer adds nothing when there is nothing to
/// list (the refusal path's own no-commands explanation stays above).
pub fn command_listing(commands: &[PanelCommand]) -> String {
    if commands.is_empty() {
        return String::new();
    }
    let list = commands.iter().map(|c| format!("`/{}`", c.id)).collect::<Vec<_>>().join(", ");
    format!("Available commands: {list}.")
}

/// Split a raw prompt into `(command name, raw args)` — **the mode bit**
/// (issue #1698, "the slash becomes the mode bit"). Matches ONLY when the
/// first non-whitespace character is a literal `/`; anything else,
/// including empty/whitespace-only text, is `None`.
///
/// **Bare-word invocation is RETIRED (#1698 Packet B, "retired in the same
/// change" as the no-slash interpreted channel).** Before this change, a
/// leading slash was optional (matching the pre-#1684 spike's own
/// `/review`-or-`review` leniency) — harmless while unmatched no-slash text
/// hit a plain help wall, but load-bearing-wrong the instant free text
/// gains meaning: with the no-slash interpreted channel now live
/// (`src/acp.rs`'s `run_no_slash_route`), a bare command name racing
/// against sentence-shaped text would be genuinely ambiguous — is
/// `"review this with me when you have a sec"` a slash-optional command
/// invocation (first token `review` happens to match) or a sentence the
/// interpreted channel should route? **Empirically confirmed live** (this
/// packet's own investigation, piping `darkmux acp` directly): under the
/// PRE-fix parser, that exact sentence matched `review` as a bare command
/// and launched the full review pipeline — the ambiguity this retirement
/// closes is not hypothetical. `/x` is now law (exact, zero
/// interpretation); no slash is always the interpreted channel's to
/// classify, never a pattern match.
///
/// The first whitespace-delimited word after the slash, LOWERCASED
/// (mission-config ids are conventionally lowercase-kebab, and the
/// pre-#1684 spike itself lowercased — `/Review` must still resolve), is
/// the command name; everything after it (trimmed, case PRESERVED) is the
/// raw args string forwarded verbatim to whichever route the command
/// resolves to.
pub fn parse_command(text: &str) -> Option<(String, String)> {
    let trimmed = text.trim();
    let without_slash = trimmed.strip_prefix('/')?;
    if without_slash.is_empty() {
        return None;
    }
    let mut parts = without_slash.splitn(2, char::is_whitespace);
    let name = parts.next()?.to_ascii_lowercase();
    let args = parts.next().unwrap_or("").trim().to_string();
    Some((name, args))
}

/// What `session/prompt`'s command dispatch decided to do with an invoked
/// command name — see [`route_command`].
pub enum RoutePlan {
    /// The config's graph contains ZERO model-dispatching steps (every
    /// step kind is `procedural.*`) — run in-process via [`run_ephemeral`],
    /// no mission instance minted.
    Ephemeral(Box<MissionConfig>),
    /// The config's graph has at least one model-seated step — launch it
    /// as a normal `darkmux mission launch <id>` subprocess (a full
    /// instance), same pattern as `review`'s own subprocess.
    Launch(String),
}

/// Decide how an invoked command name should run. `None` when `cmd` is not
/// one of the CURRENTLY advertised commands — `advertised` is recomputed
/// per prompt by the caller (the registry can change between `session/new`
/// and a later `session/prompt`), never cached across the session's
/// lifetime. A command that WAS advertised but no longer resolves (e.g.
/// its file was deleted between `session/new` and this prompt) also
/// returns `None` here rather than panicking — `advertised` already
/// reflects the current registry, so this is defense in depth, not the
/// primary check.
///
/// Matching is CASE-INSENSITIVE against the advertised registry keys
/// (#1695 merge-gate finding 2) — `cmd` arrives already lowercased from
/// [`parse_command`], but a registry key (an on-disk filename stem) keeps
/// whatever case the operator gave the file, so a naive `==` would make a
/// mixed-case filename permanently unreachable even though it was
/// advertised. `advertised` is caller-sorted ([`list_panel_commands`]),
/// so `.find()`'s first case-insensitive match is deterministic — two
/// configs that collide only after lowercasing resolve to whichever sorts
/// first, never a panic. The MATCHED entry's own (correctly-cased) `id` is
/// what actually gets loaded/launched — never the lowercased `cmd` the
/// user typed.
pub fn route_command(advertised: &[PanelCommand], cmd: &str) -> Option<RoutePlan> {
    let matched = advertised.iter().find(|c| c.id.eq_ignore_ascii_case(cmd))?;
    let resolved_id = matched.id.clone();
    let loaded = mission_config::load(&resolved_id).ok()?;
    if is_procedural_only(&loaded.config) {
        Some(RoutePlan::Ephemeral(Box::new(loaded.config)))
    } else {
        Some(RoutePlan::Launch(resolved_id))
    }
}

/// (#2310 P4d) The inputs a diff-scoped config needs that an invoked
/// COMMAND surface (`/review` in the editor panel, `radio "review this"`)
/// cannot type: the diff itself, and a workspace the planner can read the
/// post-diff tree through.
///
/// **Why this exists.** The bespoke review launcher used to synthesize
/// `diff_file`/`worktree` for the panel before spawning; it retired with
/// the funnel, while `review.json` still declares `diff_file` REQUIRED. A
/// panel invocation therefore has to supply it or the launch bails on a
/// missing input. Deciding that STRUCTURALLY — does this config declare a
/// required `diff_file`? — rather than by matching the id `"review"` keeps
/// a renamed variant working, the same discipline `route_command` follows.
///
/// **What it reviews, stated plainly.** The diff is the branch's COMMITTED
/// work (`git diff <base>..HEAD`, base = the merge-base with the first of
/// `origin/HEAD`/`origin/main`/`origin/master`/local `main`/local
/// `master`/`init.defaultBranch` that resolves, else `HEAD~1` as a last
/// resort — see [`synthesize_diff_launch_inputs`]'s own comment for the
/// full order), NOT the uncommitted working tree the
/// retired launcher used to pass. That is forced by the planner's own
/// contract: `plan.sites`'s diff source reads the post-diff content through
/// a MATERIALIZED tree ("the tree is the confirmation surface"), and a
/// materialized checkout is a clone at a ref — it cannot contain
/// uncommitted work. Reviewing the working tree would hand the rules a diff
/// whose lines do not exist in the tree they are read against. Uncommitted
/// changes are reported as excluded rather than silently reviewed.
pub enum DiffLaunchInputs {
    /// The config declares no required `diff_file` — nothing to synthesize.
    NotNeeded,
    /// A git repo with nothing committed to review against its base.
    Nothing(String),
    /// The `--param key=value` values to append, plus the tempdir holding
    /// the diff + workspace spec.
    Ready(SynthesizedInputs),
}

/// The synthesized `--param` values and the tempdir they live in. The
/// tempdir is removed by [`Drop`] — every exit path, including a `?` out of
/// the caller and a cancelled subprocess, since the guard is dropped with
/// the caller's frame.
pub struct SynthesizedInputs {
    params: Vec<String>,
    dir: std::path::PathBuf,
    /// Non-empty when the working tree carries changes this review does NOT
    /// cover — surfaced by the caller so the operator is never told a
    /// review covered work it could not read.
    pub excluded_note: Option<String>,
}

impl SynthesizedInputs {
    /// The `key=value` strings to pass as `--param` arguments.
    pub fn params(&self) -> &[String] {
        &self.params
    }
}

impl Drop for SynthesizedInputs {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// Run `git` in `cwd`, returning trimmed stdout on success.
fn git_out(cwd: &Path, args: &[&str]) -> Option<String> {
    let out = std::process::Command::new("git").args(args).current_dir(cwd).output().ok()?;
    if !out.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// See [`DiffLaunchInputs`]. `Err` only for genuine IO failures (the repo
/// itself missing, an unwritable temp dir) — an empty diff is a
/// [`DiffLaunchInputs::Nothing`] outcome the caller renders, not an error.
pub fn synthesize_diff_launch_inputs(config: &MissionConfig, cwd: &Path) -> Result<DiffLaunchInputs> {
    let needs_diff = config
        .inputs
        .iter()
        .any(|i| i.name == "diff_file" && i.required.unwrap_or(true));
    if !needs_diff {
        return Ok(DiffLaunchInputs::NotNeeded);
    }
    let head = git_out(cwd, &["rev-parse", "HEAD"]).ok_or_else(|| {
        anyhow::anyhow!(
            "`{}` needs a diff, and {} is not a git repository with a commit to review",
            config.id,
            cwd.display()
        )
    })?;
    // The base to diff against, tried in order, first hit wins: the
    // merge-base with the remote's default branch (`origin/HEAD` covers a
    // clone whose remote HEAD symref is set; `origin/main`/`origin/master`
    // cover the common case where it isn't); then a LOCAL `main`/`master`
    // branch (a repo with no remote at all — the common shape for a
    // freshly-initialized or fully-local project — still finds its own
    // base branch instead of falling straight to "review only the last
    // commit"); then whatever `git config init.defaultBranch` names, if
    // that branch actually exists; and only then the previous commit, so
    // a repo with genuinely no base branch anywhere still reviews
    // something rather than refusing. `fallback_to_last_commit` records
    // whether that last resort fired, so the caller can say so.
    let mut fallback_to_last_commit = false;
    let default_branch_candidate = git_out(cwd, &["config", "init.defaultBranch"]);
    let mut candidates: Vec<String> =
        vec!["origin/HEAD".into(), "origin/main".into(), "origin/master".into(), "main".into(), "master".into()];
    if let Some(b) = &default_branch_candidate {
        if !b.trim().is_empty() {
            candidates.push(b.trim().to_string());
        }
    }
    let base = candidates
        .iter()
        .find_map(|r| git_out(cwd, &["merge-base", "HEAD", r]))
        // Deliberate: when the checkout itself IS one of the base-branch
        // candidates (e.g. running this panel command directly on `main`),
        // `merge-base HEAD <candidate>` returns HEAD's own sha — a
        // "diff against yourself" answer that would otherwise short-circuit
        // to an empty diff below. Rejecting that case here (turning it back
        // to `None`) falls through to the `HEAD~1` last resort instead, so
        // the panel still reviews the most recent commit rather than
        // silently reviewing nothing.
        .filter(|b| b != &head)
        .or_else(|| {
            let h1 = git_out(cwd, &["rev-parse", "HEAD~1"]);
            if h1.is_some() {
                fallback_to_last_commit = true;
            }
            h1
        })
        .unwrap_or_else(|| head.clone());
    let diff = if base == head {
        String::new()
    } else {
        git_out(cwd, &["diff", &format!("{base}..{head}")]).unwrap_or_default()
    };
    if diff.trim().is_empty() {
        return Ok(DiffLaunchInputs::Nothing(format!(
            "Nothing committed to review in {} (no changes between {} and HEAD).",
            cwd.display(),
            &base[..base.len().min(12)]
        )));
    }

    let dir = std::env::temp_dir().join(format!(
        "darkmux-{}-{}-review",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    std::fs::create_dir_all(&dir)
        .with_context(|| format!("creating the synthesized-input dir {}", dir.display()))?;
    // Constructed BEFORE the writes below so an error on either one still
    // drops the guard and removes the directory.
    let mut synthesized = SynthesizedInputs { params: Vec::new(), dir: dir.clone(), excluded_note: None };

    let diff_path = dir.join("review.diff");
    std::fs::write(&diff_path, &diff)
        .with_context(|| format!("writing the synthesized diff {}", diff_path.display()))?;

    // One `path` source at THIS checkout, pinned to the diff's own head —
    // the same `workspace_spec` shape an operator writes by hand, so the
    // planner materializes it through exactly one mechanism.
    let name = cwd.file_name().and_then(|s| s.to_str()).unwrap_or("workspace").to_string();
    let spec_path = dir.join("workspace.json");
    let spec = serde_json::json!({
        "name": name,
        "sources": [{ "id": name, "path": cwd.to_string_lossy(), "ref": head }],
    });
    std::fs::write(&spec_path, serde_json::to_vec_pretty(&spec)?)
        .with_context(|| format!("writing the synthesized workspace spec {}", spec_path.display()))?;

    synthesized.params = vec![
        format!("diff_file={}", diff_path.display()),
        format!("workspace={}", spec_path.display()),
        format!("head_sha={head}"),
    ];
    let mut notes: Vec<String> = Vec::new();
    if fallback_to_last_commit {
        notes.push("no base branch found; reviewing only the last commit.".to_string());
    }
    if let Some(dirty) = git_out(cwd, &["status", "--porcelain"]).filter(|s| !s.trim().is_empty()) {
        let n = dirty.lines().count();
        notes.push(format!(
            "{n} uncommitted change{} in this tree {} NOT part of this review — it reads the \
             committed tree at {}.",
            if n == 1 { "" } else { "s" },
            if n == 1 { "is" } else { "are" },
            &head[..head.len().min(12)]
        ));
    }
    if !notes.is_empty() {
        synthesized.excluded_note = Some(notes.join(" "));
    }
    Ok(DiffLaunchInputs::Ready(synthesized))
}

/// `true` iff the config's graph declares at least one step AND every
/// declared step kind's REGISTRY ID is prefixed `procedural.` — the
/// ephemeral-vs-mission-launch routing test (rule D). This is a
/// declared-KIND test, not a runtime guarantee that zero model work can
/// possibly happen: a `procedural.shell` step could itself invoke
/// `darkmux dispatch` (or any other model-touching command) from inside
/// its shell command. The routing rule governs what this LAUNCHER
/// declares/dispatches directly, matching every Tier 1 builtin's naming
/// convention (`procedural.*` vs `dispatch.*`) and failing safe for any
/// unrecognized Tier 2/3 kind (routes to `Launch`, a full instance, never
/// silently ephemeral). A config with zero steps anywhere (a freeform
/// document — every phase manual, nothing to dispatch) is NOT ephemeral:
/// `mission launch` already handles the freeform mint-and-work-by-hand
/// path correctly, and there is nothing for an in-process runner to
/// execute.
pub fn is_procedural_only(config: &MissionConfig) -> bool {
    let mut saw_step = false;
    for phase in &config.phases {
        for task in &phase.tasks {
            for step in &task.steps {
                saw_step = true;
                if !step.kind.starts_with("procedural.") {
                    return false;
                }
            }
        }
    }
    saw_step
}

/// Run a procedural-only config's graph in-process — no mission instance
/// minted, no lifecycle records (rule D: "instances for work you'd
/// revisit, flow records for acts you'd audit"). Steps still emit their
/// own flow records through the ordinary sink (`crate::flow::record`,
/// the SAME sink `mission launch`/`dispatch` use); only the
/// mission/phase/task/step INSTANCE persistence is skipped — the `persist`
/// callback below is a deliberate no-op, a documented-valid `run_step_graph`
/// caller shape (see that function's own doc on `persist`).
///
/// Returns the TERMINAL step's `output` (rule E) — the sink task (the one
/// no other task's `depends_on`/`reads` names) in DOCUMENT order, its last
/// step. `cwd` (rule C: "the subprocess/execution cwd is the session's cwd
/// — always") is filled into every `procedural.shell` step's config that
/// doesn't already declare its own `cwd`, never overriding a step-authored
/// one.
///
/// Blocking — every call in here (`interpret`, `run_step_graph`,
/// `procedural.shell`'s own `std::process::Command::output()`) is
/// synchronous. `acp.rs`'s caller runs this on a `tokio::task::
/// spawn_blocking` thread rather than the connection's own async task, so
/// it never stalls the ACP event loop.
///
/// `gate` (#1684 Packet 2) is the operator sign-off gate handler for any
/// step in `config`'s graph that declares `"gate": "operator"` (e.g. the
/// `pr-merge`/`pr-approve` example verbs — see `mission_config::StepConfig::
/// gate`). `acp.rs`'s `session/prompt` handler wires the ACP `session/
/// request_permission` handler here (a channel round-trip back to the
/// connection's async task — see `acp.rs`'s own doc on why that shape is
/// required from a `spawn_blocking` thread); `None` (no handler at all)
/// still fails CLOSED rather than silently ungated — a gated step with no
/// handler wired refuses itself via `crew::gate::resolve_gate`'s own
/// `None` fallback.
pub fn run_ephemeral(
    config: &MissionConfig,
    args: &str,
    cwd: &Path,
    gate: Option<&mut crate::crew::gate::GateHandler<'_>>,
) -> Result<EphemeralOutcome> {
    // (#1685) The command allowlist gate — checked FIRST, before
    // validate()/interpret() ever run, so a blocked config never executes a
    // single step (not even a read-only gather step). See
    // `mission_config::check_cmd`'s own doc. Rendered the same way a
    // declined operator sign-off gate renders (a command-failed message,
    // never a hard `Err` across the ACP boundary), so the panel shows the
    // operator exactly why nothing ran.
    if let Some(reason) = mission_config::check_cmd(config) {
        return Ok(EphemeralOutcome { text: format!("darkmux: command failed:\n\n{reason}"), success: false });
    }

    let mut config = config.clone();
    mission_config::inject_panel_args_task_if_referenced(&mut config, args);

    // (#1684 QA finding — CONSIDER 7) `mission launch` runs
    // `MissionConfig::validate` at its consumption point before ever
    // calling `interpret` (contract 7: semantic validation is a separate,
    // explicit pass); the ephemeral path is a consumption point too and
    // gets the SAME gate — a zero-step task, for instance, would otherwise
    // either wedge (never reaches `Complete`, `task_status` never
    // resolves) or surface as an unhelpful "terminal task has no steps"
    // deep inside `render_ephemeral_result`, instead of a clear, named
    // validate()-time error. `known_kinds` is Tier 1 builtins only —
    // sufficient because ephemeral is procedural-only by construction
    // (the routing decision in `is_procedural_only` already excludes
    // anything else), and the reserved `PANEL_ARGS_TASK_ID` reads/depends_on
    // carve-out in `validate` means the just-injected args task never
    // trips a false dangling-reference finding here.
    let known_ids = StepKindRegistry::with_builtins().ids();
    let known_kinds: Vec<&str> = known_ids.iter().map(String::as_str).collect();
    let errors: Vec<_> = config
        .validate(&known_kinds)
        .into_iter()
        .filter(|f| f.severity == mission_config::FindingSeverity::Error)
        .collect();
    if !errors.is_empty() {
        let msg = errors.iter().map(|f| f.to_string()).collect::<Vec<_>>().join("\n");
        anyhow::bail!("panel command config \"{}\" failed validation:\n{msg}", config.id);
    }

    let params = LaunchParams::default();
    let (ordered_tasks, mut steps, interpret_warnings) =
        mission_config::interpret(&config, &params).context("interpreting panel command graph")?;

    apply_default_cwd(&mut steps, cwd);

    let tasks: BTreeMap<String, Task> = ordered_tasks.iter().map(|t| (t.id.clone(), t.clone())).collect();
    let registry = StepKindRegistry::with_builtins();
    let facts = Facts::default();
    let est = FixedEstimator::default();

    // (#1684 QA finding — MUST-FIX 4) A per-INVOCATION correlation id,
    // backfilled onto every emitted flow record exactly the way
    // `mission_launch.rs`'s `run_step_graph` call sites backfill their own
    // minted `mission_id` (`record.mission_id.get_or_insert_with(...)`,
    // never overwriting a record that already carries one — see
    // `step_lifecycle_record`'s own doc in `darkmux-crew`'s scheduler for
    // why the bare records carry NO mission_id and rely on the caller to
    // backfill it). Without this, every ephemeral run of the SAME config
    // shares one CONFIG-scoped `session_id` (`session_id::task` hashes
    // only `step.task_id`) and no `mission_id` at all — two concurrent
    // invocations collide in the viewer with nothing to tell them apart.
    // This id is a FLOW-RECORD correlation label only — no mission
    // instance is minted for it (rule D still holds: nothing under
    // `<mission_id>/` is ever written).
    let correlation_id = mint_ephemeral_correlation_id(&config.id);

    // (#1877 QA must-fix 1) Telemetry + a whole-run dispatch bookend,
    // PRESCRIBED here too — not just for `darkmux mission launch`. Before
    // this, `run_ephemeral` was a second door into `run_step_graph` with
    // neither: the SAME cmd configs `launch()` now covers (`pr-merge`,
    // `pr-approve`) run through here whenever the ACP panel in Zed invokes
    // them, so a panel-launched `pr-merge` that hung had none of the
    // observability its terminal-launched twin gets, and the Runs lens
    // showed nothing for it. This path is Tier-1-only by construction
    // (`is_procedural_only` gates routing here at all — see `route_command`),
    // so no per-step model dispatch ever happens on it; telemetry is the
    // whole value (host cpu/ram/gpu + lms load/unload deltas alongside a
    // `procedural.shell` step that shells out to `gh` and might hang), and
    // the bookend pair is what makes an ephemeral run's liveness visible at
    // all, the same way `mission_bookend_record`'s own doc explains for
    // `launch`. Reuses `mission_launch`'s builders (`mission_bookend_record`,
    // `drained_telemetry`) rather than a second hand-rolled copy of the
    // record shape — same discipline #1685 QA MUST-FIX 2 already applied to
    // `emit_cmd_audit`. No mission instance is minted either way (rule
    // D still holds): `mission_id`/`session_id` on every record here is the
    // per-invocation `correlation_id`, never a real mission id.
    let telemetry = crate::crew::run_obs::HostTelemetrySampler::start(
        correlation_id.clone(),
        config.id.clone(),
        crate::crew::run_obs::DEFAULT_TELEMETRY_INTERVAL,
        crate::crew::run_obs::DEFAULT_TELEMETRY_POLL,
        crate::crew::telemetry_sampler::sample_host,
        darkmux_profiles::lms::list_loaded,
    );
    let mut dispatch_sink = |record: crate::flow::FlowRecord| {
        let _ = crate::flow::record(record);
    };
    let config_id_for_abort = config.id.clone();
    let correlation_id_for_abort = correlation_id.clone();
    // `BookendGuard`'s Drop fires this `on_abort` closure for any exit
    // between `open()` below and a matching `close()` that this function
    // doesn't already reach explicitly — mirrors `launch`'s own bookend;
    // see its doc for why this is the accepted backstop for the
    // unexpected case, not the primary mechanism.
    let mut bookend = crate::flow::BookendGuard::new(&mut dispatch_sink, move |_id, _kind| {
        crate::mission_launch::mission_bookend_record(
            crate::flow::Level::Error,
            "dispatch error",
            &config_id_for_abort,
            &correlation_id_for_abort,
            serde_json::json!({
                "runtime": "ephemeral",
                "result_class": "error",
                "error": "ephemeral panel dispatch terminated before completion (early return or panic)",
            }),
        )
    });
    bookend.open(
        "dispatch",
        "dispatch",
        crate::mission_launch::mission_bookend_record(
            crate::flow::Level::Info,
            "dispatch start",
            &config.id,
            &correlation_id,
            serde_json::json!({ "runtime": "ephemeral" }),
        ),
    );

    // (#1685) Track whether an operator sign-off gate was actually
    // CONFIRMED during this run, for the command-gate audit record emitted
    // below. `Rc<Cell<..>>` (not a plain captured `&mut`) so this closure
    // can wrap the caller's own handler while still letting the outer
    // function read the result AFTER `run_step_graph` returns — the
    // closure's last use is the call below, so the borrow checker is happy,
    // but a Cell keeps the intent obvious. `None` stays `None` for the
    // whole run when no gated step ever runs (a read-only verb like
    // `pr-list`/`pr-info`, or a config with no gate at all) — the audit
    // record then records "no gate" rather than fabricating a yes/no for a
    // decision that never happened.
    let gate_confirmed: std::rc::Rc<std::cell::Cell<Option<bool>>> =
        std::rc::Rc::new(std::cell::Cell::new(None));
    type BoxedGateHandler<'a> = Box<dyn FnMut(&Step, &BTreeMap<String, String>) -> crate::crew::gate::GateDecision + 'a>;
    let mut instrumented_gate: Option<BoxedGateHandler<'_>> = gate.map(|handler| {
        let flag = gate_confirmed.clone();
        Box::new(move |step: &Step, facts: &BTreeMap<String, String>| {
            let decision = handler(step, facts);
            flag.set(Some(matches!(decision, crate::crew::gate::GateDecision::Approved)));
            decision
        }) as BoxedGateHandler<'_>
    });

    let scheduler_result = crate::crew::scheduler::run_step_graph(
        &mut steps,
        &tasks,
        &registry,
        &facts,
        &est,
        1,
        &crate::crew::concurrent_dispatch::lms_host_factory,
        &mut |mut record| {
            record.mission_id.get_or_insert_with(|| correlation_id.clone());
            // (#1877) Drain before emit — same interleaving discipline
            // `launch`'s own emit closure uses, so telemetry streams
            // alongside this run's other records rather than batching at
            // the end (CLAUDE.md's "no blind runs" mandate).
            for sample in crate::mission_launch::drained_telemetry(&telemetry, &correlation_id) {
                let _ = crate::flow::record(sample);
            }
            let _ = crate::flow::record(record);
        },
        &mut |_step: &Step| {
            // Deliberately a no-op — ephemeral runs mint no mission
            // instance, so there is no `<mission>/<phase>/steps/<id>.json`
            // to persist a Step transition into (rule D).
        },
        instrumented_gate.as_deref_mut(),
        None,
        &[],
    );

    // (#1877 QA must-fix 1) Explicit close on every KNOWN exit — a
    // scheduler-level error, a `render_ephemeral_result` error, or the
    // ordinary success finish — same discipline `launch`'s own three
    // explicit-close sites use; the `BookendGuard` Drop backstop above is
    // strictly the fallback for the unexpected case (a panic).
    let report = match scheduler_result {
        Ok(report) => report,
        Err(e) => {
            for sample in crate::mission_launch::drained_telemetry(&telemetry, &correlation_id) {
                bookend.emit_now(sample);
            }
            bookend.close(
                "dispatch",
                crate::mission_launch::mission_bookend_record(
                    crate::flow::Level::Error,
                    "dispatch error",
                    &config.id,
                    &correlation_id,
                    serde_json::json!({
                        "runtime": "ephemeral",
                        "result_class": "error",
                        "error": e.to_string(),
                    }),
                ),
            );
            return Err(e.context("running panel command graph"));
        }
    };

    let outcome = match render_ephemeral_result(&ordered_tasks, &steps, &report, &interpret_warnings) {
        Ok(outcome) => outcome,
        Err(e) => {
            for sample in crate::mission_launch::drained_telemetry(&telemetry, &correlation_id) {
                bookend.emit_now(sample);
            }
            bookend.close(
                "dispatch",
                crate::mission_launch::mission_bookend_record(
                    crate::flow::Level::Error,
                    "dispatch error",
                    &config.id,
                    &correlation_id,
                    serde_json::json!({
                        "runtime": "ephemeral",
                        "result_class": "error",
                        "error": e.to_string(),
                    }),
                ),
            );
            return Err(e);
        }
    };

    for sample in crate::mission_launch::drained_telemetry(&telemetry, &correlation_id) {
        bookend.emit_now(sample);
    }
    bookend.close(
        "dispatch",
        crate::mission_launch::mission_bookend_record(
            if outcome.success { crate::flow::Level::Info } else { crate::flow::Level::Error },
            if outcome.success { "dispatch complete" } else { "dispatch error" },
            &config.id,
            &correlation_id,
            serde_json::json!({
                "runtime": "ephemeral",
                "result_class": if outcome.success { "ok" } else { "error" },
            }),
        ),
    );

    // (#1685) Flow-record audit — ONE record per executed gated command,
    // regardless of outcome: a blocked-by-gate or otherwise-failed attempt
    // is still "the operator's session tried to act as them," an
    // audit-worthy fact on its own (trail 6: "the audit trail of 'when I
    // allowed the tool to act as me' is a compliance artifact in its own
    // right"). Configs with no `cmd` (the ordinary case) never emit
    // this record at all.
    if let Some(verb) = config.cmd.as_deref() {
        emit_cmd_audit(verb, args, cwd, gate_confirmed.get(), outcome.success, &correlation_id);
    }

    Ok(outcome)
}

/// (#1685) Emit the flow-record audit-trail entry for one executed cmd-gate
/// command — see `run_ephemeral`'s own doc for when THIS module fires it.
/// Follows the SAME audit-trail convention `darkmux flow note --source
/// adjudication` uses (`Category::Audit`, a distinguishing `source` string,
/// structured detail in `payload`), never a parallel record channel.
///
/// `pub(crate)` (#1685 QA MUST-FIX 2): `src/mission_launch.rs::launch`'s
/// own `darkmux mission launch <id>` entry point calls this SAME function
/// (via `emit_launch_cmd_audit`'s thin wrapper — see its doc) rather
/// than growing a second, drifting copy of the record shape. Before this,
/// only the ACP ephemeral route emitted `gh.verb.executed` at all: a bare
/// `darkmux mission launch pr-merge` from a terminal executed the merge
/// with ZERO audit trail, contradicting this feature's own docs page
/// ("Every EXECUTED gated command ... emits one flow record") and the
/// PR body that shipped it.
///
/// `pr` is a BEST-EFFORT extraction — the first whitespace-delimited token
/// of the raw args string, verbatim. darkmux core has no notion of what a
/// PR number IS or whether the operator typed one; it only records what
/// was typed after the slash command, exactly as `gh` itself received it.
pub(crate) fn emit_cmd_audit(verb: &str, args: &str, cwd: &Path, gate_confirmed: Option<bool>, success: bool, correlation_id: &str) {
    let pr = args.split_whitespace().next().map(str::to_string);
    let handle = match &pr {
        Some(pr) => format!("{verb} {pr}"),
        None => verb.to_string(),
    };
    let record = crate::flow::FlowRecord {
        ts: crate::flow::ts_utc_now(),
        level: if success { crate::flow::Level::Info } else { crate::flow::Level::Warn },
        category: crate::flow::Category::Audit,
        tier: crate::flow::Tier::Operator,
        stage: crate::flow::Stage::Review,
        action: "gh.verb.executed".to_string(),
        handle,
        phase_id: None,
        session_id: None,
        source: Some("cmd-gate-audit".to_string()),
        model: None,
        reasoning: None,
        mission_id: Some(correlation_id.to_string()),
        machine_id: None,
        machine_uid: None,
        prev_hash: None,
        hash: None,
        payload: Some(serde_json::json!({
            "verb": verb,
            "pr": pr,
            "worktree": cwd.to_string_lossy(),
            "confirmed": gate_confirmed,
            "success": success,
        })),
        work_id: None,
        attempt: None,
    };
    let _ = crate::flow::record(record);
}

/// Mint a per-invocation flow-record correlation id for an ephemeral
/// panel-command run — see `run_ephemeral`'s own doc for why this exists.
/// Shape mirrors `mission_launch::mint_run_id`'s spirit
/// (`<config-id>-<unix-nanos>-<counter>`) without pulling in `blake3` here
/// (`acp_panel.rs` has no other cryptographic need); a nanosecond
/// timestamp plus an in-process atomic counter is already collision-free
/// for concurrent ACP prompts within one process.
fn mint_ephemeral_correlation_id(config_id: &str) -> String {
    static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    format!("acp-ephemeral-{config_id}-{nanos}-{n}")
}

/// Fill `cwd` into every `procedural.shell` step's config that doesn't
/// already declare one — never overrides a step-authored `cwd`.
fn apply_default_cwd(steps: &mut BTreeMap<String, Step>, cwd: &Path) {
    let cwd_str = cwd.to_string_lossy().to_string();
    for step in steps.values_mut() {
        if step.kind != "procedural.shell" {
            continue;
        }
        if step.config.get("cwd").and_then(|v| v.as_str()).is_some() {
            continue;
        }
        match &mut step.config {
            serde_json::Value::Object(map) => {
                map.insert("cwd".to_string(), serde_json::Value::String(cwd_str.clone()));
            }
            serde_json::Value::Null => {
                let mut map = serde_json::Map::new();
                map.insert("cwd".to_string(), serde_json::Value::String(cwd_str.clone()));
                step.config = serde_json::Value::Object(map);
            }
            _ => {}
        }
    }
}

/// [`run_ephemeral`]'s result: the rendered TEXT (identical rendering to
/// the pre-#1698-Packet-B `Ok(String)` contract — every caller that only
/// ever displayed the string, `src/acp.rs`'s ACP panel path, keeps
/// byte-identical output) PLUS a real typed success/failure signal.
///
/// **Retires the failure-prefix string-sniffing** (#1698 Packet B carry-
/// list item 5) that used to be the only way a caller with a real exit
/// code to decide (`src/radio_cli.rs`) could tell success from failure:
/// `EPHEMERAL_FAILURE_PREFIX`/`EPHEMERAL_INCOMPLETE_PREFIX`/
/// `ephemeral_output_is_failure` are gone — `success` is now computed
/// directly from the scheduler's own typed state in
/// [`render_ephemeral_result`], not re-derived by matching a literal
/// prefix on rendered prose.
///
/// **Fixes the documented "Known gap" as a direct consequence, not a
/// separate follow-up:** the terminal step can `Complete` cleanly while a
/// SIDE branch elsewhere in the graph errored (`report.errored` non-empty)
/// — the string-sniffing contract COULD NOT distinguish that case from a
/// genuine clean success (the warning was appended to the SAME
/// success-shaped text, no distinct prefix), so a caller with a real exit
/// code silently exited 0 for a partially-failed run. With a real typed
/// field, [`render_ephemeral_result`] now sets `success: false` for that
/// case directly — see its own doc.
#[derive(Debug)]
pub struct EphemeralOutcome {
    pub text: String,
    pub success: bool,
}

/// The sink task in DOCUMENT order — a task no other task's `depends_on`
/// OR `reads` names — and its last step's output, rendered as the final
/// panel message. `ordered_tasks` MUST be the original `Vec<Task>`
/// `interpret` returned (document order); a `BTreeMap`'s key order is
/// lexicographic by id, not document order, so it cannot substitute here.
///
/// `interpret_warnings` (#1695 merge-gate finding 3) are `interpret`'s own
/// non-fatal findings (e.g. an absent `expand.over` collection under a
/// pre-2.0 document — see `InterpretedGraph`'s doc) — `run_ephemeral`
/// previously bound and dropped them; every other production caller
/// (`mission_launch.rs`) prints them, so silently discarding them here was
/// the one place in the codebase where they went nowhere. These are
/// informational only (never flip `success` to `false` on their own — an
/// absent `expand.over` collection isn't a run failure) — same for the
/// multi-sink notice below.
///
/// `success` (#1698 Packet B carry-list item 5): `false` for a terminal
/// step that errored OR never completed, AND (the fixed "Known gap") for a
/// terminal step that completed cleanly while `report.errored` names a
/// SIDE branch that failed elsewhere in the graph — `report.errored` is
/// scheduler-wide, not scoped to the chain that fed the terminal step, so
/// a clean-looking terminal output over a partially-failed run is exactly
/// the "silence reads as success" failure this project's own doctrine
/// (CLAUDE.md's "no blind runs") warns against.
fn render_ephemeral_result(
    ordered_tasks: &[Task],
    steps: &BTreeMap<String, Step>,
    report: &SchedulerReport,
    interpret_warnings: &[String],
) -> Result<EphemeralOutcome> {
    let referenced: BTreeSet<&str> = ordered_tasks
        .iter()
        .flat_map(|t| t.depends_on.iter().chain(t.reads.iter()))
        .map(|s| s.as_str())
        .collect();
    // (#1684 QA finding — CONSIDER 6) Every SINK task (no other task's
    // `depends_on`/`reads` names it) in document order, not just the last
    // one — a fan-out graph with more than one sink loses every branch but
    // the last silently otherwise. The LAST sink in document order is
    // still what becomes the "final message" (rule E is singular by
    // design), but the others are named in a warning rather than dropped
    // with no trace.
    let sinks: Vec<&Task> = ordered_tasks.iter().filter(|t| !referenced.contains(t.id.as_str())).collect();
    let sink = sinks
        .last()
        .ok_or_else(|| anyhow::anyhow!("panel command graph has no terminal (sink) task"))?;
    let last_step_id = sink
        .step_ids
        .last()
        .ok_or_else(|| anyhow::anyhow!("panel command graph's terminal task `{}` has no steps", sink.id))?;
    let terminal = steps
        .get(last_step_id)
        .ok_or_else(|| anyhow::anyhow!("panel command graph's terminal step `{last_step_id}` vanished"))?;

    let output = terminal.output.clone().unwrap_or_default();
    if terminal.status == NodeStatus::Error {
        return Ok(EphemeralOutcome {
            text: format!("darkmux: command failed:\n\n{output}"),
            success: false,
        });
    }
    if terminal.status != NodeStatus::Complete {
        // The terminal task never completed — its dependency chain
        // stranded on an earlier error (`step_is_ready` never schedules a
        // task whose dependency didn't reach Complete). Name what errored
        // rather than returning an empty/misleading message.
        return Ok(EphemeralOutcome {
            text: format!(
                "darkmux: command did not reach its final step (status: {:?}) — step(s) errored: {}",
                terminal.status,
                if report.errored.is_empty() { "(none recorded)".to_string() } else { report.errored.join(", ") }
            ),
            success: false,
        });
    }

    // (#1684 QA finding — CONSIDER 6, fixed by #1698 Packet B carry-list
    // item 5) The terminal step can `Complete` cleanly while a SIDE branch
    // (a non-terminal sink, or any other step off the terminal's own
    // dependency chain) errored — `report.errored` is scheduler-wide, not
    // scoped to the chain that fed the terminal step. A clean-looking
    // final message over a partially-failed run is exactly the
    // "silence reads as success" failure CLAUDE.md's "no blind runs"
    // warns against; name it in the text AND flip `success` to `false` —
    // no longer just a warning appended to a success-shaped string.
    let mut warnings: Vec<String> = interpret_warnings.to_vec();
    let mut success = true;
    if !report.errored.is_empty() {
        warnings.push(format!("step(s) errored elsewhere in the graph: {}", report.errored.join(", ")));
        success = false;
    }
    if sinks.len() > 1 {
        let other_ids: Vec<&str> = sinks[..sinks.len() - 1].iter().map(|t| t.id.as_str()).collect();
        warnings.push(format!(
            "graph has {} terminal branches; only `{}`'s output is shown (also ran: {})",
            sinks.len(),
            sink.id,
            other_ids.join(", ")
        ));
    }
    if warnings.is_empty() {
        return Ok(EphemeralOutcome { text: output, success });
    }
    Ok(EphemeralOutcome {
        text: format!("{output}\n\n---\n⚠ {}", warnings.join("\n⚠ ")),
        success,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crew::mission_config::{PanelConfig, PhaseConfig, StepConfig, TaskConfig, PANEL_ARGS_TASK_ID};
    use std::collections::BTreeMap as Map;

    fn step(id: &str, kind: &str, config: serde_json::Value) -> StepConfig {
        StepConfig { id: id.to_string(), kind: kind.to_string(), config, gate: None, enabled: None, extras: Map::new() }
    }

    fn gated_step(id: &str, kind: &str, config: serde_json::Value, gate: &str) -> StepConfig {
        StepConfig { gate: Some(gate.to_string()), ..step(id, kind, config) }
    }

    fn task(id: &str, depends_on: &[&str], reads: &[&str], steps: Vec<StepConfig>) -> TaskConfig {
        TaskConfig {
            enabled: None,
            excludes: Vec::new(),
            id: id.to_string(),
            description: None,
            display_name: None,
            depends_on: depends_on.iter().map(|s| s.to_string()).collect(),
            reads: reads.iter().map(|s| s.to_string()).collect(),
            role_id: None,
            run_on: None,
            steps,
            grow: None,
            extras: Map::new(),
        }
    }

    fn phase(id: &str, tasks: Vec<TaskConfig>) -> PhaseConfig {
        PhaseConfig { id: id.to_string(), description: None, display_name: None, tasks, enabled: None, extras: Map::new() }
    }

    fn config(id: &str, panel: Option<PanelConfig>, phases: Vec<PhaseConfig>) -> MissionConfig {
        MissionConfig {
            id: id.to_string(),
            name: id.to_string(),
            description: None,
            schema_version: None,
            inputs: Vec::new(),
            phases,
            panel,
            cmd: None,
            outcome_from: None,
            extras: Map::new(),
        }
    }

    // ── parse_command ────────────────────────────────────────────────

    #[test]
    fn parse_command_strips_slash_and_splits_name_from_args() {
        assert_eq!(parse_command("/pr-view 42"), Some(("pr-view".to_string(), "42".to_string())));
        assert_eq!(parse_command("/review"), Some(("review".to_string(), String::new())));
        assert_eq!(parse_command("   "), None);
        assert_eq!(parse_command(""), None);
    }

    /// (#1698 Packet B — the mode bit) The retirement itself: a bare
    /// command name with NO leading slash must never match, even when it
    /// spells an advertised command id exactly, and even when it's the
    /// first word of an otherwise ordinary sentence — the live-repro'd
    /// ambiguity ("review this with me when you have a sec" routing as a
    /// bare `review` invocation) this packet's own investigation confirmed
    /// against the PRE-fix parser. Paired with the test above (a leading
    /// slash still resolves) so the fix can't regress into "nothing
    /// matches anymore" either.
    #[test]
    fn parse_command_retires_bare_word_invocation() {
        assert_eq!(parse_command("pr-view 42"), None);
        assert_eq!(parse_command("review"), None);
        assert_eq!(
            parse_command("review this with me when you have a sec"),
            None,
            "the exact sentence this retirement's own live investigation confirmed \
             mis-fired under the pre-fix slash-optional parser"
        );
    }

    // ── is_procedural_only (rule D routing test) ────────────────────────

    #[test]
    fn a_config_with_only_procedural_steps_is_ephemeral() {
        let cfg = config(
            "echo-test",
            None,
            vec![phase(
                "p1",
                vec![task("t1", &[], &[], vec![step("s1", "procedural.shell", serde_json::json!({"command": "echo hi"}))])],
            )],
        );
        assert!(is_procedural_only(&cfg));
    }

    #[test]
    fn a_config_with_a_dispatch_step_is_not_ephemeral() {
        // (Required test) A `dispatch.*` step must route AWAY from the
        // ephemeral path — this only asserts the ROUTING decision, never
        // actually dispatches a model.
        let cfg = config(
            "coder-verb",
            None,
            vec![phase(
                "p1",
                vec![
                    task("t1", &[], &[], vec![step("s1", "procedural.shell", serde_json::json!({"command": "echo hi"}))]),
                    task("t2", &["t1"], &[], vec![step("s2", "dispatch.internal", serde_json::Value::Null)]),
                ],
            )],
        );
        assert!(!is_procedural_only(&cfg), "a graph with any dispatch.* step must not be ephemeral");
    }

    #[test]
    fn a_config_with_zero_steps_is_not_ephemeral() {
        // A freeform config (every phase manual) — nothing for an
        // in-process runner to execute; `mission launch` handles this path.
        let cfg = config("freeform", None, vec![phase("p1", vec![])]);
        assert!(!is_procedural_only(&cfg));
    }

    // ── route_command ────────────────────────────────────────────────

    #[test]
    fn route_command_returns_none_for_an_unadvertised_command() {
        let advertised = vec![PanelCommand { id: "review".to_string(), description: "d".to_string(), hint: None }];
        assert!(route_command(&advertised, "not-advertised").is_none());
    }

    /// (#1695 merge-gate finding 2) A mixed-case on-disk config filename
    /// advertises under its own (correctly-cased) id, but a user typing
    /// the command lowercases it (`parse_command`'s own normalization) —
    /// `route_command` must still resolve it, and must resolve/launch
    /// using the ORIGINAL-CASED registry key, never the lowercased text
    /// the user typed (a case-sensitive filesystem would 404 on that).
    #[test]
    #[serial_test::serial]
    fn route_command_matches_case_insensitively_and_launches_the_correctly_cased_id() {
        let tmp = tempfile::TempDir::new().unwrap();
        let prev = std::env::var("DARKMUX_CREW_DIR").ok();
        // SAFETY: this test is #[serial_test::serial].
        unsafe { std::env::set_var("DARKMUX_CREW_DIR", tmp.path()) };

        let dir = tmp.path().join("mission-configs");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("Pr-View.json"),
            serde_json::to_string(&serde_json::json!({
                "id": "Pr-View",
                "name": "PR View",
                "panel": {"description": "View a PR"},
                "phases": [{
                    "id": "p1",
                    "tasks": [{"id": "t1", "steps": [{"id": "s1", "kind": "procedural.shell", "config": {"command": "echo hi"}}]}]
                }]
            }))
            .unwrap(),
        )
        .unwrap();

        // The advertised entry keeps the file's own case; the user typed
        // "/Pr-View", which `parse_command` lowercases to "pr-view" before
        // it ever reaches `route_command`.
        let advertised =
            vec![PanelCommand { id: "Pr-View".to_string(), description: "View a PR".to_string(), hint: None }];
        let plan = route_command(&advertised, "pr-view").expect("a mixed-case filename must still be invocable");
        match plan {
            RoutePlan::Ephemeral(config) => {
                assert_eq!(config.id, "Pr-View", "the loaded config is the correctly-cased file's own document");
            }
            _ => panic!("expected an Ephemeral route for a procedural-only fixture"),
        }

        // SAFETY: this test is #[serial_test::serial].
        unsafe {
            match prev {
                Some(v) => std::env::set_var("DARKMUX_CREW_DIR", v),
                None => std::env::remove_var("DARKMUX_CREW_DIR"),
            }
        }
    }

    // ── (#1684 QA finding — MUST-FIX 2/3) advertised id + description ──

    #[test]
    #[serial_test::serial]
    fn list_panel_commands_advertises_the_resolvable_filename_not_the_document_body_id() {
        let tmp = tempfile::TempDir::new().unwrap();
        let prev = std::env::var("DARKMUX_CREW_DIR").ok();
        // SAFETY: this test is #[serial_test::serial].
        unsafe { std::env::set_var("DARKMUX_CREW_DIR", tmp.path()) };

        let dir = tmp.path().join("mission-configs");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("pr-view.json"),
            serde_json::to_string(&serde_json::json!({
                // Deliberately DIFFERENT from the filename stem — the
                // exact drift #1684 QA finding 2 caught: a config file
                // whose on-disk name and JSON body `id` don't match.
                "id": "pr_view",
                "name": "PR View — a very long developer-facing name nobody wants in a menu",
                "description": "a 2000-character developer provenance essay stands in here in the real bug",
                "panel": {"description": "View a PR"},
                "phases": []
            }))
            .unwrap(),
        )
        .unwrap();

        let commands = list_panel_commands();
        let found = commands
            .iter()
            .find(|c| c.description == "View a PR")
            .expect("the injected config must be advertised (short panel.description, not the long one)");
        assert_eq!(
            found.id, "pr-view",
            "must advertise the RESOLVABLE filename stem, not the document body's mismatched `id` field"
        );
        // And what got advertised must actually be loadable under that id
        // — the whole point of fixing MUST-FIX 2.
        assert!(mission_config::load(&found.id).is_ok(), "the advertised id must resolve via mission_config::load");

        // SAFETY: this test is #[serial_test::serial].
        unsafe {
            match prev {
                Some(v) => std::env::set_var("DARKMUX_CREW_DIR", v),
                None => std::env::remove_var("DARKMUX_CREW_DIR"),
            }
        }
    }

    // ── not_a_command_message ────────────────────────────────────────

    #[test]
    fn not_a_command_message_lists_the_advertised_commands() {
        let advertised = vec![
            PanelCommand { id: "review".to_string(), description: "d".to_string(), hint: None },
            PanelCommand { id: "pr-list".to_string(), description: "d2".to_string(), hint: None },
        ];
        let msg = not_a_command_message(&advertised);
        assert!(msg.contains("/review"), "{msg}");
        assert!(msg.contains("/pr-list"), "{msg}");
    }

    #[test]
    fn not_a_command_message_handles_an_empty_advertised_list() {
        let msg = not_a_command_message(&[]);
        assert!(!msg.is_empty());
        assert!(!msg.contains("/review"));
    }

    // ── run_ephemeral (required test: two-step procedural.shell chain) ──

    #[serial_test::serial]

    #[test]
    fn ephemeral_run_chains_two_procedural_shell_steps_via_step_input_env_var() {
        let cfg = config(
            "echo-chain",
            None,
            vec![phase(
                "p1",
                vec![
                    task(
                        "producer",
                        &[],
                        &[],
                        vec![step("producer-step", "procedural.shell", serde_json::json!({"command": "echo hello-from-producer"}))],
                    ),
                    task(
                        "consumer",
                        &["producer"],
                        &[],
                        vec![step(
                            "consumer-step",
                            "procedural.shell",
                            serde_json::json!({"command": "echo got: $DARKMUX_STEP_INPUT_PRODUCER"}),
                        )],
                    ),
                ],
            )],
        );
        let tmp = std::env::temp_dir();
        let out = run_ephemeral(&cfg, "", &tmp, None).expect("ephemeral run succeeds");
        assert_eq!(out.text.trim(), "got: hello-from-producer");
        assert!(out.success);
    }

    #[test]
    #[serial_test::serial]
    fn ephemeral_run_never_mints_a_mission_instance_directory() {
        // (Required test) Assert NO mission instance directory was
        // created — isolate DARKMUX_CREW_DIR so this test can inspect the
        // (empty) missions dir without racing any other test's real state.
        let tmp = tempfile::TempDir::new().unwrap();
        let prev = std::env::var("DARKMUX_CREW_DIR").ok();
        // SAFETY: this test is #[serial_test::serial].
        unsafe { std::env::set_var("DARKMUX_CREW_DIR", tmp.path()) };

        let cfg = config(
            "noop-test",
            None,
            vec![phase("p1", vec![task("t1", &[], &[], vec![step("s1", "procedural.noop", serde_json::Value::Null)])])],
        );
        let cwd = std::env::temp_dir();
        let out = run_ephemeral(&cfg, "", &cwd, None).expect("ephemeral run succeeds");
        // `procedural.noop` with no `output` override defaults to its own
        // step id (see `ProceduralNoopStepKind::run`) — proves the run
        // actually executed, not just that no directory appeared.
        assert_eq!(out.text, "s1");
        assert!(out.success);

        let missions_dir = crate::crew::loader::missions_dir();
        let entries: Vec<_> = std::fs::read_dir(&missions_dir).map(|d| d.collect()).unwrap_or_default();
        assert!(entries.is_empty(), "ephemeral run must mint zero mission instance directories, found {entries:?}");

        // SAFETY: this test is #[serial_test::serial].
        unsafe {
            match prev {
                Some(v) => std::env::set_var("DARKMUX_CREW_DIR", v),
                None => std::env::remove_var("DARKMUX_CREW_DIR"),
            }
        }
    }

    // ── #1877 QA must-fix 1 — `run_ephemeral` gets telemetry + the same ──
    // whole-run `dispatch *` bookend pair `mission_launch::launch` gives
    // every OTHER config. Before this, a panel-launched `pr-merge` had
    // neither: identical steps, identical `cmd`, zero observability.
    //
    // RED PROVED: against the pre-fix `run_ephemeral` (no telemetry/bookend
    // construction at all), `read_all_flow_records()` in both tests below
    // returned only the step's own `step result` record — no `source:
    // "mission"` record of any kind, so the `starts`/`terminals` filters
    // found nothing and both `assert_eq!(..., 1, ...)` calls failed on `0`.

    #[test]
    #[serial_test::serial]
    fn run_ephemeral_emits_a_mission_source_dispatch_bookend_pair_on_success() {
        let tmp_flows = tempfile::TempDir::new().unwrap();
        let prev_flows = std::env::var("DARKMUX_FLOWS_DIR").ok();
        unsafe { std::env::set_var("DARKMUX_FLOWS_DIR", tmp_flows.path()) };

        let cfg = config(
            "bookend-noop-test",
            None,
            vec![phase("p1", vec![task("t1", &[], &[], vec![step("s1", "procedural.noop", serde_json::Value::Null)])])],
        );
        let cwd = std::env::temp_dir();
        let out = run_ephemeral(&cfg, "", &cwd, None).expect("ephemeral run succeeds");
        assert!(out.success);

        let records = read_all_flow_records();
        let mission_records: Vec<&serde_json::Value> =
            records.iter().filter(|r| r["source"] == "mission").collect();
        let starts: Vec<&&serde_json::Value> =
            mission_records.iter().filter(|r| r["action"] == "dispatch start").collect();
        let completes: Vec<&&serde_json::Value> =
            mission_records.iter().filter(|r| r["action"] == "dispatch complete").collect();
        assert_eq!(
            starts.len(),
            1,
            "an ephemeral run must open exactly one mission-level bookend, matching launch()'s \
             own pair: {mission_records:#?}"
        );
        assert_eq!(
            completes.len(),
            1,
            "a successful ephemeral run must close as `dispatch complete`: {mission_records:#?}"
        );
        assert_eq!(starts[0]["handle"], serde_json::json!("bookend-noop-test"));
        let mission_id = starts[0]["mission_id"].as_str().expect("mission_id present").to_string();
        assert!(
            mission_id.starts_with("acp-ephemeral-bookend-noop-test-"),
            "the bookend's mission_id must be the minted per-invocation correlation id: {mission_id}"
        );
        assert_eq!(starts[0]["session_id"], serde_json::json!(mission_id));
        assert_eq!(completes[0]["mission_id"], serde_json::json!(mission_id));
        assert_eq!(completes[0]["payload"]["gate"], serde_json::Value::Null, "no coder-phase gate on this path");

        unsafe {
            match prev_flows {
                Some(v) => std::env::set_var("DARKMUX_FLOWS_DIR", v),
                None => std::env::remove_var("DARKMUX_FLOWS_DIR"),
            }
        }
    }

    #[test]
    #[serial_test::serial]
    fn run_ephemeral_closes_the_bookend_as_dispatch_error_when_the_gate_declines() {
        let tmp_flows = tempfile::TempDir::new().unwrap();
        let prev_flows = std::env::var("DARKMUX_FLOWS_DIR").ok();
        unsafe { std::env::set_var("DARKMUX_FLOWS_DIR", tmp_flows.path()) };

        let cfg = gather_then_gated_config();
        let tmp = std::env::temp_dir();
        let mut decline = |_s: &Step, _f: &Map<String, String>| crate::crew::gate::GateDecision::Declined {
            reason: "operator declined".to_string(),
        };
        let out = run_ephemeral(&cfg, "", &tmp, Some(&mut decline))
            .expect("a declined gate still renders a command-failed message, not an Err");
        assert!(!out.success);

        let records = read_all_flow_records();
        let mission_records: Vec<&serde_json::Value> =
            records.iter().filter(|r| r["source"] == "mission").collect();
        let starts: Vec<&&serde_json::Value> =
            mission_records.iter().filter(|r| r["action"] == "dispatch start").collect();
        let errors: Vec<&&serde_json::Value> =
            mission_records.iter().filter(|r| r["action"] == "dispatch error").collect();
        assert_eq!(starts.len(), 1, "{mission_records:#?}");
        assert_eq!(
            errors.len(),
            1,
            "a declined gate is a command FAILURE — the bookend must close as `dispatch error`: \
             {mission_records:#?}"
        );

        unsafe {
            match prev_flows {
                Some(v) => std::env::set_var("DARKMUX_FLOWS_DIR", v),
                None => std::env::remove_var("DARKMUX_FLOWS_DIR"),
            }
        }
    }

    #[serial_test::serial]

    #[test]
    fn ephemeral_run_seeds_args_when_a_task_reads_the_synthetic_args_task() {
        // `procedural.shell`'s `sanitize_env_key` uppercases alnum bytes
        // and maps everything else (including `_`) to `_`, so the reserved
        // id `__panel_args__` becomes the env var name below verbatim
        // (case-folded).
        let cfg = config(
            "args-echo",
            None,
            vec![phase(
                "p1",
                vec![task(
                    "t1",
                    &[],
                    &[PANEL_ARGS_TASK_ID],
                    vec![step(
                        "s1",
                        "procedural.shell",
                        serde_json::json!({"command": "echo arg: $DARKMUX_STEP_INPUT___PANEL_ARGS__"}),
                    )],
                )],
            )],
        );
        let tmp = std::env::temp_dir();
        let out = run_ephemeral(&cfg, "hello world", &tmp, None).expect("ephemeral run succeeds");
        assert_eq!(out.text.trim(), "arg: hello world");
    }

    #[serial_test::serial]

    #[test]
    fn ephemeral_run_with_empty_args_still_resolves_a_task_that_reads_the_reserved_id() {
        // (#1684 QA context) The reserved id is injected with an EMPTY
        // string when the command was invoked with no arguments — the
        // config must still interpret/run cleanly, not dangle.
        let cfg = config(
            "args-echo-empty",
            None,
            vec![phase(
                "p1",
                vec![task(
                    "t1",
                    &[],
                    &[PANEL_ARGS_TASK_ID],
                    vec![step(
                        "s1",
                        "procedural.shell",
                        serde_json::json!({"command": "echo arg:[$DARKMUX_STEP_INPUT___PANEL_ARGS__]"}),
                    )],
                )],
            )],
        );
        let tmp = std::env::temp_dir();
        let out = run_ephemeral(&cfg, "", &tmp, None).expect("ephemeral run succeeds");
        assert_eq!(out.text.trim(), "arg:[]");
    }

    // ── (#1695 merge-gate finding 1) reserved-id collision ──────────────

    #[serial_test::serial]

    #[test]
    fn ephemeral_run_skips_injection_when_the_document_already_declares_the_reserved_task_id() {
        // The document itself owns a task literally named "__panel_args__"
        // — injection must be skipped (never double-inject, which would
        // collide at interpret()'s own duplicate-id check), and the
        // reading task must receive the DOCUMENT's own task's real
        // output, never the synthetic args string.
        let cfg = config(
            "reserved-collision",
            None,
            vec![phase(
                "p1",
                vec![
                    task(
                        PANEL_ARGS_TASK_ID,
                        &[],
                        &[],
                        vec![step(
                            "producer-step",
                            "procedural.noop",
                            serde_json::json!({"output": "operator-owned-value"}),
                        )],
                    ),
                    task(
                        "consumer",
                        &[],
                        &[PANEL_ARGS_TASK_ID],
                        vec![step(
                            "consumer-step",
                            "procedural.shell",
                            serde_json::json!({"command": "echo got: $DARKMUX_STEP_INPUT___PANEL_ARGS__"}),
                        )],
                    ),
                ],
            )],
        );
        let tmp = std::env::temp_dir();
        // A NON-EMPTY args string — if injection had run anyway (ignoring
        // the collision), the reading task would see THIS value instead
        // of the document's own task output.
        let out = run_ephemeral(&cfg, "this-should-be-ignored", &tmp, None).expect("ephemeral run succeeds, no duplicate-id bail");
        assert_eq!(out.text.trim(), "got: operator-owned-value");
    }

    // ── (#1684 QA finding — CONSIDER 7) validate() runs before interpret ──

    #[test]
    fn ephemeral_run_rejects_a_zero_step_task_at_validate_time_not_a_confusing_runtime_error() {
        let cfg = config(
            "hollow",
            None,
            vec![phase(
                "p1",
                vec![
                    task("t1", &[], &[], vec![]), // zero steps — a real MissionConfig::validate() Error
                ],
            )],
        );
        let tmp = std::env::temp_dir();
        let err = run_ephemeral(&cfg, "", &tmp, None).expect_err("a zero-step task must fail validate(), not run");
        assert!(err.to_string().contains("failed validation"), "{err:#}");
    }

    // ── (#1695 merge-gate finding 3) interpret() warnings surfaced ──────

    #[test]
    fn render_ephemeral_result_appends_interpret_warnings_to_the_output() {
        // `run_ephemeral` used to bind and drop `interpret`'s own non-fatal
        // warnings; `render_ephemeral_result` must surface them in the
        // final message the same way it already surfaces scheduler-level
        // findings (errored steps, multi-sink branches) — a direct unit
        // test since `interpret()` itself has no live producer of a
        // non-empty warnings Vec today (see `InterpretedGraph`'s doc).
        let t = Task {
            run_on: darkmux_crew::types::default_run_on(),
            id: "t1".to_string(),
            phase_id: "p1".to_string(),
            description: String::new(),
            display_name: None,
            step_ids: vec!["s1".to_string()],
            depends_on: Vec::new(),
            reads: Vec::new(),
            role_id: None,
            profile_name: None,
            workdir: None,
            image: None,
        };
        let mut steps = BTreeMap::new();
        steps.insert(
            "s1".to_string(),
            Step {
                id: "s1".to_string(),
                task_id: "t1".to_string(),
                gate: None,
                kind: "procedural.noop".to_string(),
                status: NodeStatus::Complete,
                config: serde_json::Value::Null,
                started_ts: None,
                completed_ts: None,
                output: Some("done".to_string()),
            },
        );
        let report = SchedulerReport::default();
        let interpret_warnings = vec!["absent expand.over collection for task foo".to_string()];

        let out = render_ephemeral_result(&[t], &steps, &report, &interpret_warnings).expect("renders");
        assert!(out.text.contains("done"), "{}", out.text);
        assert!(out.text.contains("absent expand.over collection for task foo"), "{}", out.text);
        // Informational only (an absent expand.over collection isn't a run
        // failure) — `success` stays true (#1698 Packet B carry-list item 5).
        assert!(out.success);
    }

    /// (#1698 Packet B carry-list item 5 — the "Known gap" fix) A terminal
    /// step that `Complete`s cleanly while `report.errored` names a SIDE
    /// branch that failed elsewhere in the graph must report `success:
    /// false`, not just append a warning to a success-shaped text. Before
    /// the typed `EphemeralOutcome`, the string-sniffing contract
    /// (`ephemeral_output_is_failure`) could not distinguish this case from
    /// a genuine clean success — this is the direct regression test for
    /// that fix, paired with the inverted case above (no errored steps ->
    /// `success: true`) so a change that makes EVERY run `false` can't
    /// silently pass either.
    #[test]
    fn render_ephemeral_result_flips_success_false_when_a_side_branch_errored() {
        let t = Task {
            run_on: darkmux_crew::types::default_run_on(),
            id: "t1".to_string(),
            phase_id: "p1".to_string(),
            description: String::new(),
            display_name: None,
            step_ids: vec!["s1".to_string()],
            depends_on: Vec::new(),
            reads: Vec::new(),
            role_id: None,
            profile_name: None,
            workdir: None,
            image: None,
        };
        let mut steps = BTreeMap::new();
        steps.insert(
            "s1".to_string(),
            Step {
                id: "s1".to_string(),
                task_id: "t1".to_string(),
                gate: None,
                kind: "procedural.noop".to_string(),
                status: NodeStatus::Complete,
                config: serde_json::Value::Null,
                started_ts: None,
                completed_ts: None,
                output: Some("terminal step's own clean output".to_string()),
            },
        );
        let report = SchedulerReport {
            errored: vec!["side-branch-step".to_string()],
            ..SchedulerReport::default()
        };

        let out = render_ephemeral_result(&[t], &steps, &report, &[]).expect("renders");
        assert!(
            !out.success,
            "a Complete terminal step alongside an errored side branch must not report success"
        );
        assert!(out.text.contains("terminal step's own clean output"), "{}", out.text);
        assert!(out.text.contains("side-branch-step"), "{}", out.text);
    }

    // ── (#1684 Packet 2) run_ephemeral + operator sign-off gate ────────

    /// A two-step procedural config — a `gather` task feeding a gated
    /// `executor` task — is the shape every documented gated panel verb
    /// (`pr-merge`, `pr-approve`) actually has: a gather step assembles the
    /// facts, the gated step is the consequential action. This test
    /// exercises BOTH decisions through the real `run_ephemeral` path (not
    /// just `gate::resolve_gate`'s own unit tests) and asserts the
    /// handler's facts map is literally the gather task's output — the
    /// dialog-body contract the #1685 spec depends on.
    fn gather_then_gated_config() -> MissionConfig {
        config(
            "gather-then-gated",
            None,
            vec![phase(
                "p1",
                vec![
                    task(
                        "gather",
                        &[],
                        &[],
                        vec![step(
                            "gather-step",
                            "procedural.shell",
                            serde_json::json!({"command": "echo 42 open PRs"}),
                        )],
                    ),
                    task(
                        "executor",
                        &["gather"],
                        &[],
                        vec![gated_step(
                            "executor-step",
                            "procedural.noop",
                            serde_json::json!({"output": "merged"}),
                            "operator",
                        )],
                    ),
                ],
            )],
        )
    }

    #[serial_test::serial]

    #[test]
    fn ephemeral_run_gate_handler_receives_the_gather_tasks_output_and_approving_runs_the_executor() {
        let cfg = gather_then_gated_config();
        let tmp = std::env::temp_dir();

        let mut received: Option<Map<String, String>> = None;
        let mut approve = |_s: &Step, f: &Map<String, String>| {
            received = Some(f.clone());
            crate::crew::gate::GateDecision::Approved
        };
        let out = run_ephemeral(&cfg, "", &tmp, Some(&mut approve)).expect("ephemeral run succeeds");
        assert_eq!(out.text.trim(), "merged", "an approved gate must let the executor step actually run");
        assert!(out.success);
        assert_eq!(
            received.as_ref().and_then(|f| f.get("gather")).map(|s| s.trim()),
            Some("42 open PRs"),
            "the gate handler must receive the gather task's output as its facts map — the \
             dialog-body contract: {received:?}"
        );
    }

    #[serial_test::serial]

    #[test]
    fn ephemeral_run_gate_handler_declining_fails_the_command_without_running_the_executor() {
        let cfg = gather_then_gated_config();
        let tmp = std::env::temp_dir();

        let mut decline = |_s: &Step, _f: &Map<String, String>| crate::crew::gate::GateDecision::Declined {
            reason: "operator declined".to_string(),
        };
        // `run_ephemeral` still returns `Ok` — a declined gate is a command
        // FAILURE (rendered as such, mirroring `render_ephemeral_result`'s
        // existing Error-terminal handling), never a hard `Err` propagated
        // across the ACP boundary.
        let out = run_ephemeral(&cfg, "", &tmp, Some(&mut decline))
            .expect("a declined gate still renders a command-failed message, not an Err");
        assert!(!out.success, "a declined gate must report success: false");
        assert!(out.text.contains("darkmux: command failed"), "{}", out.text);
        assert!(out.text.contains("operator declined"), "{}", out.text);
    }

    // ── (#1685) cmd-gate allowlist + audit record ────────────────────────

    fn gather_then_gated_config_with_verb(verb: &str) -> MissionConfig {
        let mut cfg = gather_then_gated_config();
        cfg.cmd = Some(verb.to_string());
        cfg
    }

    /// Env isolation for the `DARKMUX_CMD_ENABLED`/`DARKMUX_CMD_ALLOWED` pair
    /// — restores both on drop, mirroring the restore blocks the rest of
    /// this file's tests hand-roll inline (kept as a tiny guard here since
    /// this cluster of tests uses it four times).
    struct GhEnvGuard {
        prev_enabled: Option<String>,
        prev_allowed: Option<String>,
    }
    impl GhEnvGuard {
        fn off() -> Self {
            let g = Self {
                prev_enabled: std::env::var("DARKMUX_CMD_ENABLED").ok(),
                prev_allowed: std::env::var("DARKMUX_CMD_ALLOWED").ok(),
            };
            unsafe {
                std::env::remove_var("DARKMUX_CMD_ENABLED");
                std::env::remove_var("DARKMUX_CMD_ALLOWED");
            }
            g
        }
        fn on(allowed: &str) -> Self {
            let g = Self {
                prev_enabled: std::env::var("DARKMUX_CMD_ENABLED").ok(),
                prev_allowed: std::env::var("DARKMUX_CMD_ALLOWED").ok(),
            };
            unsafe {
                std::env::set_var("DARKMUX_CMD_ENABLED", "true");
                std::env::set_var("DARKMUX_CMD_ALLOWED", allowed);
            }
            g
        }
    }
    impl Drop for GhEnvGuard {
        fn drop(&mut self) {
            unsafe {
                match &self.prev_enabled {
                    Some(v) => std::env::set_var("DARKMUX_CMD_ENABLED", v),
                    None => std::env::remove_var("DARKMUX_CMD_ENABLED"),
                }
                match &self.prev_allowed {
                    Some(v) => std::env::set_var("DARKMUX_CMD_ALLOWED", v),
                    None => std::env::remove_var("DARKMUX_CMD_ALLOWED"),
                }
            }
        }
    }

    /// Every flow record written to the isolated `DARKMUX_FLOWS_DIR` so far
    /// — mirrors `mission_launch.rs`'s own `read_all_flow_records` test
    /// helper (same on-disk shape, read raw off disk).
    fn read_all_flow_records() -> Vec<serde_json::Value> {
        let dir = std::env::var("DARKMUX_FLOWS_DIR").expect("DARKMUX_FLOWS_DIR must be set by an active guard");
        let mut out = Vec::new();
        let rd = match std::fs::read_dir(&dir) {
            Ok(rd) => rd,
            Err(_) => return out,
        };
        for entry in rd.filter_map(|e| e.ok()) {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
                continue;
            }
            let contents = std::fs::read_to_string(&path).unwrap_or_default();
            for line in contents.lines() {
                if line.trim().is_empty() {
                    continue;
                }
                if let Ok(v) = serde_json::from_str::<serde_json::Value>(line) {
                    out.push(v);
                }
            }
        }
        out
    }

    /// The allowlist check happens BEFORE `validate()`/`interpret()` ever
    /// run — with the gate off, NOT ONE step of the graph executes, gated
    /// or not (proved two ways: the gate handler, which would only ever be
    /// invoked for the GATED `executor` step, panics if called at all; and
    /// zero flow records land on disk — the UNGATED `gather` step would
    /// otherwise have emitted step-lifecycle records even though it needs
    /// no sign-off).
    #[test]
    #[serial_test::serial]
    fn run_ephemeral_blocks_a_cmd_config_when_the_allowlist_gate_is_off() {
        let _gh = GhEnvGuard::off();
        let tmp_flows = tempfile::TempDir::new().unwrap();
        let prev_flows = std::env::var("DARKMUX_FLOWS_DIR").ok();
        unsafe { std::env::set_var("DARKMUX_FLOWS_DIR", tmp_flows.path()) };

        let cfg = gather_then_gated_config_with_verb("pr-merge");
        let tmp = std::env::temp_dir();
        let mut never_called = |_s: &Step, _f: &Map<String, String>| {
            panic!("the gate handler must never be invoked — the allowlist check refuses the whole config first")
        };
        let out = run_ephemeral(&cfg, "", &tmp, Some(&mut never_called))
            .expect("a blocked cmd-gate config still renders a command-failed message, not an Err");
        assert!(!out.success, "{}", out.text);
        assert!(out.text.contains("pr-merge"), "names the verb: {}", out.text);
        assert!(out.text.contains("cmd.enabled"), "points at the fix: {}", out.text);
        assert!(
            read_all_flow_records().is_empty(),
            "a blocked config must run zero steps, not even the ungated gather step"
        );

        unsafe {
            match prev_flows {
                Some(v) => std::env::set_var("DARKMUX_FLOWS_DIR", v),
                None => std::env::remove_var("DARKMUX_FLOWS_DIR"),
            }
        }
    }

    /// The inverted case (required by the #1685 spec): a verb that IS
    /// allowlisted, with its gate approved, actually runs — proving the
    /// allowlist gate is not fail-closed by accident (it also opens).
    #[test]
    #[serial_test::serial]
    fn run_ephemeral_runs_a_cmd_config_once_allowlisted_and_gate_approved() {
        let _gh = GhEnvGuard::on("pr-list,pr-merge");
        let cfg = gather_then_gated_config_with_verb("pr-merge");
        let tmp = std::env::temp_dir();
        let mut approve = |_s: &Step, _f: &Map<String, String>| crate::crew::gate::GateDecision::Approved;
        let out = run_ephemeral(&cfg, "", &tmp, Some(&mut approve)).expect("ephemeral run succeeds");
        assert_eq!(out.text.trim(), "merged", "allowlisted + approved must actually run the executor");
        assert!(out.success);
    }

    /// The permission-dialog facts contract this whole feature depends on:
    /// the gathered CI/verdict/adjudication text the operator sees in the
    /// `session/request_permission` dialog IS the cmd-gate config's own
    /// `gather` task output — exercised end to end through `run_ephemeral`
    /// (the ACP wiring in `src/acp.rs::acp_gate_handler` renders this exact
    /// map with `render_gate_facts`, one `key: value` line per fact).
    #[test]
    #[serial_test::serial]
    fn run_ephemeral_cmd_gate_handler_sees_the_gathered_ci_and_verdict_facts() {
        let _gh = GhEnvGuard::on("pr-merge");
        let cfg = config(
            "pr-merge",
            None,
            vec![phase(
                "p1",
                vec![
                    task(
                        "gather",
                        &[],
                        &[],
                        vec![step(
                            "gather-step",
                            "procedural.shell",
                            serde_json::json!({
                                "command": "echo 'ci: SUCCESS\nreview: no confirmed findings\nadjudication: advisory only, no higher-tier review\npr: 123 -> main'"
                            }),
                        )],
                    ),
                    task(
                        "executor",
                        &["gather"],
                        &[],
                        vec![gated_step(
                            "executor-step",
                            "procedural.noop",
                            serde_json::json!({"output": "merged"}),
                            "operator",
                        )],
                    ),
                ],
            )],
        );
        let mut cfg = cfg;
        cfg.cmd = Some("pr-merge".to_string());
        let tmp = std::env::temp_dir();

        let mut received: Option<Map<String, String>> = None;
        let mut approve = |_s: &Step, f: &Map<String, String>| {
            received = Some(f.clone());
            crate::crew::gate::GateDecision::Approved
        };
        let out = run_ephemeral(&cfg, "", &tmp, Some(&mut approve)).expect("ephemeral run succeeds");
        assert_eq!(out.text.trim(), "merged");
        let facts = received.expect("the gate handler must have been invoked");
        let gathered = facts.get("gather").expect("the gather task's output must be a fact");
        assert!(gathered.contains("ci: SUCCESS"), "CI status by conclusion, not just completed: {gathered}");
        assert!(gathered.contains("no confirmed findings"), "the review verdict: {gathered}");
        assert!(gathered.contains("advisory only"), "the adjudication state: {gathered}");
        assert!(gathered.contains("123 -> main"), "PR/branch provenance: {gathered}");
    }

    /// One flow-record audit entry per EXECUTED gated command — verb, PR
    /// (best-effort from the raw args), worktree (the session cwd), and
    /// whether the operator confirmed it.
    #[test]
    #[serial_test::serial]
    fn run_ephemeral_emits_one_audit_flow_record_per_executed_cmd() {
        let _gh = GhEnvGuard::on("pr-merge");
        let tmp_flows = tempfile::TempDir::new().unwrap();
        let prev_flows = std::env::var("DARKMUX_FLOWS_DIR").ok();
        unsafe { std::env::set_var("DARKMUX_FLOWS_DIR", tmp_flows.path()) };

        let cfg = gather_then_gated_config_with_verb("pr-merge");
        let worktree = std::env::temp_dir();
        let mut approve = |_s: &Step, _f: &Map<String, String>| crate::crew::gate::GateDecision::Approved;
        let out = run_ephemeral(&cfg, "123", &worktree, Some(&mut approve)).expect("ephemeral run succeeds");
        assert!(out.success);

        let records = read_all_flow_records();
        let audit = records
            .iter()
            .find(|r| r["action"] == "gh.verb.executed")
            .expect("exactly one gh.verb.executed audit record must be emitted");
        assert_eq!(audit["category"], "audit");
        let payload = &audit["payload"];
        assert_eq!(payload["verb"], "pr-merge");
        assert_eq!(payload["pr"], "123", "best-effort PR extraction from the raw args");
        assert_eq!(payload["worktree"], worktree.to_string_lossy().to_string());
        assert_eq!(payload["confirmed"], true, "the operator approved the gate");
        assert_eq!(payload["success"], true);

        unsafe {
            match prev_flows {
                Some(v) => std::env::set_var("DARKMUX_FLOWS_DIR", v),
                None => std::env::remove_var("DARKMUX_FLOWS_DIR"),
            }
        }
    }

    /// A config with NO `cmd` (the ordinary case — every panel command
    /// that isn't a GitHub-CLI verb) never emits this audit record at all.
    #[test]
    #[serial_test::serial]
    fn run_ephemeral_emits_no_audit_record_for_a_non_cmd_config() {
        let tmp_flows = tempfile::TempDir::new().unwrap();
        let prev_flows = std::env::var("DARKMUX_FLOWS_DIR").ok();
        unsafe { std::env::set_var("DARKMUX_FLOWS_DIR", tmp_flows.path()) };

        let cfg = config(
            "echo-test",
            None,
            vec![phase(
                "p1",
                vec![task("t1", &[], &[], vec![step("s1", "procedural.shell", serde_json::json!({"command": "echo hi"}))])],
            )],
        );
        assert!(cfg.cmd.is_none());
        let tmp = std::env::temp_dir();
        let out = run_ephemeral(&cfg, "", &tmp, None).expect("ephemeral run succeeds");
        assert!(out.success);
        assert!(
            read_all_flow_records().iter().all(|r| r["action"] != "gh.verb.executed"),
            "no cmd declared → no audit record, ever"
        );

        unsafe {
            match prev_flows {
                Some(v) => std::env::set_var("DARKMUX_FLOWS_DIR", v),
                None => std::env::remove_var("DARKMUX_FLOWS_DIR"),
            }
        }
    }

    // ── (#2310 P4d) diff synthesis for the panel/radio surfaces ────────

    /// A git repo with `n` commits, each adding one line to `src/a.rs`.
    fn temp_repo(commits: usize) -> tempfile::TempDir {
        let dir = tempfile::TempDir::new().unwrap();
        let run = |args: &[&str]| {
            let out = std::process::Command::new("git")
                .args(args)
                .current_dir(dir.path())
                .output()
                .expect("git runs");
            assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
        };
        run(&["init", "-q", "-b", "main"]);
        run(&["config", "user.email", "t@example.com"]);
        run(&["config", "user.name", "t"]);
        std::fs::create_dir_all(dir.path().join("src")).unwrap();
        for i in 0..commits {
            let mut body = String::new();
            for k in 0..=i {
                body.push_str(&format!("fn f{k}() {{}}\n"));
            }
            std::fs::write(dir.path().join("src/a.rs"), body).unwrap();
            run(&["add", "-A"]);
            run(&["commit", "-q", "-m", &format!("c{i}")]);
        }
        dir
    }

    fn embedded_review() -> MissionConfig {
        mission_config::load("review").expect("the embedded review config loads").config
    }

    /// (#2404 P4d round 3) A no-remote repo whose base branch is only
    /// reachable LOCALLY (no `origin/*` refs at all) used to fall straight
    /// to the `HEAD~1` last resort, which reviews only the single most
    /// recent commit — silently under-reviewing every commit before it.
    /// `main` gets 3 commits, then `feat` branches off and gets 3 more;
    /// the merge-base chain's local `main` candidate must resolve the
    /// base to where `feat` diverged, so the diff covers all 3 of
    /// `feat`'s commits, not just its last one.
    #[test]
    #[serial_test::serial]
    fn no_remote_repo_finds_the_local_main_branch_and_reviews_every_commit() {
        // Isolate from the operator's real ~/.gitconfig: if it happens to
        // set `init.defaultBranch = main` globally, that candidate alone
        // would mask a broken local-branch fallback and this test would
        // stay green for the wrong reason. `GIT_CONFIG_GLOBAL`/`_SYSTEM`
        // pointed at `/dev/null` makes `git config init.defaultBranch`
        // resolve to nothing here, so only the LOCAL `main`/`master`
        // candidates this test means to prove can make it pass.
        let prev_global = std::env::var("GIT_CONFIG_GLOBAL").ok();
        let prev_nosystem = std::env::var("GIT_CONFIG_NOSYSTEM").ok();
        unsafe {
            std::env::set_var("GIT_CONFIG_GLOBAL", "/dev/null");
            // Xcode's bundled git ships its OWN system gitconfig
            // (`init.defaultbranch=main`) that `GIT_CONFIG_SYSTEM=/dev/null`
            // does not suppress on this platform — `GIT_CONFIG_NOSYSTEM`
            // is the flag that actually disables it.
            std::env::set_var("GIT_CONFIG_NOSYSTEM", "1");
        }
        let repo = temp_repo(3);
        let run = |args: &[&str]| {
            let out = std::process::Command::new("git")
                .args(args)
                .current_dir(repo.path())
                .output()
                .expect("git runs");
            assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
        };
        run(&["checkout", "-q", "-b", "feat"]);
        for k in 3..6 {
            std::fs::write(repo.path().join(format!("src/f{k}.rs")), format!("fn f{k}() {{}}
")).unwrap();
            run(&["add", "-A"]);
            run(&["commit", "-q", "-m", &format!("feat c{k}")]);
        }
        let synth = match synthesize_diff_launch_inputs(&embedded_review(), repo.path()).unwrap() {
            DiffLaunchInputs::Ready(s) => s,
            other => panic!("expected Ready, got {}", match other {
                DiffLaunchInputs::NotNeeded => "NotNeeded",
                DiffLaunchInputs::Nothing(_) => "Nothing",
                DiffLaunchInputs::Ready(_) => unreachable!(),
            }),
        };
        let diff_path = synth
            .params()
            .iter()
            .find_map(|p| p.strip_prefix("diff_file=").map(String::from))
            .expect("diff_file param present");
        let diff = std::fs::read_to_string(diff_path).unwrap();
        for k in 3..6 {
            assert!(diff.contains(&format!("f{k}.rs")), "commit for f{k}.rs missing from the diff — reviewed less than 3 of 3:\n{diff}");
        }
        assert!(synth.excluded_note.is_none(), "a local main WAS found, so no last-resort note should print: {:?}", synth.excluded_note);
        unsafe {
            match prev_global {
                Some(v) => std::env::set_var("GIT_CONFIG_GLOBAL", v),
                None => std::env::remove_var("GIT_CONFIG_GLOBAL"),
            }
            match prev_nosystem {
                Some(v) => std::env::set_var("GIT_CONFIG_NOSYSTEM", v),
                None => std::env::remove_var("GIT_CONFIG_NOSYSTEM"),
            }
        }
    }

    /// RED before this packet's synthesis existed: the panel typed no
    /// params, `review.json` declares `diff_file` REQUIRED, and the launch
    /// bailed on the missing input — `/review` in the editor was broken.
    #[test]
    #[serial_test::serial]
    fn a_diff_scoped_config_gets_its_diff_workspace_and_head_synthesized_from_the_cwd() {
        let repo = temp_repo(2);
        let synth = match synthesize_diff_launch_inputs(&embedded_review(), repo.path()).unwrap() {
            DiffLaunchInputs::Ready(s) => s,
            other => panic!("expected Ready, got {}", match other {
                DiffLaunchInputs::NotNeeded => "NotNeeded",
                DiffLaunchInputs::Nothing(_) => "Nothing",
                DiffLaunchInputs::Ready(_) => unreachable!(),
            }),
        };
        let by_key = |k: &str| {
            synth
                .params()
                .iter()
                .find_map(|p| p.strip_prefix(&format!("{k}=")).map(String::from))
                .unwrap_or_else(|| panic!("no `{k}=` among {:?}", synth.params()))
        };
        let diff = std::fs::read_to_string(by_key("diff_file")).expect("the diff file is written");
        assert!(diff.contains("src/a.rs"), "the synthesized diff must cover the commit: {diff}");
        let spec: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(by_key("workspace")).unwrap()).unwrap();
        let sources = spec["sources"].as_array().unwrap();
        assert_eq!(sources.len(), 1, "exactly one source: {spec}");
        assert_eq!(
            sources[0]["path"].as_str().unwrap(),
            repo.path().to_string_lossy(),
            "the source is a `path` origin at THIS checkout — no clone from a remote: {spec}"
        );
        let head = by_key("head_sha");
        assert_eq!(sources[0]["ref"].as_str().unwrap(), head, "the source is pinned to the diff's head");
    }

    /// The tempdir is removed when the guard drops — every exit path, since
    /// the caller holds it for the whole spawn.
    #[test]
    #[serial_test::serial]
    fn dropping_the_synthesized_inputs_removes_their_tempdir() {
        let repo = temp_repo(2);
        let dir = match synthesize_diff_launch_inputs(&embedded_review(), repo.path()).unwrap() {
            DiffLaunchInputs::Ready(s) => {
                let d = s.params()[0]
                    .strip_prefix("diff_file=")
                    .map(|p| std::path::PathBuf::from(p).parent().unwrap().to_path_buf())
                    .unwrap();
                assert!(d.exists());
                d
            }
            _ => panic!("expected Ready"),
        };
        assert!(!dir.exists(), "the synthesized-input dir must be gone once the guard drops");
    }

    /// A repo whose HEAD has nothing to review against its base is a
    /// rendered outcome, never an error or a launch that bails on a
    /// missing input.
    #[test]
    #[serial_test::serial]
    fn a_repo_with_no_reviewable_commit_reports_nothing_rather_than_launching() {
        let repo = temp_repo(1);
        match synthesize_diff_launch_inputs(&embedded_review(), repo.path()).unwrap() {
            DiffLaunchInputs::Nothing(msg) => assert!(msg.contains("Nothing committed"), "{msg}"),
            _ => panic!("expected Nothing"),
        }
    }

    /// A config that declares no required `diff_file` is untouched — the
    /// decision is STRUCTURAL, never `id == "review"`.
    #[test]
    #[serial_test::serial]
    fn a_config_without_a_required_diff_file_gets_nothing_synthesized() {
        let repo = temp_repo(2);
        let cfg = config(
            "plain",
            None,
            vec![phase("p1", vec![task("t1", &[], &[], vec![step("s1", "dispatch.internal", serde_json::Value::Null)])])],
        );
        assert!(matches!(
            synthesize_diff_launch_inputs(&cfg, repo.path()).unwrap(),
            DiffLaunchInputs::NotNeeded
        ));
    }

    /// The end the operator actually cares about: the params the panel
    /// synthesizes make `mission launch review --dry-run` resolve every
    /// input and mint the graph. Stubbed home/profiles/`lms` — no model,
    /// no network, no mutation of the operator's real state.
    #[test]
    #[serial_test::serial]
    fn the_panel_synthesized_params_dry_run_green() {
        use assert_cmd::prelude::*;
        let repo = temp_repo(2);
        let home = tempfile::TempDir::new().unwrap();
        std::fs::write(home.path().join("profiles.json"), r#"{"profiles":{},"default_profile":null}"#).unwrap();
        let synth = match synthesize_diff_launch_inputs(&embedded_review(), repo.path()).unwrap() {
            DiffLaunchInputs::Ready(s) => s,
            _ => panic!("expected Ready"),
        };
        let mut cmd = std::process::Command::cargo_bin("darkmux").unwrap();
        cmd.args(["mission", "launch", "review"]);
        for p in synth.params() {
            cmd.args(["--param", p]);
        }
        let out = cmd
            .arg("--dry-run")
            .env("DARKMUX_HOME", home.path())
            .env("DARKMUX_FLOWS_DIR", home.path().join("flows"))
            .env("DARKMUX_PROFILES", home.path().join("profiles.json"))
            .env("DARKMUX_LMS_BIN", "/usr/bin/true")
            .output()
            .expect("mission launch review --dry-run runs");
        let stdout = String::from_utf8_lossy(&out.stdout);
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(out.status.success(), "dry run must be green.\nstdout:\n{stdout}\nstderr:\n{stderr}");
        assert!(stdout.contains("plan.sites"), "the graph must mint the plan steps:\n{stdout}");
        assert!(
            !stdout.contains("diff_file") || stdout.contains("diff_file = "),
            "diff_file must resolve, never be reported missing:\n{stdout}"
        );
    }

}
