//! `darkmux lab run <workload> [opts]` — execute a workload and capture output.

use crate::lab::artifact_dirs;
use crate::lab::cow_clone::cow_clone_dir_excluding;
use crate::lab::paths::{self, ResolveScope};
use crate::lab::sandbox_hash::hash_sandbox_dir;
use darkmux_profiles::profiles::{get_profile, load_registry};
use crate::workloads::load::{list_available, load};
use crate::workloads::registry::with_provider;
use anyhow::{anyhow, Context, Result};
use std::fs;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

pub struct RunOpts {
    pub workload_id: String,
    pub profile_name: Option<String>,
    pub runs: u32,
    pub config_path: Option<String>,
    pub quiet: bool,
    /// Which agent runtime to dispatch the workload through.
    /// `Runtime::Internal` (default) uses darkmux's container-bounded
    /// runtime; `Runtime::Openclaw` shells out to the openclaw CLI
    /// (legacy path).
    pub runtime: darkmux_crew::dispatch::Runtime,
    /// Executable path for the openclaw shell-out (Sprint-E
    /// replacement for the removed `DARKMUX_RUNTIME_CMD` env var).
    /// Defaults to `"openclaw"`; override via `--runtime-cmd <path>`
    /// to point at Aider / Cline / any tool exposing the
    /// `<cmd> agent --message` calling convention. Ignored when
    /// `runtime == Runtime::Internal`.
    pub runtime_cmd: String,
}

/// `run_dir` is the canonical path to the run's output directory.
/// Public-API surface — downstream tools (notebook drafting, viewer
/// loading) read it after `lab run` completes. The CLI itself prints
/// `run_id` and not the full path, hence the dead-code lint.
#[allow(dead_code)]
#[derive(Debug, Clone)]
pub struct RunOutcome {
    pub run_id: String,
    pub run_dir: std::path::PathBuf,
    /// Did the dispatch itself complete successfully (runtime exit 0, reply
    /// payload received)? Distinct from `verify_passed` — a dispatch can
    /// succeed but its reply may not pass keyword/test verification.
    pub ok: bool,
    /// Did the workload's verify spec pass? `None` if no verify was defined.
    pub verify_passed: Option<bool>,
    pub duration_ms: u128,
    pub notes: Vec<String>,
}

pub fn lab_run(opts: RunOpts) -> Result<Vec<RunOutcome>> {
    let paths = paths::resolve(ResolveScope::Auto);
    paths::ensure(&paths)?;

    let user_dir = if paths.scope == paths::Scope::Project || paths.scope == paths::Scope::User {
        Some(paths.root.as_path())
    } else {
        None
    };
    let loaded_workload = load(&opts.workload_id, user_dir)?;

    let registry_loaded = load_registry(opts.config_path.as_deref())?;
    let profile_name = opts
        .profile_name
        .clone()
        .or_else(|| registry_loaded.registry.default_profile.clone())
        .ok_or_else(|| anyhow!("no profile specified and no default_profile in registry"))?;
    let profile = get_profile(&registry_loaded.registry, &profile_name)?;

    // (#365/#544) Best-effort provenance guard: if the operator swapped a
    // different profile before this dispatch (or the default_profile
    // doesn't match what's loaded), the manifest's `profile=` tag would
    // silently misattribute the runtime envelope. Compare the requested
    // profile's declared models against `lms ps` and warn (never block —
    // operator-sovereignty: the operator may have swapped deliberately).
    // The check runs per-run inside the loop below: with `--runs N` the
    // loaded model can drift between runs (LMStudio eviction under memory
    // pressure), and each run is independently stamped `profile=<name>`.
    // `prev_envelope_warns` dedups a stable picture so a persistent
    // mismatch warns once, not once-per-run.
    let mut prev_envelope_warns: Option<Vec<String>> = None;

    let runs = opts.runs.max(1);
    let mut outcomes: Vec<RunOutcome> = Vec::new();

    for i in 1..=runs {
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let run_id = format!("{}-{}-{}-{}", opts.workload_id, profile_name, stamp, i);
        let run_dir = paths.runs.join(&run_id);
        // (#488 Phase 1 / #490 Phase 3) The workload's *source* sandbox
        // is what gets COW-cloned per run. Phase 3 resolution shape:
        //   1. If workload declares `requires_fixture: <name@version>`,
        //      consult the lab registry for a fixture satisfying that
        //      requirement → use its path.
        //   2. Else fall back to `{paths.sandboxes}/<workload-id>/`
        //      (the convention for workloads with setupContent or no
        //      external dependency).
        // No env-var fallback per the no-compat-baggage-pre-1.0 doctrine.
        let source_sandbox_dir =
            resolve_source_sandbox(&loaded_workload, &paths).with_context(|| {
                format!(
                    "resolving source sandbox for workload `{}`",
                    opts.workload_id
                )
            })?;
        // (#488) Phase 1 — the per-run sandbox lives UNDER the per-run
        // dir, isolated from every other run's edits. Each run starts
        // either as a COW clone of the source sandbox (if it exists)
        // OR as a fresh empty dir that the provider's setup() will
        // populate (workloads with setupContent).
        let per_run_sandbox_dir = run_dir.join("sandbox");

        if !opts.quiet {
            println!(
                "[lab] run {i}/{runs} — workload={} profile={} → {}",
                opts.workload_id, profile_name, run_id
            );

            // (#365/#544) Per-run envelope check. An `lms ps` failure is
            // surfaced distinctly (verification didn't run) rather than
            // silently skipped — methodology citations depend on knowing
            // the verification status.
            let warns = match darkmux_profiles::lms::list_loaded() {
                Ok(loaded) => {
                    crate::lab::profile_check::envelope_warnings(profile, &profile_name, &loaded)
                }
                Err(e) => vec![format!(
                    "could not verify profile-load match — `lms ps` failed ({e}); \
                     this run's `profile={profile_name}` tag is unverified. (#365)"
                )],
            };
            if prev_envelope_warns.as_ref() != Some(&warns) {
                for w in &warns {
                    eprintln!("[lab] warn: {w}");
                }
                prev_envelope_warns = Some(warns);
            }
        }

        fs::create_dir_all(&run_dir).with_context(|| format!("creating {}", run_dir.display()))?;

        // (#488) Phase 1 — materialize the per-run sandbox. If the
        // source exists, COW-clone it (cheap on APFS/btrfs/xfs;
        // fallback to deep copy elsewhere). If not, create an empty
        // dir for the provider's setup() to populate. This is the
        // load-bearing isolation: subsequent runs get fresh sandboxes
        // and never observe prior runs' edits.
        //
        // (#489) Phase 2 — compute baseline_hash, then (#496) hash the
        // per-run sandbox AFTER the COW clone rather than the source
        // before it. The COW copy is byte-identical, so the recorded
        // value is unchanged — but hashing the clone closes the race
        // window where a concurrent writer could mutate the source
        // between the hash and the clone, leaving baseline_hash not
        // matching what the clone actually copied. The per-run sandbox
        // is private to this run, so nothing else touches it between the
        // clone and the hash. Best-effort: skip silently for
        // self-contained workloads (no source yet) — the provider's
        // setup() populates the empty dir; baseline_hash stays None.
        let baseline_hash: Option<String> = if source_sandbox_dir.exists() {
            // Prune run-artifact dirs (.darkmux-runtime, coverage, .git,
            // …) from the clone so a stale dropping in a fixture source
            // can't contaminate this run. node_modules is deliberately
            // NOT in RUN_ARTIFACT_DIRS — the in-sandbox tests need it; the
            // hash drops it separately via HASH_ONLY_EXCLUDES. Because the
            // baseline_hash below runs on this now-pruned clone, the
            // run-path baseline is clean for free. (lab-contamination fix)
            cow_clone_dir_excluding(
                &source_sandbox_dir,
                &per_run_sandbox_dir,
                artifact_dirs::RUN_ARTIFACT_DIRS,
            )
            .with_context(|| {
                format!(
                    "cow-cloning source sandbox {} → {}",
                    source_sandbox_dir.display(),
                    per_run_sandbox_dir.display()
                )
            })?;
            match hash_sandbox_dir(&per_run_sandbox_dir) {
                Ok(h) => Some(h),
                Err(e) => {
                    if !opts.quiet {
                        eprintln!(
                            "[lab] warn: baseline_hash for {} skipped: {e}",
                            per_run_sandbox_dir.display()
                        );
                    }
                    None
                }
            }
        } else {
            fs::create_dir_all(&per_run_sandbox_dir).with_context(|| {
                format!("creating empty per-run sandbox {}", per_run_sandbox_dir.display())
            })?;
            None
        };

        let provider_id = loaded_workload.manifest.workload.provider.clone();
        let runtime = opts.runtime;
        let runtime_cmd = opts.runtime_cmd.as_str();
        // (#488) Phase 1 — provider operates against the per-run
        // sandbox, not the source. Provider has no awareness of the
        // COW step; it just gets a sandbox dir and works against it.
        let result = with_provider(&provider_id, |p| {
            p.setup(&loaded_workload, &run_dir, &per_run_sandbox_dir)?;
            p.run(
                &loaded_workload,
                &run_dir,
                &per_run_sandbox_dir,
                profile,
                &profile_name,
                runtime,
                runtime_cmd,
            )
        })??;

        // (#489) Phase 2 — enrich the provider-written manifest.json
        // with fixture provenance (baseline_hash + source_fixture_path).
        // Provider's manifest stays workload/runtime-focused; lab adds
        // the cross-cutting fixture-integrity fields. Best-effort: a
        // missing or malformed manifest is logged but doesn't fail the
        // run (observability data, not correctness).
        if let Err(e) = enrich_manifest_with_fixture_info(
            &run_dir,
            baseline_hash.as_deref(),
            &source_sandbox_dir,
        ) {
            if !opts.quiet {
                eprintln!(
                    "[lab] warn: enriching manifest with fixture info skipped: {e}"
                );
            }
        }

        let mut notes = vec![
            format!("provider={}", provider_id),
            format!("wall={}s", result.duration_ms / 1000),
            if result.ok {
                "ok".to_string()
            } else {
                format!("error: {}", result.error.as_deref().unwrap_or("unknown"))
            },
        ];
        if let Some(v) = result.verify.as_ref() {
            notes.push(format!(
                "verify={} ({})",
                if v.passed { "pass" } else { "fail" },
                v.details
            ));
        }

        if !opts.quiet {
            println!("  {}", notes.join(" | "));
        }

        outcomes.push(RunOutcome {
            run_id,
            run_dir,
            ok: result.ok,
            verify_passed: result.verify.as_ref().map(|v| v.passed),
            duration_ms: result.duration_ms,
            notes,
        });
    }

    Ok(outcomes)
}

pub fn lab_workloads() -> Vec<String> {
    let paths = paths::resolve(ResolveScope::Auto);
    list_available(Some(&paths.root))
}

/// (#489) Phase 2 — read the provider-written `<run_dir>/manifest.json`,
/// merge in a `fixture` section carrying baseline_hash + source path,
/// bump schemaVersion to 4, write it back. Provider stays unaware of
/// fixture provenance; lab is the orchestration layer that knows what
/// source fed the COW clone.
///
/// Best-effort: errors are returned (caller decides how loudly to log)
/// but never abort the dispatch — fixture metadata is observability,
/// not correctness.
fn enrich_manifest_with_fixture_info(
    run_dir: &Path,
    baseline_hash: Option<&str>,
    source_sandbox_dir: &Path,
) -> Result<()> {
    let manifest_path = run_dir.join("manifest.json");
    if !manifest_path.exists() {
        return Err(anyhow!("manifest.json not present at {}", manifest_path.display()));
    }
    let raw = fs::read_to_string(&manifest_path)
        .with_context(|| format!("reading {}", manifest_path.display()))?;
    let mut manifest: serde_json::Value = serde_json::from_str(&raw)
        .with_context(|| format!("parsing {} as JSON", manifest_path.display()))?;

    // The fixture object names what the model started from. Phase 3
    // will add `name` + `satisfies` + `manifest_version` once the
    // registry resolver is wired in.
    //
    // (#496) source_path semantics for `dm lab compare`'s cross-run
    // string-equality:
    //   - source exists → canonicalized absolute path (stable across
    //     runs; the rare canonicalize failure on an existing dir, e.g.
    //     a permissions quirk, falls back to the raw path).
    //   - source does NOT exist (self-contained workload populated by
    //     the provider's setup()) → JSON `null`, an explicit "no
    //     source" signal rather than a non-canonical raw path that
    //     would spuriously mismatch a canonicalized run.
    let source_path = if source_sandbox_dir.exists() {
        let p = source_sandbox_dir
            .canonicalize()
            .unwrap_or_else(|_| source_sandbox_dir.to_path_buf());
        serde_json::Value::String(p.display().to_string())
    } else {
        serde_json::Value::Null
    };
    let fixture = serde_json::json!({
        "source_path": source_path,
        "baseline_hash": baseline_hash,
    });

    if let Some(obj) = manifest.as_object_mut() {
        obj.insert("fixture".to_string(), fixture);
        obj.insert("schemaVersion".to_string(), serde_json::json!(4));
    } else {
        return Err(anyhow!("manifest is not a JSON object"));
    }

    fs::write(&manifest_path, serde_json::to_string_pretty(&manifest)?)
        .with_context(|| format!("writing {}", manifest_path.display()))?;
    Ok(())
}

/// (#490) Phase 3 — resolve the source sandbox for a workload.
///
/// Resolution order:
///   1. If `workload.requires_fixture` is set, look up a registered
///      fixture satisfying it via the lab registry. If found, use that
///      fixture's path as the source. If no fixture satisfies, return
///      an operator-actionable error pointing at `dm lab register`.
///   2. Otherwise fall back to `{paths.sandboxes}/<workload-id>/`
///      (the default location for workloads with `setupContent` or
///      no external dependency).
///
/// The pre-#490 `DARKMUX_SANDBOX_<WORKLOAD-ID>` env-var path has been
/// removed cleanly per the `no-compat-baggage-pre-1.0` doctrine. The
/// fixture registry (Phase 2 + 4) is the only persistent binding.
pub(crate) fn resolve_source_sandbox(
    loaded: &crate::workloads::types::LoadedWorkload,
    paths: &paths::DarkmuxPaths,
) -> Result<std::path::PathBuf> {
    if let Some(requires) = &loaded.manifest.workload.requires_fixture {
        let reg_path = crate::lab::registry::default_registry_path(paths);
        let registry = crate::lab::registry::LabRegistry::load(&reg_path)
            .with_context(|| format!("loading {}", reg_path.display()))?;
        match registry.find_satisfying(requires) {
            Some((_name, fixture)) => Ok(fixture.path.clone()),
            None => Err(anyhow!(
                "workload `{}` requires a fixture satisfying `{}` but no registered \
                 fixture matches.\n\
                 \n\
                 Fix:\n\
                   1. Register an existing fixture that satisfies this requirement:\n\
                      darkmux lab register /path/to/your/fixture\n\
                   2. Or inspect what's registered:\n\
                      darkmux lab fixtures\n\
                   3. Or update the fixture's `.fixture.json` to set:\n\
                      \"satisfies\": \"{}\"",
                loaded.manifest.workload.id,
                requires,
                requires,
            )),
        }
    } else {
        Ok(paths.sandboxes.join(&loaded.manifest.workload.id))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn workloads_returns_strings_without_panicking() {
        // Just verify the function doesn't panic on a fresh user dir.
        let _ = lab_workloads();
    }

    // (#488) Phase 1 — per-run COW sandbox isolation invariants. These
    // tests exercise the lab/run.rs orchestration directly (not via
    // provider dispatch) so they don't require a live runtime / docker /
    // LMStudio. The provider-side `sandbox` field substitution is
    // tested elsewhere in coding_task.rs's test module.

    /// Two consecutive runs against the same workload must produce
    /// two distinct per-run sandbox dirs, each independent of the
    /// other. This is the load-bearing isolation that eliminates
    /// the cross-run contamination surfaced in Beat 55.
    ///
    /// Test-only flow: we invoke `cow_clone_dir` + `per_run_sandbox`
    /// construction directly, simulating what `lab_run` does without
    /// requiring a working provider/runtime stack.
    #[test]
    fn per_run_sandbox_dirs_are_isolated() {
        use crate::lab::cow_clone::cow_clone_dir;

        let tmp = TempDir::new().unwrap();
        // Simulate a source sandbox (what a workload's
        // resolve_sandbox_dir() would return).
        let source_sandbox = tmp.path().join("source-sandbox");
        std::fs::create_dir_all(&source_sandbox).unwrap();
        std::fs::write(source_sandbox.join("baseline.txt"), "baseline").unwrap();
        std::fs::create_dir_all(source_sandbox.join("tests")).unwrap();
        std::fs::write(source_sandbox.join("tests/a.test.js"), "test('a')").unwrap();

        // Simulate two per-run dirs (what lab_run loops produce).
        let runs_dir = tmp.path().join("runs");
        std::fs::create_dir_all(&runs_dir).unwrap();
        let run_1_dir = runs_dir.join("run-1");
        let run_2_dir = runs_dir.join("run-2");
        std::fs::create_dir_all(&run_1_dir).unwrap();
        std::fs::create_dir_all(&run_2_dir).unwrap();

        let run_1_sandbox = run_1_dir.join("sandbox");
        let run_2_sandbox = run_2_dir.join("sandbox");

        cow_clone_dir(&source_sandbox, &run_1_sandbox).unwrap();
        cow_clone_dir(&source_sandbox, &run_2_sandbox).unwrap();

        // Each per-run sandbox has the baseline content.
        assert_eq!(
            std::fs::read_to_string(run_1_sandbox.join("baseline.txt")).unwrap(),
            "baseline"
        );
        assert_eq!(
            std::fs::read_to_string(run_2_sandbox.join("baseline.txt")).unwrap(),
            "baseline"
        );

        // Mutate run 1 — it should NOT affect run 2 OR the source.
        std::fs::write(run_1_sandbox.join("baseline.txt"), "run-1-edit").unwrap();
        std::fs::write(run_1_sandbox.join("tests/a.test.js"), "test('a-modified')").unwrap();

        // Run 2 still has the original baseline.
        assert_eq!(
            std::fs::read_to_string(run_2_sandbox.join("baseline.txt")).unwrap(),
            "baseline",
            "run 2's sandbox got run 1's edit — isolation broken"
        );
        assert_eq!(
            std::fs::read_to_string(run_2_sandbox.join("tests/a.test.js")).unwrap(),
            "test('a')",
            "run 2's test file got run 1's edit — isolation broken"
        );

        // Source is also untouched.
        assert_eq!(
            std::fs::read_to_string(source_sandbox.join("baseline.txt")).unwrap(),
            "baseline",
            "source sandbox got run 1's edit — COW invariant broken"
        );
    }

    /// (#490) Phase 3 — `resolve_source_sandbox` returns the
    /// default-path location when the workload has no
    /// `requires_fixture` field.
    #[test]
    fn resolver_falls_back_to_default_sandbox_when_no_requires_fixture() {
        use crate::workloads::types::{LoadedWorkload, WorkloadManifest, WorkloadSource, WorkloadSpec};
        use std::collections::BTreeMap;
        let tmp = TempDir::new().unwrap();
        let paths = paths::DarkmuxPaths {
            root: tmp.path().to_path_buf(),
            runs: tmp.path().join("runs"),
            sandboxes: tmp.path().join("sandboxes"),
            crew: tmp.path().join("crew"),
            notebook: tmp.path().join("notebook"),
            profiles: tmp.path().join("profiles.json"),
            config: tmp.path().join("config.json"),
            scope: paths::Scope::User,
        };
        let loaded = LoadedWorkload {
            manifest: WorkloadManifest {
                workload: WorkloadSpec {
                    id: "demo".into(),
                    provider: "prompt".into(),
                    description: None,
                    role: None,
                    prompt: None,
                    prompt_file: None,
                    sandbox_seed: None,
                    setup_content: BTreeMap::new(),
                    requires_external_sandbox: false,
                    requires_fixture: None,
                    verify: None,
                    expected: None,
                    extras: BTreeMap::new(),
                },
            },
            manifest_path: tmp.path().join("workloads/demo.json"),
            base_dir: tmp.path().to_path_buf(),
            source: WorkloadSource::Builtin,
        };
        let resolved = resolve_source_sandbox(&loaded, &paths).unwrap();
        assert_eq!(resolved, paths.sandboxes.join("demo"));
    }

    /// (#490) Phase 3 — happy path: register a fixture whose
    /// `.fixture.json::satisfies` matches a workload's
    /// `requires_fixture`, resolve → returns the fixture's path.
    /// Pins the load-bearing end-to-end resolution.
    #[test]
    #[serial_test::serial]
    fn resolver_returns_registered_fixture_path_when_requires_matches() {
        use crate::lab::registry::{default_registry_path, LabRegistry};
        use crate::workloads::types::{
            LoadedWorkload, WorkloadManifest, WorkloadSource, WorkloadSpec,
        };
        use std::collections::BTreeMap;
        let tmp = TempDir::new().unwrap();
        // Realistic darkmux home layout — registry lives at root.
        let paths = paths::DarkmuxPaths {
            root: tmp.path().to_path_buf(),
            runs: tmp.path().join("runs"),
            sandboxes: tmp.path().join("sandboxes"),
            crew: tmp.path().join("crew"),
            notebook: tmp.path().join("notebook"),
            profiles: tmp.path().join("profiles.json"),
            config: tmp.path().join("config.json"),
            scope: paths::Scope::User,
        };
        // Create fixture dir with .fixture.json declaring satisfies.
        let fixture_dir = tmp.path().join("my-fx");
        std::fs::create_dir_all(&fixture_dir).unwrap();
        std::fs::write(
            fixture_dir.join(".fixture.json"),
            r#"{"name": "my-fx", "satisfies": "demo-shape@1.0"}"#,
        )
        .unwrap();
        std::fs::write(fixture_dir.join("source.txt"), "baseline").unwrap();

        // Register the fixture (mimics what dm lab register does).
        let mut registry = LabRegistry::default();
        registry.register(&fixture_dir, None, false).unwrap();
        registry.save(&default_registry_path(&paths)).unwrap();

        // Workload declares the matching requires_fixture.
        let loaded = LoadedWorkload {
            manifest: WorkloadManifest {
                workload: WorkloadSpec {
                    id: "demo".into(),
                    provider: "coding-task".into(),
                    description: None,
                    role: Some("coder".into()),
                    prompt: Some("do work".into()),
                    prompt_file: None,
                    sandbox_seed: None,
                    setup_content: BTreeMap::new(),
                    requires_external_sandbox: true,
                    requires_fixture: Some("demo-shape@1.0".into()),
                    verify: None,
                    expected: None,
                    extras: BTreeMap::new(),
                },
            },
            manifest_path: tmp.path().join("workloads/demo.json"),
            base_dir: tmp.path().to_path_buf(),
            source: WorkloadSource::Builtin,
        };

        let resolved = resolve_source_sandbox(&loaded, &paths).unwrap();
        // Expect the canonicalized fixture dir.
        let expected = fixture_dir.canonicalize().unwrap();
        assert_eq!(resolved, expected);
    }

    /// (#490, #496) Phase 3 ships LITERAL string-matching in
    /// `find_satisfying`. Workloads that use semver operators like
    /// `>=1.0` will NOT resolve against fixtures declaring `1.0`.
    /// This test pins the gap until semver matching lands so the
    /// behavior change becomes explicit if/when semver is added.
    #[test]
    #[serial_test::serial]
    fn resolver_does_not_match_semver_operator_yet() {
        use crate::lab::registry::{default_registry_path, LabRegistry};
        use crate::workloads::types::{
            LoadedWorkload, WorkloadManifest, WorkloadSource, WorkloadSpec,
        };
        use std::collections::BTreeMap;
        let tmp = TempDir::new().unwrap();
        let paths = paths::DarkmuxPaths {
            root: tmp.path().to_path_buf(),
            runs: tmp.path().join("runs"),
            sandboxes: tmp.path().join("sandboxes"),
            crew: tmp.path().join("crew"),
            notebook: tmp.path().join("notebook"),
            profiles: tmp.path().join("profiles.json"),
            config: tmp.path().join("config.json"),
            scope: paths::Scope::User,
        };
        let fixture_dir = tmp.path().join("my-fx");
        std::fs::create_dir_all(&fixture_dir).unwrap();
        std::fs::write(
            fixture_dir.join(".fixture.json"),
            r#"{"name": "my-fx", "satisfies": "shape@1.0"}"#,
        )
        .unwrap();
        std::fs::write(fixture_dir.join("s.txt"), "x").unwrap();
        let mut registry = LabRegistry::default();
        registry.register(&fixture_dir, None, false).unwrap();
        registry.save(&default_registry_path(&paths)).unwrap();

        // Workload uses semver operator — current resolver does
        // literal compare, so this MUST NOT match. When semver
        // lands, this test flips intentionally.
        let loaded = LoadedWorkload {
            manifest: WorkloadManifest {
                workload: WorkloadSpec {
                    id: "demo".into(),
                    provider: "coding-task".into(),
                    description: None,
                    role: Some("coder".into()),
                    prompt: Some("x".into()),
                    prompt_file: None,
                    sandbox_seed: None,
                    setup_content: BTreeMap::new(),
                    requires_external_sandbox: true,
                    requires_fixture: Some("shape@>=1.0".into()),
                    verify: None,
                    expected: None,
                    extras: BTreeMap::new(),
                },
            },
            manifest_path: tmp.path().join("workloads/demo.json"),
            base_dir: tmp.path().to_path_buf(),
            source: WorkloadSource::Builtin,
        };
        let err = resolve_source_sandbox(&loaded, &paths).unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("shape@>=1.0"),
            "expected error to name the unsatisfied requirement: {msg}"
        );
    }

    /// (#490) Phase 3 — when `requires_fixture` is set, the resolver
    /// consults the lab registry. Missing registry / unsatisfied
    /// requirement → operator-actionable error pointing at the
    /// registry CLI verbs.
    #[test]
    #[serial_test::serial]
    fn resolver_errors_when_required_fixture_not_registered() {
        use crate::workloads::types::{LoadedWorkload, WorkloadManifest, WorkloadSource, WorkloadSpec};
        use std::collections::BTreeMap;
        let tmp = TempDir::new().unwrap();
        let paths = paths::DarkmuxPaths {
            root: tmp.path().to_path_buf(),
            runs: tmp.path().join("runs"),
            sandboxes: tmp.path().join("sandboxes"),
            crew: tmp.path().join("crew"),
            notebook: tmp.path().join("notebook"),
            profiles: tmp.path().join("profiles.json"),
            config: tmp.path().join("config.json"),
            scope: paths::Scope::User,
        };
        let loaded = LoadedWorkload {
            manifest: WorkloadManifest {
                workload: WorkloadSpec {
                    id: "demo".into(),
                    provider: "coding-task".into(),
                    description: None,
                    role: Some("coder".into()),
                    prompt: Some("do the thing".into()),
                    prompt_file: None,
                    sandbox_seed: None,
                    setup_content: BTreeMap::new(),
                    requires_external_sandbox: true,
                    requires_fixture: Some("never-registered@1.0".into()),
                    verify: None,
                    expected: None,
                    extras: BTreeMap::new(),
                },
            },
            manifest_path: tmp.path().join("workloads/demo.json"),
            base_dir: tmp.path().to_path_buf(),
            source: WorkloadSource::Builtin,
        };
        let err = resolve_source_sandbox(&loaded, &paths).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("never-registered@1.0"), "got: {msg}");
        assert!(msg.contains("darkmux lab register"), "got: {msg}");
        assert!(msg.contains("darkmux lab fixtures"), "got: {msg}");
    }

    /// (#489) Phase 2 — `enrich_manifest_with_fixture_info` adds the
    /// `fixture` section to a provider-written manifest.json, bumps
    /// schemaVersion to 4, preserves all existing fields.
    #[test]
    fn enrich_adds_fixture_section_to_existing_manifest() {
        let tmp = TempDir::new().unwrap();
        let run_dir = tmp.path().join("run-1");
        std::fs::create_dir_all(&run_dir).unwrap();
        let source_sandbox = tmp.path().join("source-sandbox");
        std::fs::create_dir_all(&source_sandbox).unwrap();
        // Pretend the provider already wrote manifest.json with v3
        // shape (Phase 1's shape: has final_hash but no fixture obj).
        std::fs::write(
            run_dir.join("manifest.json"),
            r#"{
                "schemaVersion": 3,
                "runId": "test-run-1",
                "workload": "demo",
                "final_hash": "blake3:abc"
            }"#,
        )
        .unwrap();

        enrich_manifest_with_fixture_info(
            &run_dir,
            Some("blake3:source-hash"),
            &source_sandbox,
        )
        .unwrap();

        let raw = std::fs::read_to_string(run_dir.join("manifest.json")).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&raw).unwrap();
        // Schema bumped to v4.
        assert_eq!(parsed["schemaVersion"], 4);
        // Existing fields preserved.
        assert_eq!(parsed["runId"], "test-run-1");
        assert_eq!(parsed["final_hash"], "blake3:abc");
        // New fixture section.
        assert_eq!(parsed["fixture"]["baseline_hash"], "blake3:source-hash");
        assert!(parsed["fixture"]["source_path"].is_string());
    }

    /// `baseline_hash: None` is recorded as JSON null (operator-visible
    /// "this run had no baseline" — distinct from missing key). Source
    /// dir exists here, so the concern under test is purely the
    /// baseline, not the source_path.
    #[test]
    fn enrich_records_null_when_baseline_hash_missing() {
        let tmp = TempDir::new().unwrap();
        let run_dir = tmp.path().join("run-1");
        std::fs::create_dir_all(&run_dir).unwrap();
        let source_sandbox = tmp.path().join("source-sandbox");
        std::fs::create_dir_all(&source_sandbox).unwrap();
        std::fs::write(
            run_dir.join("manifest.json"),
            r#"{"schemaVersion": 3, "runId": "r1"}"#,
        )
        .unwrap();

        enrich_manifest_with_fixture_info(&run_dir, None, &source_sandbox).unwrap();

        let parsed: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(run_dir.join("manifest.json")).unwrap())
                .unwrap();
        assert!(parsed["fixture"]["baseline_hash"].is_null());
        // Source exists → canonical string path, not null.
        assert!(parsed["fixture"]["source_path"].is_string());
    }

    /// (#496) When the source sandbox doesn't exist (self-contained
    /// workload populated by the provider's setup()), `source_path` is
    /// recorded as JSON `null` — an explicit "no source" signal rather
    /// than a non-canonical raw path that would spuriously mismatch a
    /// canonicalized run under `dm lab compare`.
    #[test]
    fn enrich_records_null_source_path_when_source_missing() {
        let tmp = TempDir::new().unwrap();
        let run_dir = tmp.path().join("run-1");
        std::fs::create_dir_all(&run_dir).unwrap();
        // Deliberately NOT created — self-contained workload.
        let source_sandbox = tmp.path().join("does-not-exist");
        std::fs::write(
            run_dir.join("manifest.json"),
            r#"{"schemaVersion": 3, "runId": "r1"}"#,
        )
        .unwrap();

        enrich_manifest_with_fixture_info(&run_dir, Some("blake3:x"), &source_sandbox).unwrap();

        let parsed: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(run_dir.join("manifest.json")).unwrap())
                .unwrap();
        assert!(
            parsed["fixture"]["source_path"].is_null(),
            "source_path should be null when source is missing, got: {}",
            parsed["fixture"]["source_path"]
        );
    }

    #[test]
    fn enrich_errors_on_missing_manifest() {
        let tmp = TempDir::new().unwrap();
        let run_dir = tmp.path().join("run-1");
        std::fs::create_dir_all(&run_dir).unwrap();
        let err = enrich_manifest_with_fixture_info(&run_dir, None, tmp.path()).unwrap_err();
        assert!(err.to_string().contains("manifest.json not present"), "got: {err}");
    }

    /// If the source sandbox doesn't exist (self-contained workload
    /// that will be populated by setupContent), the per-run dir is
    /// created empty for the provider's setup() to fill in.
    #[test]
    fn missing_source_sandbox_yields_empty_per_run_dir() {
        let tmp = TempDir::new().unwrap();
        let source = tmp.path().join("does-not-exist");
        let runs_dir = tmp.path().join("runs");
        std::fs::create_dir_all(&runs_dir).unwrap();
        let run_dir = runs_dir.join("run-1");
        std::fs::create_dir_all(&run_dir).unwrap();
        let per_run_sandbox = run_dir.join("sandbox");

        // The orchestration in lab_run does: if source exists → COW,
        // else create_dir_all. Mirror that here.
        if source.exists() {
            crate::lab::cow_clone::cow_clone_dir(&source, &per_run_sandbox).unwrap();
        } else {
            std::fs::create_dir_all(&per_run_sandbox).unwrap();
        }

        assert!(per_run_sandbox.exists());
        assert!(per_run_sandbox.is_dir());
        // Empty: ready for the provider's setupContent.
        let entries: Vec<_> = std::fs::read_dir(&per_run_sandbox).unwrap().collect();
        assert_eq!(entries.len(), 0, "per-run sandbox should be empty");
    }

    #[serial_test::serial]
    #[test]
    fn run_errors_when_no_default_profile_and_no_explicit() {
        let tmp = TempDir::new().unwrap();
        // Set up a registry without default_profile.
        let cfg = tmp.path().join("profiles.json");
        fs::write(
            &cfg,
            r#"{"profiles":{"fast":{"models":[{"id":"model-a","n_ctx":32000,"role":"primary"}]}}}"#,
        )
        .unwrap();
        // Set up a workload manifest in the user dir.
        let darkmux_dir = tmp.path().join(".darkmux");
        fs::create_dir_all(darkmux_dir.join("workloads")).unwrap();
        fs::write(
            darkmux_dir.join("workloads/q.json"),
            r#"{"workload":{"id":"q","provider":"prompt","prompt":"hi"}}"#,
        )
        .unwrap();
        let prev = std::env::current_dir().unwrap();
        std::env::set_current_dir(tmp.path()).unwrap();
        let err = lab_run(RunOpts {
            workload_id: "q".into(),
            profile_name: None,
            runs: 1,
            config_path: Some(cfg.to_str().unwrap().into()),
            quiet: true,
            runtime: darkmux_crew::dispatch::Runtime::Internal,
            runtime_cmd: "openclaw".to_string(),
        })
        .unwrap_err();
        std::env::set_current_dir(prev).unwrap();
        assert!(err.to_string().contains("default_profile"));
    }
}
