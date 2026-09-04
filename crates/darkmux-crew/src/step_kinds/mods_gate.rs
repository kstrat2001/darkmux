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
//! malformed config) fails the step itself.

use crate::mods::{self, GateOutcome, ModRecord};
use crate::step_kinds::registry::StepKindRegistry;
use crate::step_kinds::types::{StepKind, StepOutcome};
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
            let res = mods::record_gate(&root, &m.key, outcome, skip_reason.as_deref())
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
    let scratch = match tempfile::TempDir::new() {
        Ok(t) => t,
        Err(e) => return (None, Some(format!("could not create a scratch dir: {e}"))),
    };
    let scratch_checkout = scratch.path().join("checkout");
    if let Err(e) = copy_dir_recursive(&source_dir, &scratch_checkout) {
        return (None, Some(format!("could not copy the source checkout: {e}")));
    }
    let patch_path = scratch.path().join("mod.patch");
    if let Err(e) = std::fs::write(&patch_path, kit) {
        return (None, Some(format!("could not write the mod's kit to a scratch file: {e}")));
    }

    // `git apply --check` first — a dry run that never touches the
    // checkout, so a kit that fails to apply is detected before anything
    // is mutated.
    let check = std::process::Command::new("git")
        .current_dir(&scratch_checkout)
        .args(["apply", "--check", &patch_path.to_string_lossy()])
        .output();
    let applied_cleanly = match check {
        Ok(out) => out.status.success(),
        Err(e) => return (None, Some(format!("could not run `git apply --check`: {e}"))),
    };
    if !applied_cleanly {
        // (#2310 P4c-2b PR #2357 review MUST FIX A) A kit that does not
        // apply is DATA about the mod, not an infrastructure failure —
        // `passed: false`, `applied: false`, never a skip.
        return (
            Some(GateOutcome {
                passed: false,
                command: command.to_string(),
                exit_code: None,
                applied: Some(false),
                reason: Some("kit did not apply".to_string()),
            }),
            None,
        );
    }
    let apply = std::process::Command::new("git")
        .current_dir(&scratch_checkout)
        .args(["apply", &patch_path.to_string_lossy()])
        .output();
    match apply {
        Ok(out) if out.status.success() => {}
        // `--check` passed but the real apply failed anyway (rare —
        // a race on the scratch dir, a disk error) — same "kit did not
        // apply" data outcome, not a skip: the reviewer asked for the
        // kit's OWN applicability to be the signal, and this IS that.
        Ok(_) | Err(_) => {
            return (
                Some(GateOutcome {
                    passed: false,
                    command: command.to_string(),
                    exit_code: None,
                    applied: Some(false),
                    reason: Some("kit did not apply".to_string()),
                }),
                None,
            )
        }
    }

    // (#2310 P4c-2b PR #2357 review MUST FIX B) `test_command` runs
    // INSIDE the checkout, never `workdir`/`tree_root` — the whole point
    // of the fix. A command that cannot even be SPAWNED (a missing
    // interpreter, a workdir that vanished) is an infra skip; a command
    // that runs and exits nonzero is a real, data `passed: false`.
    let mut test_cmd = std::process::Command::new("sh");
    test_cmd.arg("-c").arg(command).current_dir(&scratch_checkout);
    match test_cmd.output() {
        Ok(out) => (
            Some(GateOutcome {
                passed: out.status.success(),
                command: command.to_string(),
                exit_code: out.status.code(),
                applied: Some(true),
                reason: None,
            }),
            None,
        ),
        Err(e) => (None, Some(format!("could not run the gate command in {}: {e}", scratch_checkout.display()))),
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
