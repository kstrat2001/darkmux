//! `mods.gate` (#2310 P4c-2b, fixed post-review in PR #2357) — the
//! create-mods confirmation gate. DESIGN.md "the changed files name the
//! test targets, which is what makes confirmation cheap enough to do per
//! finding": APPLY a mod's kit onto a scratch copy of the source checkout,
//! then run `config.test_command` (when the review declared one) against
//! THAT patched copy, and record the outcome onto every stored mod naming
//! `config.for_key` ([`mods::record_gate`]).
//!
//! **MUST FIX A (PR #2357 review, proven).** The first shipped version of
//! this kind ran `test_command` directly in the READ-ONLY source mirror,
//! never touching the mod's own kit at all — so `gate.passed` measured
//! "does the baseline tree's test suite pass", the SAME answer for every
//! mod regardless of what it proposed, and a wrong mod would have shipped
//! as a confirmed suggestion. [`gate_one_mod`] now: resolves the single
//! source checkout under `config.workdir` (the crawl's own `tree_root`,
//! whose only child is the materialized source — `resolve_single_source_dir`),
//! copies it to a scratch dir ([`copy_dir_recursive`] — never `git
//! worktree add`, which would need the mirror to be a real git checkout;
//! a plain copy works for any source and leaves the mirror provably
//! untouched, asserted by this module's own tests), `git apply --check`
//! then `git apply`s the mod's kit there, and only THEN runs
//! `test_command` — inside the PATCHED copy.
//!
//! **MUST FIX B (PR #2357 review, proven live).** The pre-fix `workdir`
//! was `tree_root` itself — one level ABOVE the actual checkout (the
//! crawl mounts `/workspace/<source>/...`; `tree_root` is the PARENT of
//! `<source>`) — so a real `npm test` found no `package.json` and every
//! gate silently measured "this command can't find its project", not
//! "does the test suite pass". Fixed by construction here: [`gate_one_mod`]
//! always runs `test_command` inside the RESOLVED source checkout (the
//! scratch copy's own root once MUST FIX A applies), never `tree_root`.
//! A genuine INFRASTRUCTURE failure (workdir missing, more than one
//! source checkout resolved, the scratch copy could not be made, the
//! command could not even be spawned) is a SKIP (`gate_skipped_reason`),
//! never `passed: false` — that distinction is the whole point of MUST
//! FIX B: an operator reading a mod's gate must be able to tell "the
//! change is bad" from "the gate itself couldn't run" at a glance.
//!
//! **Mission-agnostic by construction**, same discipline every other
//! `step_kinds::` Tier 1 kind in this crate follows: this reads/writes the
//! SHARED mod store (`crate::mods`) keyed by a finding key handed in
//! through config, never a review or crawl type.
//!
//! **Tier 1 (#1352).** A fixed procedure — apply a kit (or don't), run one
//! command (or don't), write one fact onto zero or more already-stored
//! records — config-driven, no caller-supplied strategy, no per-mission
//! control flow. Physically its own file (not folded into `builtins.rs`,
//! already ~4200 lines) for the same monolith-avoidance reason
//! `deliver_github_review.rs` states for itself.
//!
//! **Every non-"real gate ran" outcome is a named SKIP, never a fabricated
//! pass or fail:**
//! - no `test_command` configured → `"no test_command configured"`
//! - the mod's kit isn't a unified diff (or has none) → `"no applicable
//!   kit"` (only a `kit_kind == "unified-diff"` mod can be mechanically
//!   applied — see `deliver_github_review`'s own doc on why an opaque kit
//!   is never treated as a patch)
//! - `config.workdir` missing/unreadable, or it resolves to zero or more
//!   than one source checkout, or the scratch copy/command could not even
//!   be spawned → a workdir/infra reason naming what failed
//!
//! A kit that DOES apply and whose `test_command` DOES run is the only
//! path that ever produces a real `GateOutcome` — `passed: true` (the
//! patched checkout's own test run exited 0) or `passed: false` (kit
//! failed to apply, `reason: "kit did not apply"`; or it applied and the
//! patched checkout's test run exited nonzero). `deliver_github_review`'s
//! own render already tells "confirmed"/"failed"/"never gated" apart
//! (`GatedMod::gate_passed: Some(true)`/`Some(false)`/`None`), so an
//! honest skip reads correctly downstream with no special-casing there.
//!
//! **A nonzero test exit is DATA, not a step failure.** Unlike
//! `procedural.shell` (which bails the step on a nonzero exit), this kind
//! never fails the STEP over the command's own exit code, or over a kit
//! that failed to apply — either is exactly as valid an outcome as a
//! pass; only a genuine infrastructure problem (an unreadable mod store, a
//! malformed config) fails the step itself. A `test_command` that exceeds
//! `runtime.step_command_timeout_seconds` is likewise a named SKIP
//! (`"test_command exceeded <n>s"`, #2361 / swarm S4-4), never a
//! fabricated `passed: false`: a suite that hung says nothing about the
//! kit.
//!
//! # Trust boundary — a gate run EXECUTES model-authored code (S5-1)
//!
//! This is the one place in darkmux where a MODEL's product is not just
//! recorded but RUN. The gate applies a mod's kit — written by whatever
//! model the create-mods seat was staffed with — to a scratch copy and
//! then runs the operator's `test_command` inside that patched tree, as
//! the darkmux user, with that user's filesystem and network. A kit is
//! only a "small unified diff" by convention: one that edits
//! `package.json`'s `scripts`, a `Makefile`, `conftest.py`, `build.rs`,
//! `.cargo/config.toml`, or any file the test command loads is model code
//! that executes the moment the gate runs. `git apply --check` proves a
//! patch APPLIES; it says nothing about what the patched tree then does.
//!
//! What bounds the blast radius here, honestly: the patch lands in a
//! throwaway copy (the source checkout is never patched), kit paths that
//! escape the checkout are refused before the apply, and the command is
//! killed at the configured deadline. What does NOT bound it: the
//! command's own privileges, its network access, or anything it writes
//! outside the scratch dir. **So point `test_command` only at a tree you
//! would be willing to run untrusted code in** — and treat enabling the
//! gate on a machine holding credentials as the decision it is. An
//! operator who configures no `test_command` never executes anything: the
//! gate skips with `"no test_command configured"`.

use crate::mods::{self, GateOutcome, ModRecord};
use crate::step_kinds::registry::StepKindRegistry;
use crate::step_kinds::types::{SeatClaim, StepKind, StepOutcome, StepRunCtx};
use crate::types::{Step, Task};
use anyhow::{anyhow, Context, Result};
use serde::Serialize;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

pub const MODS_GATE_KIND: &str = "mods.gate";

/// What one `mods.gate` step reports as its own output — a small summary,
/// never the gated mods themselves (a reader wanting those reads the mod
/// store, the same discipline `crawl.summary` uses for findings).
#[derive(Debug, Clone, Serialize)]
struct GateSummary {
    for_key: String,
    mods_seen: usize,
    mods_gated: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    skipped_reason: Option<String>,
}

pub struct ModsGateStepKind;

impl StepKind for ModsGateStepKind {
    /// (#2394) [`SeatClaim::NoModel`] — this kind runs an operator-supplied `test_command` per mod; it
    /// dispatches nothing. Bounded by `runtime.dispatch_free_concurrency`
    /// and, per command, by `runtime.step_command_timeout_seconds` — never
    /// by the hosted-endpoint cap.
    /// The `test_command` per mod is why the dispatch-free cap is a real
    /// number and not "unbounded": N of these at once is N test suites.
    fn seat(
        &self,
        _step: &Step,
        _task: &Task,
        _input: &std::collections::BTreeMap<String, String>,
        _ctx: &StepRunCtx,
    ) -> SeatClaim {
        SeatClaim::NoModel
    }

    fn id(&self) -> &'static str {
        MODS_GATE_KIND
    }

    fn display_name(&self) -> &'static str {
        "Gate"
    }

    /// (#1979) No model work, no dispatch session — same opt-out
    /// `deliver.github_review`/`procedural.shell` use.
    fn dispatch_session_id(&self, _step: &Step) -> Option<String> {
        None
    }

    fn run(&self, step: &Step, _task: &Task, _input: &BTreeMap<String, String>) -> Result<StepOutcome> {
        let for_key = step
            .config
            .get("for_key")
            .and_then(|v| v.as_str())
            .filter(|s| !s.trim().is_empty())
            .ok_or_else(|| anyhow!("step `{}`: `{MODS_GATE_KIND}` requires config.for_key", step.id))?;
        let test_command =
            step.config.get("test_command").and_then(|v| v.as_str()).filter(|s| !s.trim().is_empty());
        let workdir = step.config.get("workdir").and_then(|v| v.as_str());

        let root = mods::mods_dir();
        let all = mods::load_all_at(&root).context("loading the mod store")?;
        let targets = mods::mods_for(&all, for_key);

        // (#2310 P4c-2b PR #2357 review) The DOCUMENT-level skip reason
        // (no `test_command` at all) is computed once; a PER-MOD skip
        // reason (no applicable kit, workdir/infra trouble) is computed
        // per mod inside `gate_one_mod` instead — the two are different
        // questions and must not collapse into one summary field.
        let no_command_reason =
            if test_command.is_none() { Some("no test_command configured".to_string()) } else { None };
        // The source id is the name of the one checkout beneath the workdir —
        // the same resolution `gate_one_mod` applies the kit in. Recorded on
        // every mod this step gates that arrived without one, so the deliverer
        // can map container-coordinate kits the way the gate did.
        let resolved_source: Option<String> = workdir
            .and_then(|w| resolve_single_source_dir(Path::new(w)).ok())
            .and_then(|d| d.file_name().and_then(|n| n.to_str()).map(str::to_string));
        let mut mods_gated = 0usize;
        for m in &targets {
            // A mutation self-check on this line is documented in this
            // module's own test module (`a_mod_already_gated_is_left_
            // untouched_on_a_second_pass`'s own doc) — it is a
            // performance optimization (skip the process spawn entirely),
            // never the correctness guard; `mods::record_gate`'s own
            // already-gated check is that guard.
            if m.gate.is_some() || m.gate_skipped_reason.is_some() {
                continue;
            }
            let (outcome, skip_reason) = match test_command {
                None => (None, no_command_reason.clone()),
                Some(cmd) => gate_one_mod(m, cmd, workdir),
            };
            let res = mods::record_gate_with_source(&root, &m.key, outcome, skip_reason.as_deref(), resolved_source.as_deref())
                .with_context(|| format!("step `{}`: recording the gate for mod `{}`", step.id, m.key))?;
            if res == mods::Materialized::Created {
                mods_gated += 1;
            }
        }

        let summary =
            GateSummary { for_key: for_key.to_string(), mods_seen: targets.len(), mods_gated, skipped_reason: no_command_reason };
        Ok(StepOutcome {
            output: serde_json::to_string(&summary).context("serializing the gate summary")?,
            flow_records: Vec::new(),
        })
    }
}

/// Gate ONE mod against `command`, run inside a scratch copy of its
/// `for` finding's source checkout with the mod's own kit applied first —
/// MUST FIX A/B's whole point. Returns `(Some(outcome), None)` for a real
/// gate run (pass or fail), or `(None, Some(reason))` for an infra-level
/// skip. NEVER `(Some(outcome), Some(reason))` or `(None, None)` — exactly
/// one of the two is populated, always.
fn gate_one_mod(m: &ModRecord, command: &str, workdir: Option<&str>) -> (Option<GateOutcome>, Option<String>) {
    if m.kit_kind.as_deref() != Some("unified-diff") {
        // (#2310 P4c-2b PR #2357 review MUST FIX A) Only a kit DECLARED as
        // a unified diff can be mechanically applied — same discipline
        // `deliver_github_review`'s own suggestion-block rendering
        // follows for the identical reason (an opaque kit is never
        // guessed at). A kit-less mod (attachments only) falls here too.
        return (None, Some("no applicable kit".to_string()));
    }
    let Some(kit) = m.kit.as_deref() else {
        return (None, Some("no applicable kit".to_string()));
    };
    let Some(workdir) = workdir else {
        return (None, Some("no workdir configured".to_string()));
    };
    let source_dir = match resolve_single_source_dir(Path::new(workdir)) {
        Ok(d) => d,
        Err(reason) => return (None, Some(reason)),
    };
    let scratch = match new_scratch_dir() {
        Ok(t) => t,
        Err(e) => return (None, Some(format!("could not create a scratch dir: {e}"))),
    };
    let scratch_checkout = scratch.path().join("checkout");
    if let Err(e) = copy_dir_recursive(&source_dir, &scratch_checkout) {
        return (None, Some(format!("could not copy the source checkout: {e}")));
    }
    // (#2361, swarm S5-6a) The copy must not carry the source's `.git`. A
    // materialized source is a LINKED WORKTREE, so its `.git` is a small
    // FILE (`gitdir: …/worktrees/<id>`) pointing back at the shared bare
    // mirror — copied verbatim, the scratch becomes a second live worktree
    // of that mirror, and a `test_command` that does any git write (`git
    // stash`, `git commit`, `git checkout`) mutates the mirror every other
    // unit in the run is reading from. Nothing here needs a repo: `git
    // apply` runs as a raw patch tool under `GIT_CEILING_DIRECTORIES`
    // (below), which is the mode this kind already wants.
    if let Err(e) = strip_git_dir(&scratch_checkout) {
        return (None, Some(format!("could not detach the scratch copy from git: {e}")));
    }
    // (live proof 2026-09-05) A create-mod dispatch carries no `context.source`,
    // so its kit reaches the store in container coordinates — `a/<source
    // id>/src/x.ts` — with nothing having mapped it (`create_from_emission`
    // maps only when the emission names a source). The gate has just resolved
    // the one source checkout beneath the workdir, and that directory's name
    // IS the source id, so map here; a kit already in repo coordinates is a
    // no-op for the `a/`/`b/`-marked headers (see `strip_kit_source_prefix`).
    let mapped_kit;
    let kit = match source_dir.file_name().and_then(|n| n.to_str()) {
        Some(source_id) if !source_id.trim().is_empty() => {
            mapped_kit = mods::strip_kit_source_prefix(source_id, kit);
            mapped_kit.as_str()
        }
        _ => kit,
    };
    let patch_path = scratch.path().join("mod.patch");
    // (#2387) Terminate the patch. A kit arrives as a JSON string, which
    // needs no trailing newline and rarely has one; `git apply` reads an
    // unterminated last hunk line as "corrupt patch" — three of four kits in
    // the first live review-v2 run failed here byte-for-byte applicable.
    let kit: std::borrow::Cow<str> = if kit.ends_with('\n') { kit.into() } else { format!("{kit}\n").into() };
    if let Err(e) = std::fs::write(&patch_path, kit.as_bytes()) {
        return (None, Some(format!("could not write the mod's kit to a scratch file: {e}")));
    }

    let not_applied = || {
        (
            Some(GateOutcome {
                passed: false,
                command: command.to_string(),
                exit_code: None,
                applied: Some(false),
                reason: Some("kit did not apply".to_string()),
            }),
            None,
        )
    };

    // (#2310 P4c-2b PR #2357 round-2 review item 2, proven live) Without
    // `GIT_CEILING_DIRECTORIES`, `git apply` run from `scratch_checkout`
    // walks UP looking for a `.git` — if the scratch dir happens to sit
    // under an AMBIENT git repo (a real risk: `tempfile::TempDir::new()`
    // uses `$TMPDIR`, which is not always outside every repo on disk),
    // git resolves the patch against that ambient repo's root instead of
    // `scratch_checkout`, and `git apply`/`--check` reports SUCCESS while
    // touching nothing in the checkout at all — reproduced manually: the
    // exact same patch, same cwd, differs ONLY in this env var, and only
    // the ceiling'd run actually changes the file. Bounding repo discovery
    // AT `scratch.path()` (one level above the checkout, where nothing
    // this kind ever writes a `.git`) means `git` finds no repo at all —
    // exactly the "operate as a raw patch tool" mode this kind needs.
    let ceiling = scratch.path().to_string_lossy().to_string();

    // `git apply --check` first — a dry run that never touches the
    // checkout, so a kit that fails to apply is detected before anything
    // is mutated.
    let check = std::process::Command::new("git")
        .current_dir(&scratch_checkout)
        .env("GIT_CEILING_DIRECTORIES", &ceiling)
        // `--recount` (both calls): a model-written hunk header's line counts
        // are wrong often enough (`@@ -1,7 +1,5 @@` over an 8/9-line body —
        // 1 of 3 failed kits in the third live run) that git's "corrupt
        // patch" was sinking correct bodies. Recounting trusts the body,
        // which is the part the model actually looked at; wrong CONTEXT still
        // fails as "patch does not apply", honestly.
        .args(["apply", "--check", "--recount", &patch_path.to_string_lossy()])
        .output();
    let applied_cleanly = match check {
        Ok(out) => out.status.success(),
        Err(e) => return (None, Some(format!("could not run `git apply --check`: {e}"))),
    };
    if !applied_cleanly {
        // (#2310 P4c-2b PR #2357 review MUST FIX A) A kit that does not
        // apply is DATA about the mod, not an infrastructure failure —
        // `passed: false`, `applied: false`, never a skip.
        return not_applied();
    }

    // (#2310 P4c-2b PR #2357 round-2 review item 2) A fingerprint BEFORE
    // the real apply — the backstop for the same failure mode even when
    // `GIT_CEILING_DIRECTORIES` alone doesn't catch it (a future git
    // version, a different ambient-repo shape): `git apply` reporting
    // exit 0 is not proof the checkout changed; comparing the tree itself
    // is.
    let Ok(before) = tree_fingerprint(&scratch_checkout) else {
        return (None, Some("could not fingerprint the scratch checkout before applying".to_string()));
    };
    let apply = std::process::Command::new("git")
        .current_dir(&scratch_checkout)
        .env("GIT_CEILING_DIRECTORIES", &ceiling)
        .args(["apply", "--recount", &patch_path.to_string_lossy()])
        .output();
    match apply {
        Ok(out) if out.status.success() => {}
        // `--check` passed but the real apply failed anyway (rare —
        // a race on the scratch dir, a disk error) — same "kit did not
        // apply" data outcome, not a skip: the reviewer asked for the
        // kit's OWN applicability to be the signal, and this IS that.
        Ok(_) | Err(_) => return not_applied(),
    }
    let Ok(after) = tree_fingerprint(&scratch_checkout) else {
        return (None, Some("could not fingerprint the scratch checkout after applying".to_string()));
    };
    if before == after {
        // (#2310 P4c-2b PR #2357 round-2 review item 2, proven live) The
        // exact reproduction: `git apply` exited 0 but the tree it
        // reported applying to is byte-identical to before. Never a
        // false `applied: true` — this IS "the kit did not apply",
        // stated with the precise reason a reader can act on.
        return (
            Some(GateOutcome {
                passed: false,
                command: command.to_string(),
                exit_code: None,
                applied: Some(false),
                reason: Some("apply reported success but changed nothing".to_string()),
            }),
            None,
        );
    }

    // (#2310 P4c-2b PR #2357 review MUST FIX B) `test_command` runs
    // INSIDE the checkout, never `workdir`/`tree_root` — the whole point
    // of the fix. A command that cannot even be SPAWNED (a missing
    // interpreter, a workdir that vanished) is an infra skip; a command
    // that runs and exits nonzero is a real, data `passed: false`.
    //
    // (#2361, swarm S4-4) BOUNDED, in its own process group, and
    // registered with the child registry — see
    // `crate::bounded_command`'s module doc for the three failures the
    // unbounded `.output()` here produced live. A command that outruns
    // `runtime.step_command_timeout_seconds` is a named SKIP, not a
    // `passed: false`: a suite that hung says nothing about the kit.
    let mut test_cmd = std::process::Command::new("sh");
    test_cmd.arg("-c").arg(command).current_dir(&scratch_checkout);
    let timeout = crate::bounded_command::configured_timeout();
    match crate::bounded_command::run_bounded(test_cmd, timeout) {
        crate::bounded_command::Bounded::Finished { success, code, .. } => (
            Some(GateOutcome {
                passed: success,
                command: command.to_string(),
                exit_code: code,
                applied: Some(true),
                reason: None,
            }),
            None,
        ),
        crate::bounded_command::Bounded::TimedOut { seconds } => {
            (None, Some(format!("test_command exceeded {seconds}s")))
        }
        crate::bounded_command::Bounded::Interrupted => {
            (None, Some("interrupted before test_command finished".to_string()))
        }
        crate::bounded_command::Bounded::SpawnFailed(e) => {
            (None, Some(format!("could not run the gate command in {}: {e}", scratch_checkout.display())))
        }
    }
}

/// Remove a copied `.git` — a FILE for a linked worktree, a directory for
/// a plain clone — so the scratch copy is a plain tree, related to no
/// repository. Absent is fine (a source that is not a checkout at all).
fn strip_git_dir(checkout: &Path) -> std::io::Result<()> {
    let dot_git = checkout.join(".git");
    match std::fs::symlink_metadata(&dot_git) {
        Ok(meta) if meta.is_dir() => std::fs::remove_dir_all(&dot_git),
        Ok(_) => std::fs::remove_file(&dot_git),
        Err(_) => Ok(()),
    }
}

// (#2310 P4c-2b PR #2357 round-2 review item 2 self-QA) A THREAD-LOCAL
// test-only override for where the scratch dir lands — never a process
// env var (`$TMPDIR`). `cargo test`'s default harness spawns a dedicated
// OS thread per `#[test]` fn, so a thread-local is scoped to exactly the
// calling test with zero risk of leaking into a CONCURRENT, unrelated
// test — which mutating `$TMPDIR` (process-global) does NOT give you:
// the first version of the ambient-git-repo test below did exactly that
// and intermittently broke `workspace_spec::materialize`'s own git tests
// running in parallel, since only `#[serial_test::serial]`-tagged tests
// serialize against EACH OTHER, never against untagged ones.
thread_local! {
    static SCRATCH_BASE_OVERRIDE: std::cell::RefCell<Option<PathBuf>> = const { std::cell::RefCell::new(None) };
}

/// Create the scratch dir `gate_one_mod` copies a source checkout into —
/// under the test-only per-thread override when one is set, `$TMPDIR`
/// (`tempfile::TempDir::new()`'s own default) otherwise.
fn new_scratch_dir() -> std::io::Result<tempfile::TempDir> {
    let base = SCRATCH_BASE_OVERRIDE.with(|cell| cell.borrow().clone());
    match base {
        Some(dir) => tempfile::Builder::new().prefix("darkmux-gate-").tempdir_in(dir),
        None => tempfile::TempDir::new(),
    }
}

/// The single directory `tree_root` (the crawl's own `tree_root` — the
/// PARENT of the materialized source, per `FindingRef::tree_root`'s own
/// doc) holds. A single-source workspace spec (the only shape review-v2
/// supports today — `plan_diff_rule` itself refuses more than one source)
/// has exactly one. Zero or more than one is an infra-level refusal, never
/// a guess at which one is "the" source.
fn resolve_single_source_dir(tree_root: &Path) -> Result<PathBuf, String> {
    if !tree_root.is_dir() {
        return Err(format!("workdir {} does not exist", tree_root.display()));
    }
    let entries = std::fs::read_dir(tree_root).map_err(|e| format!("reading {}: {e}", tree_root.display()))?;
    let mut dirs: Vec<PathBuf> = Vec::new();
    for entry in entries.flatten() {
        if entry.path().is_dir() {
            dirs.push(entry.path());
        }
    }
    match dirs.len() {
        1 => Ok(dirs.into_iter().next().expect("len checked == 1")),
        0 => Err(format!("no source checkout found under {}", tree_root.display())),
        n => Err(format!(
            "{n} source checkouts found under {} — cannot resolve a single one",
            tree_root.display()
        )),
    }
}

/// A plain recursive file copy (directories + regular files; symlinks are
/// skipped rather than followed, to bound the walk) — deliberately NOT
/// `git worktree add` (the reviewer's own alternative): a plain copy works
/// whether or not the mirror is a real git checkout, and leaves the
/// mirror's own `.git` (if any) untouched rather than sharing object
/// storage with a scratch worktree that gets deleted out from under it.
///
/// (#2361, swarm S5-6b) **Why a serial per-file copy is cheap enough:**
/// it is one `fs::copy` per file (~8k files/s measured) and what it copies
/// is a MATERIALIZED source tree — a worktree checked out by
/// `workspace_spec::materialize`, which holds tracked files only: no
/// `target/`, no `node_modules/`, no build output (the spec's own
/// `DEFAULT_EXCLUDE` names those, and nothing in the run ever builds
/// inside the mirror). A source whose checkout DID hold a build tree would
/// make this the gate's dominant cost, and the fix then is a
/// copy-on-write clone (`clonefile`/`FICLONE`), not more threads.
fn copy_dir_recursive(src: &Path, dst: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dst)?;
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let file_type = entry.file_type()?;
        let dst_path = dst.join(entry.file_name());
        if file_type.is_dir() {
            copy_dir_recursive(&entry.path(), &dst_path)?;
        } else if file_type.is_file() {
            std::fs::copy(entry.path(), &dst_path)?;
        }
        // Symlinks: skipped, not followed — a scratch copy used once for
        // one gate run has no need of them, and following one could walk
        // outside `src`.
    }
    Ok(())
}

/// A content fingerprint of a directory tree — every regular file's
/// path (relative to `dir`) plus its bytes, hashed together in a
/// deterministic (sorted-path) order. (#2310 P4c-2b PR #2357 round-2
/// review item 2) The backstop against `git apply` reporting success
/// while changing nothing: comparing this before/after the real apply is
/// what actually proves the checkout changed, independent of the
/// process's own exit code.
fn tree_fingerprint(dir: &Path) -> std::io::Result<blake3::Hash> {
    let mut files: Vec<PathBuf> = Vec::new();
    collect_files(dir, &mut files)?;
    files.sort();
    let mut hasher = blake3::Hasher::new();
    for path in files {
        let rel = path.strip_prefix(dir).unwrap_or(&path);
        hasher.update(rel.to_string_lossy().as_bytes());
        hasher.update(b"\0");
        hasher.update(&std::fs::read(&path)?);
        hasher.update(b"\0");
    }
    Ok(hasher.finalize())
}

fn collect_files(dir: &Path, out: &mut Vec<PathBuf>) -> std::io::Result<()> {
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let file_type = entry.file_type()?;
        if file_type.is_dir() {
            collect_files(&entry.path(), out)?;
        } else if file_type.is_file() {
            out.push(entry.path());
        }
    }
    Ok(())
}

/// Register `mods.gate` — same opt-in shape
/// `deliver_github_review::register_deliver_kind`/`records_gather::
/// register_records_gather_kind` use.
pub fn register_mods_gate_kind(registry: &StepKindRegistry) -> Result<()> {
    registry.register(Arc::new(ModsGateStepKind)).context("registering mods.gate")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mods::{ForFinding, ModContext, ModRecord};
    use crate::types::{NodeStatus, Task};
    use serde_json::json;
    use tempfile::TempDir;

    /// Scopes `DARKMUX_MODS_DIR` for one test and restores the prior value
    /// on drop — including on panic/assert failure (unlike a manual
    /// `remove_var` at the end of a test body, which a failed assertion
    /// skips), the same RAII discipline `records_gather::tests::HomeGuard`
    /// uses for `DARKMUX_HOME`.
    struct ModsDirGuard(Option<String>);
    impl ModsDirGuard {
        fn set(p: &std::path::Path) -> Self {
            let prior = std::env::var("DARKMUX_MODS_DIR").ok();
            std::env::set_var("DARKMUX_MODS_DIR", p);
            Self(prior)
        }
    }
    impl Drop for ModsDirGuard {
        fn drop(&mut self) {
            match &self.0 {
                Some(v) => std::env::set_var("DARKMUX_MODS_DIR", v),
                None => std::env::remove_var("DARKMUX_MODS_DIR"),
            }
        }
    }

    fn a_mod(key: &str, for_key: &str) -> ModRecord {
        a_mod_kit(key, for_key, "kit text", None)
    }

    /// (#2310 P4c-2b PR #2357 review) `kit_kind`-parameterized — MUST FIX
    /// A only gates a mod whose kit is DECLARED `"unified-diff"`.
    fn a_mod_kit(key: &str, for_key: &str, kit: &str, kit_kind: Option<&str>) -> ModRecord {
        ModRecord {
            key: key.to_string(),
            ts: "2026-09-05T00:00:01Z".to_string(),
            by: "coder".to_string(),
            r#for: vec![for_key.to_string()],
            kit: Some(kit.to_string()),
            kit_looks_json: false,
            kit_kind: kit_kind.map(str::to_string),
            attachments: Vec::new(),
            context: ModContext {
                findings: vec![ForFinding {
                    key: for_key.to_string(),
                    mission_id: None,
                    context: None,
                    emitted: None,
                    missing: false,
                }],
            },
            warnings: Vec::new(),
            mission_id: None,
            phase_id: None,
            step_id: None,
            source: None,
            gate: None,
            gate_skipped_reason: None,
            schema_version: crate::mods::MOD_SCHEMA_VERSION.to_string(),
            extras: Default::default(),
        }
    }

    /// A `tree_root/<source_id>/answer.txt` fixture — the SAME two-level
    /// shape a real crawl unit's `tree_root` (the PARENT of the source
    /// checkout) has. `content` is the file's baseline (pre-kit) text.
    /// Returns `(tree_root_holder, tree_root_path)`; the caller passes
    /// `tree_root_path` as `config.workdir`, exactly what `review-v2.json`
    /// wires from `{{item.tree_root}}`.
    fn fixture_tree(content: &str) -> (TempDir, PathBuf) {
        let tmp = TempDir::new().unwrap();
        let tree_root = tmp.path().join("tree");
        let source_dir = tree_root.join("app");
        std::fs::create_dir_all(&source_dir).unwrap();
        std::fs::write(source_dir.join("answer.txt"), content).unwrap();
        (tmp, tree_root)
    }

    /// A unified diff changing `answer.txt`'s ONE line from `from` to `to`.
    fn one_line_kit(from: &str, to: &str) -> String {
        format!(
            "diff --git a/answer.txt b/answer.txt\n--- a/answer.txt\n+++ b/answer.txt\n@@ -1 +1 @@\n-{from}\n+{to}\n"
        )
    }

    fn step(config: serde_json::Value) -> Step {
        Step {
            id: "gate-step".into(),
            task_id: "create-mod-1".into(),
            kind: MODS_GATE_KIND.into(),
            gate: None,
            status: NodeStatus::Planned,
            config,
            started_ts: None,
            completed_ts: None,
            output: None,
        }
    }

    fn task() -> Task {
        Task {
            id: "create-mod-1".into(),
            phase_id: "p".into(),
            description: String::new(),
            display_name: None,
            step_ids: vec!["create-mod-step".into(), "gate-step".into()],
            depends_on: Vec::new(),
            reads: Vec::new(),
            role_id: None,
            profile_name: None,
            workdir: None,
            image: None,
            run_on: crate::types::default_run_on(),
        }
    }

    #[test]
    #[serial_test::serial] // scopes DARKMUX_MODS_DIR, a process-global
    fn no_test_command_configured_records_a_skip_reason_on_every_matching_mod() {
        let tmp = TempDir::new().unwrap();
        let _guard = ModsDirGuard::set(tmp.path());
        mods::materialize(tmp.path(), &a_mod("mod-1", "sess-a/1")).unwrap();

        let outcome = ModsGateStepKind
            .run(&step(json!({ "for_key": "sess-a/1" })), &task(), &BTreeMap::new())
            .unwrap();
        let summary: serde_json::Value = serde_json::from_str(&outcome.output).unwrap();
        assert_eq!(summary["mods_seen"], 1);
        assert_eq!(summary["skipped_reason"], "no test_command configured");

        let rec = mods::load_at(tmp.path(), "mod-1").unwrap().unwrap();
        assert!(rec.gate.is_none());
        assert_eq!(rec.gate_skipped_reason.as_deref(), Some("no test_command configured"));
    }

    /// (#2310 P4c-2b PR #2357 review MUST FIX A/B, proven) The reviewer's
    /// live probe, reproduced: a planted FAILING test in `answer.txt`
    /// (`grep -q right answer.txt` fails against baseline `wrong`), a mod
    /// whose kit fixes it. Before the fix, `test_command` ran directly in
    /// `tree_root` (one level above the checkout, MUST FIX B) with no kit
    /// ever applied (MUST FIX A) — so this would have measured the
    /// UNPATCHED baseline (a guaranteed fail, or a "no such file" infra
    /// error) regardless of the kit's content. After the fix: `passed:
    /// true`, `applied: Some(true)`, and the checkout used is a SCRATCH
    /// copy — the fixture's own `answer.txt` (the mirror) is unchanged.
    #[test]
    #[serial_test::serial] // scopes DARKMUX_MODS_DIR, a process-global
    fn a_kit_that_fixes_a_planted_failing_test_gates_passed() {
        let mods_dir = TempDir::new().unwrap();
        let _guard = ModsDirGuard::set(mods_dir.path());
        let (_fixture, tree_root) = fixture_tree("wrong\n");
        let mirror_file = tree_root.join("app/answer.txt");
        let before = std::fs::read_to_string(&mirror_file).unwrap();

        let kit = one_line_kit("wrong", "right");
        mods::materialize(mods_dir.path(), &a_mod_kit("mod-fix", "sess-a/1", &kit, Some("unified-diff"))).unwrap();

        ModsGateStepKind
            .run(
                &step(json!({
                    "for_key": "sess-a/1",
                    "test_command": "grep -q right answer.txt",
                    "workdir": tree_root.to_string_lossy(),
                })),
                &task(),
                &BTreeMap::new(),
            )
            .unwrap();

        let rec = mods::load_at(mods_dir.path(), "mod-fix").unwrap().unwrap();
        let gate = rec.gate.as_ref().unwrap_or_else(|| panic!("expected a real gate outcome: {rec:?}"));
        assert!(gate.passed, "{gate:?}");
        assert_eq!(gate.applied, Some(true));
        assert!(rec.gate_skipped_reason.is_none());

        // (MUST FIX A) the mirror is provably untouched — the gate ran
        // against a SCRATCH copy, never the source checkout itself.
        assert_eq!(std::fs::read_to_string(&mirror_file).unwrap(), before, "the source mirror must never be modified");
    }

    /// (#2361, swarm S4-4, proven live) A `test_command` that never
    /// returns is BOUNDED. Before the fix this ran `sh -c … .output()`
    /// with no deadline and no registered child: the step never returned,
    /// the mission stayed Active, and a SIGTERM did nothing until the
    /// command did. The bound is data about the RUN, not about the kit —
    /// a named skip, never a fabricated `passed: false`.
    #[test]
    #[serial_test::serial] // scopes DARKMUX_MODS_DIR + the timeout env var
    fn a_test_command_past_the_deadline_is_killed_and_skipped_with_the_reason() {
        let mods_dir = TempDir::new().unwrap();
        let _guard = ModsDirGuard::set(mods_dir.path());
        let (_fixture, tree_root) = fixture_tree("wrong\n");
        let kit = one_line_kit("wrong", "right");
        mods::materialize(mods_dir.path(), &a_mod_kit("mod-hang", "sess-a/1", &kit, Some("unified-diff"))).unwrap();

        let k = "DARKMUX_STEP_COMMAND_TIMEOUT_SECONDS";
        let prev = std::env::var(k).ok();
        unsafe { std::env::set_var(k, "2") };
        let started = std::time::Instant::now();
        let outcome = ModsGateStepKind
            .run(
                &step(json!({
                    "for_key": "sess-a/1",
                    "test_command": "sleep 300",
                    "workdir": tree_root.to_string_lossy(),
                })),
                &task(),
                &BTreeMap::new(),
            )
            .unwrap();
        let elapsed = started.elapsed();
        unsafe {
            match prev {
                Some(v) => std::env::set_var(k, v),
                None => std::env::remove_var(k),
            }
        }
        assert!(elapsed < std::time::Duration::from_secs(20), "the gate must return at its bound, took {elapsed:?}");
        let summary: serde_json::Value = serde_json::from_str(&outcome.output).unwrap();
        assert_eq!(summary["mods_seen"], 1);

        let rec = mods::load_at(mods_dir.path(), "mod-hang").unwrap().unwrap();
        assert!(rec.gate.is_none(), "a hung command says nothing about the kit: {rec:?}");
        assert_eq!(rec.gate_skipped_reason.as_deref(), Some("test_command exceeded 2s"));
    }

    /// (S5-6a) The scratch copy carries NO `.git`. A materialized source
    /// is a linked worktree, so its `.git` is a FILE pointing at the
    /// shared bare mirror's worktree dir — copied verbatim, the scratch
    /// becomes a live second worktree of that mirror and a `test_command`
    /// doing any git write (`git stash`, `git commit`) mutates the mirror
    /// every other unit is reading. `git apply` needs no repo (this kind
    /// already runs it as a raw patch tool under
    /// `GIT_CEILING_DIRECTORIES`), so removing it costs nothing.
    #[test]
    #[serial_test::serial] // scopes DARKMUX_MODS_DIR, a process-global
    fn the_scratch_copy_carries_no_git_and_the_kit_still_applies() {
        let mods_dir = TempDir::new().unwrap();
        let _guard = ModsDirGuard::set(mods_dir.path());
        let (_fixture, tree_root) = fixture_tree("wrong\n");
        // The exact shape a linked worktree has: `.git` is a FILE.
        std::fs::write(tree_root.join("app/.git"), "gitdir: /nowhere/.git/worktrees/app\n").unwrap();
        let kit = one_line_kit("wrong", "right");
        mods::materialize(mods_dir.path(), &a_mod_kit("mod-nogit", "sess-a/1", &kit, Some("unified-diff"))).unwrap();

        ModsGateStepKind
            .run(
                &step(json!({
                    "for_key": "sess-a/1",
                    // Passes only if the kit applied AND `.git` is gone.
                    "test_command": "grep -q right answer.txt && ! test -e .git",
                    "workdir": tree_root.to_string_lossy(),
                })),
                &task(),
                &BTreeMap::new(),
            )
            .unwrap();

        let rec = mods::load_at(mods_dir.path(), "mod-nogit").unwrap().unwrap();
        let gate = rec.gate.as_ref().unwrap_or_else(|| panic!("expected a real gate outcome: {rec:?}"));
        assert!(gate.passed, "the kit must apply with no `.git` in the scratch: {gate:?}");
        assert_eq!(gate.applied, Some(true));
        // The source checkout keeps its own `.git` — only the COPY is stripped.
        assert!(tree_root.join("app/.git").exists(), "the source checkout must be left exactly as it was");
    }

    /// (S5-6a, the consequence stated as a test) A `test_command` that
    /// does a git WRITE must not reach the shared mirror. With the
    /// worktree's `.git` copied into the scratch, the scratch IS a live
    /// second worktree of that mirror: a `git commit` inside it moves the
    /// worktree's HEAD in the mirror's own `.git/worktrees/<id>/` and
    /// writes objects into the mirror every other unit in the run is
    /// reading. Asserted against the real thing — a real `git worktree`,
    /// the real HEAD, the real worktree list.
    /// (#2387, live proof 2026-09-05) Three of four kits the coder wrote ended
    /// on the last hunk line with NO trailing newline — a JSON string does
    /// not need one, and a model does not think to add one. `git apply`
    /// reports that as "corrupt patch at line N" (N = the last line), which
    /// the gate recorded as "kit did not apply" — for kits that apply
    /// byte-for-byte once terminated. The gate owns the one place a kit
    /// reaches `git apply`, so it terminates the file there.
    #[test]
    #[serial_test::serial] // scopes DARKMUX_MODS_DIR, a process-global
    fn a_kit_without_a_trailing_newline_still_applies() {
        let mods_dir = TempDir::new().unwrap();
        let _guard = ModsDirGuard::set(mods_dir.path());
        let (_fixture, tree_root) = fixture_tree("wrong\n");
        let kit = one_line_kit("wrong", "right");
        let unterminated = kit.trim_end_matches('\n').to_string();
        assert!(!unterminated.ends_with('\n'), "the fixture must exercise the unterminated shape");
        mods::materialize(mods_dir.path(), &a_mod_kit("mod-nonl", "sess-a/1", &unterminated, Some("unified-diff")))
            .unwrap();

        ModsGateStepKind
            .run(
                &step(json!({
                    "for_key": "sess-a/1",
                    "test_command": "grep -q right answer.txt",
                    "workdir": tree_root.to_string_lossy(),
                })),
                &task(),
                &BTreeMap::new(),
            )
            .unwrap();

        let rec = mods::load_at(mods_dir.path(), "mod-nonl").unwrap().unwrap();
        let gate = rec.gate.as_ref().unwrap_or_else(|| panic!("expected a real gate outcome: {rec:?}"));
        assert_eq!(gate.applied, Some(true), "an unterminated kit must still apply: {gate:?}");
        assert!(gate.passed, "{gate:?}");
    }

    /// Runs one kit through the gate against the standard fixture and returns
    /// the recorded gate outcome. Shared by the kit-shape tests below (live
    /// proof 2026-09-05): each one is a shape a real coder seat wrote for a
    /// correct change that `git apply` refused verbatim.
    fn gate_kit(key: &str, kit: &str) -> GateOutcome {
        let mods_dir = TempDir::new().unwrap();
        let _guard = ModsDirGuard::set(mods_dir.path());
        let (_fixture, tree_root) = fixture_tree("wrong\n");
        mods::materialize(mods_dir.path(), &a_mod_kit(key, "sess-a/1", kit, Some("unified-diff"))).unwrap();
        ModsGateStepKind
            .run(
                &step(json!({
                    "for_key": "sess-a/1",
                    "test_command": "grep -q right answer.txt",
                    "workdir": tree_root.to_string_lossy(),
                })),
                &task(),
                &BTreeMap::new(),
            )
            .unwrap();
        let rec = mods::load_at(mods_dir.path(), key).unwrap().unwrap();
        rec.gate.clone().unwrap_or_else(|| panic!("expected a real gate outcome: {rec:?}"))
    }

    /// The coder explains the change in prose and puts the diff in a
    /// ```diff fence — the shape 3 of 3 pass-3 kits had. The kit is stored
    /// verbatim (the record is the artifact); the gate applies the DIFF.
    #[test]
    #[serial_test::serial]
    fn a_kit_wrapped_in_prose_and_a_diff_fence_applies() {
        let kit = format!(
            "Replace the wrong answer with the right one.\n\nFile: answer.txt\n\n```diff\n{}```\n\nThat is the whole change.\n",
            one_line_kit("wrong", "right")
        );
        let gate = gate_kit("mod-fenced", &kit);
        assert_eq!(gate.applied, Some(true), "the diff inside the fence must apply: {gate:?}");
        assert!(gate.passed, "{gate:?}");
    }

    /// A create-mod dispatch carries no `context.source`, so the kit reaches
    /// the store in container coordinates (`a/app/answer.txt`, the source id
    /// as a path prefix) with nothing having mapped it. The gate knows the
    /// source checkout it resolved, so it maps at apply time.
    #[test]
    #[serial_test::serial]
    fn a_kit_in_container_coordinates_is_mapped_at_the_gate() {
        let kit = one_line_kit("wrong", "right").replace("a/answer.txt", "a/app/answer.txt").replace("b/answer.txt", "b/app/answer.txt");
        assert!(kit.contains("--- a/app/answer.txt"), "{kit}");
        let gate = gate_kit("mod-container", &kit);
        assert_eq!(gate.applied, Some(true), "the source-prefixed kit must apply: {gate:?}");
        assert!(gate.passed, "{gate:?}");
    }

    /// Models miscount hunk headers (`@@ -1,7 +1,5 @@` over an 8/9-line body);
    /// git calls that "corrupt patch". The body is right and `--recount`
    /// applies it; a kit whose CONTEXT is wrong still fails honestly.
    #[test]
    #[serial_test::serial]
    fn a_kit_with_miscounted_hunk_headers_still_applies() {
        let kit = one_line_kit("wrong", "right").replace("@@ -1 +1 @@", "@@ -1,3 +1,4 @@");
        let gate = gate_kit("mod-recount", &kit);
        assert_eq!(gate.applied, Some(true), "a miscounted header must not sink a correct body: {gate:?}");
        assert!(gate.passed, "{gate:?}");
    }

    #[test]
    #[serial_test::serial]
    fn a_kit_whose_context_does_not_match_still_fails_honestly() {
        let kit = one_line_kit("something-else", "right");
        let gate = gate_kit("mod-badctx", &kit);
        assert_eq!(gate.applied, Some(false), "{gate:?}");
        assert_eq!(gate.reason.as_deref(), Some("kit did not apply"));
    }

    /// The gate resolved the source checkout to apply the kit; a mod that
    /// arrived with no `source` (every create-mod dispatch) gets that id
    /// recorded, so the deliverer can map the kit the same way the gate did.
    #[test]
    #[serial_test::serial]
    fn the_gate_records_the_source_id_it_resolved_on_a_sourceless_mod() {
        let mods_dir = TempDir::new().unwrap();
        let _guard = ModsDirGuard::set(mods_dir.path());
        let (_fixture, tree_root) = fixture_tree("wrong\n");
        let kit = one_line_kit("wrong", "right").replace("a/answer.txt", "a/app/answer.txt").replace("b/answer.txt", "b/app/answer.txt");
        mods::materialize(mods_dir.path(), &a_mod_kit("mod-src", "sess-a/1", &kit, Some("unified-diff"))).unwrap();
        ModsGateStepKind
            .run(
                &step(json!({ "for_key": "sess-a/1", "test_command": "true", "workdir": tree_root.to_string_lossy() })),
                &task(),
                &BTreeMap::new(),
            )
            .unwrap();
        let rec = mods::load_at(mods_dir.path(), "mod-src").unwrap().unwrap();
        assert_eq!(rec.source.as_deref(), Some("app"), "the resolved source id must be recorded: {rec:?}");
        assert_eq!(rec.gate.as_ref().map(|g| g.applied), Some(Some(true)));
    }

    #[test]
    #[serial_test::serial] // scopes DARKMUX_MODS_DIR, a process-global
    fn a_gate_command_doing_git_writes_cannot_reach_the_shared_mirror() {
        fn git(cwd: &Path, args: &[&str]) -> std::process::Output {
            std::process::Command::new("git")
                .current_dir(cwd)
                .args(args)
                .output()
                .expect("git must be available")
        }
        let mods_dir = TempDir::new().unwrap();
        let _guard = ModsDirGuard::set(mods_dir.path());
        let home = TempDir::new().unwrap();
        // A real repo, and a real linked worktree at <tree_root>/app —
        // exactly the shape `workspace_spec::materialize` produces.
        let repo = home.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        git(&repo, &["init", "-q", "-b", "main"]);
        std::fs::write(repo.join("answer.txt"), "wrong\n").unwrap();
        git(&repo, &["add", "answer.txt"]);
        git(&repo, &["-c", "user.email=t@e", "-c", "user.name=t", "commit", "-q", "-m", "seed"]);
        let tree_root = home.path().join("tree");
        std::fs::create_dir_all(&tree_root).unwrap();
        let checkout = tree_root.join("app");
        git(&repo, &["worktree", "add", "-q", "--detach", &checkout.to_string_lossy()]);
        assert!(checkout.join(".git").is_file(), "a linked worktree's `.git` is a FILE");

        let head_before = git(&checkout, &["rev-parse", "HEAD"]).stdout;
        let worktrees_before = git(&repo, &["worktree", "list"]).stdout;
        let log_before = git(&repo, &["log", "--oneline", "--all"]).stdout;

        let kit = one_line_kit("wrong", "right");
        mods::materialize(mods_dir.path(), &a_mod_kit("mod-git", "sess-a/1", &kit, Some("unified-diff"))).unwrap();
        ModsGateStepKind
            .run(
                &step(json!({
                    "for_key": "sess-a/1",
                    // The hostile shape: a kit's test target that commits.
                    "test_command": "git -c user.email=t@e -c user.name=t commit -q -am pwned; true",
                    "workdir": tree_root.to_string_lossy(),
                })),
                &task(),
                &BTreeMap::new(),
            )
            .unwrap();

        assert_eq!(git(&checkout, &["rev-parse", "HEAD"]).stdout, head_before, "the worktree's HEAD moved");
        assert_eq!(git(&repo, &["worktree", "list"]).stdout, worktrees_before, "the mirror's worktree list changed");
        assert_eq!(git(&repo, &["log", "--oneline", "--all"]).stdout, log_before, "a commit reached the mirror");
        assert_eq!(
            std::fs::read_to_string(checkout.join("answer.txt")).unwrap(),
            "wrong\n",
            "the checkout itself must be untouched"
        );
    }

    /// (#2310 P4c-2b PR #2357 review MUST FIX A, proven) The inverse
    /// probe: baseline PASSES, the mod's kit BREAKS it. A wrong mod must
    /// not read as confirmed.
    #[test]
    #[serial_test::serial] // scopes DARKMUX_MODS_DIR, a process-global
    fn a_kit_that_breaks_a_planted_passing_test_gates_failed() {
        let mods_dir = TempDir::new().unwrap();
        let _guard = ModsDirGuard::set(mods_dir.path());
        let (_fixture, tree_root) = fixture_tree("right\n");
        let kit = one_line_kit("right", "wrong");
        mods::materialize(mods_dir.path(), &a_mod_kit("mod-break", "sess-a/2", &kit, Some("unified-diff"))).unwrap();

        ModsGateStepKind
            .run(
                &step(json!({
                    "for_key": "sess-a/2",
                    "test_command": "grep -q right answer.txt",
                    "workdir": tree_root.to_string_lossy(),
                })),
                &task(),
                &BTreeMap::new(),
            )
            .unwrap();

        let rec = mods::load_at(mods_dir.path(), "mod-break").unwrap().unwrap();
        let gate = rec.gate.as_ref().unwrap();
        assert!(!gate.passed, "{gate:?}");
        assert_eq!(gate.applied, Some(true), "the kit DID apply — it just made the test fail");
        assert!(gate.reason.is_none(), "an ordinary test-exit failure carries no `reason`: {gate:?}");
    }

    /// (#2310 P4c-2b PR #2357 review MUST FIX A, proven) A kit that does
    /// not apply at all — `passed: false`, `applied: false`, `reason:
    /// "kit did not apply"` — a DATA outcome, never a skip and never a
    /// step error.
    #[test]
    #[serial_test::serial] // scopes DARKMUX_MODS_DIR, a process-global
    fn a_non_applying_kit_gates_failed_with_the_reason() {
        let mods_dir = TempDir::new().unwrap();
        let _guard = ModsDirGuard::set(mods_dir.path());
        let (_fixture, tree_root) = fixture_tree("right\n");
        // Names lines that do not exist in `answer.txt` at all.
        let bogus_kit = one_line_kit("this text is not in the file", "neither is this");
        mods::materialize(mods_dir.path(), &a_mod_kit("mod-bogus", "sess-a/3", &bogus_kit, Some("unified-diff")))
            .unwrap();

        let result = ModsGateStepKind.run(
            &step(json!({
                "for_key": "sess-a/3",
                "test_command": "grep -q right answer.txt",
                "workdir": tree_root.to_string_lossy(),
            })),
            &task(),
            &BTreeMap::new(),
        );
        assert!(result.is_ok(), "a non-applying kit is DATA, never a step failure: {result:?}");

        let rec = mods::load_at(mods_dir.path(), "mod-bogus").unwrap().unwrap();
        let gate = rec.gate.as_ref().unwrap();
        assert!(!gate.passed);
        assert_eq!(gate.applied, Some(false));
        assert_eq!(gate.reason.as_deref(), Some("kit did not apply"));
        assert!(rec.gate_skipped_reason.is_none(), "a non-applying kit is a GATE outcome, never a skip: {rec:?}");
    }

    /// (#2310 P4c-2b PR #2357 review) A mod whose kit isn't declared a
    /// unified diff has nothing this kind can mechanically apply — a
    /// named skip, never a fabricated pass/fail.
    #[test]
    #[serial_test::serial] // scopes DARKMUX_MODS_DIR, a process-global
    fn a_kit_kind_that_is_not_unified_diff_is_skipped_as_no_applicable_kit() {
        let mods_dir = TempDir::new().unwrap();
        let _guard = ModsDirGuard::set(mods_dir.path());
        let (_fixture, tree_root) = fixture_tree("right\n");
        mods::materialize(mods_dir.path(), &a_mod_kit("mod-opaque", "sess-a/4", "just apply this by hand", None))
            .unwrap();

        ModsGateStepKind
            .run(
                &step(json!({
                    "for_key": "sess-a/4",
                    "test_command": "grep -q right answer.txt",
                    "workdir": tree_root.to_string_lossy(),
                })),
                &task(),
                &BTreeMap::new(),
            )
            .unwrap();

        let rec = mods::load_at(mods_dir.path(), "mod-opaque").unwrap().unwrap();
        assert!(rec.gate.is_none());
        assert_eq!(rec.gate_skipped_reason.as_deref(), Some("no applicable kit"));
    }

    /// (#2310 P4c-2b PR #2357 review MUST FIX B, proven) An infra-level
    /// failure — `workdir` naming a directory that does not exist —
    /// records a SKIP, never `passed: false`.
    #[test]
    #[serial_test::serial] // scopes DARKMUX_MODS_DIR, a process-global
    fn a_missing_workdir_is_skipped_never_passed_false() {
        let mods_dir = TempDir::new().unwrap();
        let _guard = ModsDirGuard::set(mods_dir.path());
        let kit = one_line_kit("wrong", "right");
        mods::materialize(mods_dir.path(), &a_mod_kit("mod-nowd", "sess-a/5", &kit, Some("unified-diff"))).unwrap();

        ModsGateStepKind
            .run(
                &step(json!({
                    "for_key": "sess-a/5",
                    "test_command": "grep -q right answer.txt",
                    "workdir": "/definitely/does/not/exist/anywhere",
                })),
                &task(),
                &BTreeMap::new(),
            )
            .unwrap();

        let rec = mods::load_at(mods_dir.path(), "mod-nowd").unwrap().unwrap();
        assert!(rec.gate.is_none(), "a missing workdir must never fabricate a passed:false outcome: {rec:?}");
        assert!(rec.gate_skipped_reason.as_deref().unwrap().contains("does not exist"), "{rec:?}");
    }

    /// Scopes the thread-local `SCRATCH_BASE_OVERRIDE` (never `$TMPDIR` —
    /// see that thread-local's own doc for why a process env var would
    /// race an UNRELATED, concurrently-running test) so a test can force
    /// `gate_one_mod`'s scratch dir to be created inside a throwaway git
    /// repo. Safe without any `#[serial_test::serial]` at all: each
    /// `#[test]` fn gets its own OS thread from `cargo test`'s default
    /// harness, so this thread-local can never leak into another test.
    struct ScratchBaseGuard;
    impl ScratchBaseGuard {
        fn set(p: &std::path::Path) -> Self {
            SCRATCH_BASE_OVERRIDE.with(|cell| *cell.borrow_mut() = Some(p.to_path_buf()));
            Self
        }
    }
    impl Drop for ScratchBaseGuard {
        fn drop(&mut self) {
            SCRATCH_BASE_OVERRIDE.with(|cell| *cell.borrow_mut() = None);
        }
    }

    /// (#2310 P4c-2b PR #2357 round-2 review item 2, proven live) Manual
    /// repro: the SAME `git apply <patch>` invocation, same cwd, run once
    /// with no env override and once with `GIT_CEILING_DIRECTORIES` set —
    /// both exit 0, but ONLY the ceiling'd run actually changes the file.
    /// Without `GIT_CEILING_DIRECTORIES` (and now the fingerprint
    /// backstop), a scratch checkout nested inside an ambient git repo
    /// (a REAL risk: `tempfile::TempDir::new()` uses `$TMPDIR`, not
    /// guaranteed to sit outside every repo on disk) would report
    /// `applied: true` while the checkout was never actually patched —
    /// the exact false-positive this test forces by pointing the scratch
    /// dir's base at a throwaway `git init`'d directory.
    #[test]
    #[serial_test::serial] // scopes DARKMUX_MODS_DIR, a process-global
    fn a_scratch_dir_inside_an_ambient_git_repo_still_gates_on_a_real_change() {
        let mods_dir = TempDir::new().unwrap();
        let _guard = ModsDirGuard::set(mods_dir.path());

        let ambient_repo = TempDir::new().unwrap();
        let init = std::process::Command::new("git").current_dir(ambient_repo.path()).args(["init", "-q"]).output();
        if init.as_ref().map(|o| !o.status.success()).unwrap_or(true) {
            eprintln!("skipping: `git init` unavailable in this environment");
            return;
        }
        // Force `gate_one_mod`'s own scratch dir to be created INSIDE the
        // ambient repo for this test only (thread-local — see the guard's
        // own doc for why this is safe against concurrent tests).
        let _scratch_base_guard = ScratchBaseGuard::set(ambient_repo.path());

        let (_fixture, tree_root) = fixture_tree("wrong\n");
        let kit = one_line_kit("wrong", "right");
        mods::materialize(mods_dir.path(), &a_mod_kit("mod-ambient", "sess-a/9", &kit, Some("unified-diff"))).unwrap();

        ModsGateStepKind
            .run(
                &step(json!({
                    "for_key": "sess-a/9",
                    "test_command": "grep -q right answer.txt",
                    "workdir": tree_root.to_string_lossy(),
                })),
                &task(),
                &BTreeMap::new(),
            )
            .unwrap();

        let rec = mods::load_at(mods_dir.path(), "mod-ambient").unwrap().unwrap();
        let gate = rec.gate.as_ref().unwrap_or_else(|| panic!("expected a real gate outcome: {rec:?}"));
        assert!(gate.passed, "the checkout must have been genuinely patched and tested: {gate:?}");
        assert_eq!(gate.applied, Some(true));
        assert!(gate.reason.is_none(), "{gate:?}");
    }

    #[test]
    #[serial_test::serial] // scopes DARKMUX_MODS_DIR, a process-global
    fn a_mod_already_gated_is_left_untouched_on_a_second_pass() {
        let tmp = TempDir::new().unwrap();
        let _guard = ModsDirGuard::set(tmp.path());
        mods::materialize(tmp.path(), &a_mod("mod-4", "sess-a/4")).unwrap();
        mods::record_gate(
            tmp.path(),
            "mod-4",
            Some(GateOutcome { passed: true, command: "original".into(), exit_code: Some(0), applied: Some(true), reason: None }),
            None,
        )
        .unwrap();

        let outcome = ModsGateStepKind
            .run(&step(json!({ "for_key": "sess-a/4", "test_command": "false" })), &task(), &BTreeMap::new())
            .unwrap();
        let summary: serde_json::Value = serde_json::from_str(&outcome.output).unwrap();
        assert_eq!(summary["mods_gated"], 0, "already gated — the second pass changes nothing");

        let rec = mods::load_at(tmp.path(), "mod-4").unwrap().unwrap();
        assert_eq!(rec.gate.as_ref().unwrap().command, "original", "the original gate result survives untouched");
    }

    #[test]
    #[serial_test::serial] // scopes DARKMUX_MODS_DIR, a process-global
    fn no_matching_mods_is_not_an_error() {
        let tmp = TempDir::new().unwrap();
        let _guard = ModsDirGuard::set(tmp.path());
        let outcome =
            ModsGateStepKind.run(&step(json!({ "for_key": "sess-a/nope" })), &task(), &BTreeMap::new()).unwrap();
        let summary: serde_json::Value = serde_json::from_str(&outcome.output).unwrap();
        assert_eq!(summary["mods_seen"], 0);
        assert_eq!(summary["mods_gated"], 0);
    }

    #[test]
    #[serial_test::serial] // scopes DARKMUX_MODS_DIR, a process-global
    fn a_missing_for_key_is_refused_by_name() {
        let err = ModsGateStepKind.run(&step(json!({})), &task(), &BTreeMap::new()).unwrap_err();
        assert!(err.to_string().contains("for_key"), "{err}");
    }

    #[test]
    #[serial_test::serial] // scopes DARKMUX_MODS_DIR, a process-global
    fn the_kind_registers_via_its_own_dedicated_function() {
        let registry = StepKindRegistry::new();
        register_mods_gate_kind(&registry).unwrap();
        assert!(registry.ids().iter().any(|id| id == MODS_GATE_KIND));
    }
}
