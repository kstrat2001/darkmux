//! `darkmux lab run <workload> [opts]` — execute a workload and capture output.

use crate::lab::artifact_dirs;
use crate::lab::cow_clone::cow_clone_dir_excluding;
use crate::lab::lifecycle;
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
    /// (#986) Loop lab: per-run compaction overrides applied on top of the
    /// resolved profile's compaction config. `None` (the `lab run` /
    /// `characterize` / `tune` paths) leaves the profile intact, so those
    /// paths behave byte-identically to before.
    pub loop_override: Option<crate::lab::loop_report::LoopCompactionOverride>,
    /// (#1004) Engagement-context to PREPEND to the workload prompt before
    /// dispatch — the loop-lab A/B's "with-context" arm. `None` (every other
    /// path) leaves the prompt untouched. The caller (mission-run side) builds
    /// the real injected blocks; the lab just splices them in front of the
    /// workload's own prompt so the dispatch carries the same context a real
    /// coder brief would.
    pub inject_context: Option<String>,
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

/// (#1004) Loop-lab A/B "with-context" arm: splice the caller-built
/// engagement-context blocks in FRONT of the workload's own prompt, so the
/// dispatch carries the same context a real coder brief would. Resolves the
/// workload's effective prompt (inline or from `promptFile`) first, then inlines
/// the combined text into `prompt` (clearing `prompt_file`) — both providers read
/// `manifest.workload.prompt` via `resolve_prompt`, so this one splice covers
/// prompt + coding-task workloads with no provider change. A `None`/blank context
/// (every non-A/B path, and the baseline arm) leaves the workload untouched.
fn apply_inject_context(
    loaded: &mut crate::workloads::types::LoadedWorkload,
    ctx: Option<&str>,
) {
    let Some(ctx) = ctx.filter(|c| !c.trim().is_empty()) else {
        return;
    };
    // Resolve the effective prompt with shared borrows (clone / file read produce
    // owned Strings), THEN mutate — no overlapping mut+shared borrow.
    let base = loaded.manifest.workload.prompt.clone().or_else(|| {
        loaded
            .manifest
            .workload
            .prompt_file
            .as_ref()
            .and_then(|rel| std::fs::read_to_string(loaded.base_dir.join(rel)).ok())
    });
    if let Some(base) = base {
        loaded.manifest.workload.prompt = Some(format!("{ctx}\n\n{base}"));
        loaded.manifest.workload.prompt_file = None;
    }
}

pub fn lab_run(opts: RunOpts) -> Result<Vec<RunOutcome>> {
    let paths = paths::resolve(ResolveScope::Auto);
    paths::ensure(&paths)?;

    // (#2590) The workload USER tier is forced to the home root — a
    // SEPARATE resolution from `paths` above. `paths` stays `Auto`
    // (cwd-sensitive) on purpose: it governs run-artifact placement
    // (`darkmux_types::config_access::lab_dir()`, resolved independently
    // below) and sandbox/fixture-registry lookup (`paths.sandboxes`, used
    // by `resolve_source_sandbox` further down) — both deliberately
    // project-local when the cwd has a `.darkmux/`. Folding the workload
    // *document* lookup into that same `Auto` root is what let a stale
    // `./.darkmux/workloads/<id>.json` silently outrank the embedded
    // workload of the same id, and let a cwd-only id resolve at all — the
    // exact bug class #1012 closed for crew/mission state and #2432 closed
    // for mission configs' user tier. `mission_config::load`'s
    // `crate::loader::user_state_root()` forces `ResolveScope::ForceUser`
    // for precisely this reason; the workload loader now does the same.
    let user_workloads_root = paths::resolve(ResolveScope::ForceUser).root;
    let mut loaded_workload = load(&opts.workload_id, Some(user_workloads_root.as_path()))?;

    // (#1004) Loop-lab A/B "with-context" arm: splice the caller-built
    // engagement-context blocks in FRONT of the workload's own prompt, so the
    // dispatch carries the same context a real coder brief would. Resolve the
    // workload's effective prompt (inline or from promptFile) first, then
    // inline the combined text into `prompt` (clearing `prompt_file`) — both
    // providers read `manifest.workload.prompt` via `resolve_prompt`, so this
    // one splice covers prompt + coding-task workloads with no provider change.
    apply_inject_context(&mut loaded_workload, opts.inject_context.as_deref());

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
        // Through `lab_dir()`, not `paths.runs`: the lab READER scans that
        // root, it honors DARKMUX_LAB_DIR / config.dirs.lab, and it is
        // cfg-isolated in test builds (#994). Resolving the write root
        // independently is how a run lands somewhere the reader never looks.
        let run_dir = darkmux_types::config_access::lab_dir().join(&run_id);
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
            // (#2553) Names the WINNING tier, same as `mission launch`'s
            // banner does for mission configs — the operator can no longer
            // be left wondering whether a workload resolved from the
            // embedded built-in, an on-disk override, or a user-tier copy.
            println!(
                "[lab] run {i}/{runs} — workload={} ({} tier) profile={} → {}",
                opts.workload_id, loaded_workload.source, profile_name, run_id
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

        // The lifecycle bookend goes here — directly after the directory
        // exists and BEFORE the first fallible step, so every `?` below is
        // covered by its RAII terminal guard. Two bugs closed by this one
        // placement: the scan can now classify a LIVE run as a lab run from
        // its first moment (#1937, previously it had to wait for end-of-run
        // artifacts and meanwhile showed as an untracked DISPATCH), and a run
        // that ERRORS gets a terminal record instead of falling through to an
        // idle-time guess (#1930).
        let mut lifecycle = lifecycle::RunLifecycle::start(
            &run_dir,
            &run_id,
            &opts.workload_id,
            &profile_name,
        )?;

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
        // (#488) Phase 1 — provider operates against the per-run
        // sandbox, not the source. Provider has no awareness of the
        // COW step; it just gets a sandbox dir and works against it.
        // `Drop` would record this as `interrupted`, which is true but less
        // useful than the reason. Naming the error explicitly is the whole
        // point of #1930 — "it errored, and here is why" beats "it stopped".
        let result = match with_provider(&provider_id, |p| {
            p.setup(&loaded_workload, &run_dir, &per_run_sandbox_dir)?;
            p.run(
                &loaded_workload,
                &run_dir,
                &per_run_sandbox_dir,
                profile,
                &profile_name,
                opts.config_path.as_deref(),
                opts.loop_override.as_ref(),
                // (#2511) The provider calls this at most once, right after
                // minting its own dispatch session id — attaching it to the
                // still-`Running` lifecycle record BEFORE the dispatch
                // fires, so a live lab row is joinable to its own flow
                // session for the run's whole dispatch phase, not only
                // once `manifest.json` lands at the end.
                &mut |sid: &str| lifecycle.set_session_id(sid),
            )
        }) {
            Ok(Ok(r)) => r,
            Ok(Err(e)) | Err(e) => {
                // (#2462) A caught SIGINT/SIGTERM/SIGHUP is why `dispatch`
                // itself failed here — darkmux's own reap watchdog
                // (`launch_guard::spawn_reap_watchdog`, armed by the CLI
                // before calling `lab_run`) kills this run's in-flight
                // child the moment the signal lands, which is what turns
                // into the `Err` we're holding right now. `is_set()` is
                // the SAME sticky, process-wide flag the watchdog itself
                // polls, checked here — after the fact, not raced against
                // — so a run that genuinely failed on its own (no signal
                // ever observed) still records `Error` exactly as before.
                // Recording `Error` unconditionally is
                // the #2462 bug: it archives the operator's own Ctrl-C as
                // "the endpoint broke", pointing a debugging operator at a
                // provider that never failed.
                if darkmux_types::interrupt::is_set() {
                    lifecycle.finish_interrupted(&e);
                } else {
                    lifecycle.finish_error(&e);
                }
                return Err(e);
            }
        };

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

        // Reached the end of the run. `result.ok` is the WORK's outcome and is
        // carried in the outcome/notes; the lifecycle records that the run
        // itself ran to completion rather than being cut short.
        lifecycle.finish_complete();

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
    // (#2590) Forced home, matching `lab_run`'s workload user-tier
    // resolution above — a project-local `.darkmux/workloads/` must not
    // appear in this listing either.
    let user_root = paths::resolve(ResolveScope::ForceUser).root;
    list_available(Some(&user_root))
}

/// (#489) Phase 2 — read the provider-written `<run_dir>/manifest.json`,
/// merge in a `fixture` section carrying baseline_hash + source path,
/// bump schema_version to 4, write it back. Provider stays unaware of
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
        obj.insert("schema_version".to_string(), serde_json::json!(4));
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
        // (#871) Fixture matching is LITERAL today — `find_satisfying` compares
        // the `satisfies` string for exact equality. A semver range operator in
        // the version part would therefore silently never match a registered
        // fixture, surfacing as a confusing "no fixture matches" for one that IS
        // registered. Reject the operator syntax loudly with a pointer instead
        // (full semver tracked in #496).
        if let Some((name, ver)) = requires.split_once('@') {
            if ver.starts_with(['>', '<', '^', '~', '=']) {
                return Err(anyhow!(
                    "workload `{}` requires_fixture `{}` uses a semver range operator, but fixture \
                     matching is LITERAL today (semver support tracked in #496). Use an exact \
                     `<name>@<version>`, e.g. `{name}@1.0`.",
                    loaded.manifest.workload.id,
                    requires,
                ));
            }
        }
        let reg_path = crate::lab::registry::default_registry_path(paths);
        let registry = crate::lab::registry::LabRegistry::load(&reg_path)
            .with_context(|| format!("loading {}", reg_path.display()))?;
        match registry.find_satisfying(requires) {
            Some((_name, fixture)) => Ok(fixture.path.clone()),
            // (#2590 follow-up) The fixture REGISTRY still resolves through
            // `paths` — project-local via `ResolveScope::Auto` — while the
            // workload DOCUMENT above is now home-only via `ForceUser`. That
            // split is newly reachable in a state it never could be before
            // this fix: a home-tier workload that `requires_fixture` can now
            // resolve and dispatch from a plain directory (no `.darkmux`
            // anywhere in cwd) yet fail this exact lookup from a directory
            // that happens to hold its OWN project-local `.darkmux` — even
            // when the fixture is registered globally at
            // `~/.darkmux/lab-registry.json`. Before this fix that state was
            // unreachable: the workload document itself failed to resolve
            // from such a directory, so the operator got a clean "workload
            // not found" instead of a fixture error that looks wrong for a
            // fixture that IS registered. Naming the consulted path here is
            // the minimum fix — it makes the split visible instead of
            // silent; forcing the registry itself to the home root too is a
            // real blast-radius change and belongs in its own issue.
            None => {
                // (MUST FIX, third-round frontier review) `paths::resolve`
                // decides this root in a fixed order — `DARKMUX_HOME` when
                // set (an explicit override that wins regardless of cwd,
                // checked BEFORE the project-local branch below is ever
                // evaluated), then project-local when the current directory
                // has its own `.darkmux/`, then the home tier
                // (`~/.darkmux`) otherwise. The old wording named only two
                // of those three branches, folding the `DARKMUX_HOME`
                // override into the same "otherwise" bucket as the genuine
                // home tier — so with an explicit `DARKMUX_HOME` set AND a
                // project-local `.darkmux/` also present, it named the
                // right path but mislabeled the reason as "the home tier",
                // and `DARKMUX_HOME` appeared in the sentence only as a
                // negative condition on the OTHER branch ("...and
                // DARKMUX_HOME is not set"). A test asserting the message
                // merely CONTAINS "DARKMUX_HOME" passed on that negative
                // clause alone — it never checked which branch the message
                // claimed was actually taken. Name all three branches, in
                // decision order, and say which one fired for THIS run.
                let darkmux_home_is_set = std::env::var("DARKMUX_HOME")
                    .ok()
                    .is_some_and(|v| !v.trim().is_empty());
                let decided_by = if darkmux_home_is_set {
                    "DARKMUX_HOME is set, so its override root above is what was actually \
                     consulted — not the home tier, and not this directory's own \
                     project-local `.darkmux/` even if one exists"
                } else if paths.scope == paths::Scope::Project {
                    "the current directory has its own `.darkmux/` and DARKMUX_HOME is unset, \
                     so this run resolved project-local"
                } else {
                    "DARKMUX_HOME is unset and the current directory has no project-local \
                     `.darkmux/`, so this run resolved to the home tier (~/.darkmux)"
                };
                Err(anyhow!(
                    "workload `{}` requires a fixture satisfying `{}` but no registered \
                     fixture matches in the registry at {} (this registry's root is decided \
                     in order — DARKMUX_HOME override when set, else project-local when the \
                     current directory has its own `.darkmux/`, else the home tier; for THIS \
                     run: {} — which can differ from what `darkmux lab fixture list` shows from \
                     elsewhere).\n\
                     \n\
                     Fix:\n\
                       1. Register an existing fixture that satisfies this requirement:\n\
                          darkmux lab fixture register /path/to/your/fixture\n\
                       2. Or inspect what's registered in THIS directory's registry:\n\
                          darkmux lab fixture list\n\
                       3. Or update the fixture's `.fixture.json` to set:\n\
                          \"satisfies\": \"{}\"",
                    loaded.manifest.workload.id,
                    requires,
                    reg_path.display(),
                    decided_by,
                    requires,
                ))
            }
        }
    } else {
        Ok(paths.sandboxes.join(&loaded.manifest.workload.id))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    /// RAII guard that changes the process cwd for the test's duration and
    /// restores it on drop — mirrors `workloads::load`'s test-only `CwdGuard`
    /// (#2432/#2553). Every caller MUST be `#[serial_test::serial]` — cwd is
    /// a process-global resource, and `serial_test` only coordinates among
    /// ANNOTATED tests, not any unannotated test elsewhere in this crate
    /// that happens to read/write cwd too. RAII (not manual set/restore)
    /// matters here specifically because these tests assert on the fix
    /// under test: an assertion panic mid-test must still restore cwd, or a
    /// red-proof run (which is EXPECTED to panic when the fix is reverted)
    /// leaks the temp cwd into every test that runs after it in the same
    /// `cargo test` process.
    struct CwdGuard {
        prev: std::path::PathBuf,
    }

    impl CwdGuard {
        fn new(dir: &Path) -> Self {
            let prev = std::env::current_dir().unwrap();
            std::env::set_current_dir(dir).unwrap();
            Self { prev }
        }
    }

    impl Drop for CwdGuard {
        fn drop(&mut self) {
            let _ = std::env::set_current_dir(&self.prev);
        }
    }

    /// RAII guard for `DARKMUX_HOME`: sets it for the test's duration and
    /// restores the PRIOR value (or removes it) on drop, for the same
    /// red-proof-must-still-clean-up reason as `CwdGuard`. Mirrors
    /// `crawl::unit_step_tests::HomeGuard`.
    struct HomeGuard {
        prev: Option<std::ffi::OsString>,
    }

    impl HomeGuard {
        fn set(dir: &Path) -> Self {
            let prev = std::env::var_os("DARKMUX_HOME");
            unsafe { std::env::set_var("DARKMUX_HOME", dir) };
            Self { prev }
        }
    }

    impl Drop for HomeGuard {
        fn drop(&mut self) {
            unsafe {
                match &self.prev {
                    Some(v) => std::env::set_var("DARKMUX_HOME", v),
                    None => std::env::remove_var("DARKMUX_HOME"),
                }
            }
        }
    }

    /// RAII guard that scopes the REAL `HOME` env var (which
    /// `dirs::home_dir()` reads) and force-clears `DARKMUX_HOME` for the
    /// duration, restoring both on drop. `HomeGuard` above sets
    /// `DARKMUX_HOME`, the bootstrap override that short-circuits
    /// `paths::resolve` BEFORE the `Auto`/`ForceUser` distinction is ever
    /// evaluated — the wrong tool for a test that wants to actually
    /// EXERCISE that distinction, since `DARKMUX_HOME` would make `Auto`
    /// and `ForceUser` resolve identically regardless of cwd, silently
    /// proving nothing. Setting `HOME` instead moves `dirs::home_dir()`'s
    /// answer without pre-empting the branch under test. Clearing
    /// `DARKMUX_HOME` too matters for the same reason the operator's
    /// standing note does (`env -u DARKMUX_HOME HOME=<tmp>`): an ambient
    /// `DARKMUX_HOME` in the shell running `cargo test` would otherwise
    /// still win and mask the test's real HOME override.
    struct RealHomeGuard {
        prev_home: Option<std::ffi::OsString>,
        prev_darkmux_home: Option<std::ffi::OsString>,
    }

    impl RealHomeGuard {
        fn set(dir: &Path) -> Self {
            let prev_home = std::env::var_os("HOME");
            let prev_darkmux_home = std::env::var_os("DARKMUX_HOME");
            unsafe {
                std::env::set_var("HOME", dir);
                std::env::remove_var("DARKMUX_HOME");
            }
            Self {
                prev_home,
                prev_darkmux_home,
            }
        }
    }

    impl Drop for RealHomeGuard {
        fn drop(&mut self) {
            unsafe {
                match &self.prev_home {
                    Some(v) => std::env::set_var("HOME", v),
                    None => std::env::remove_var("HOME"),
                }
                match &self.prev_darkmux_home {
                    Some(v) => std::env::set_var("DARKMUX_HOME", v),
                    None => std::env::remove_var("DARKMUX_HOME"),
                }
            }
        }
    }

    // (#2590) `lab_workloads()` resolves the workload user tier via
    // `paths::resolve(ResolveScope::ForceUser)`, which reads `DARKMUX_HOME` —
    // a process-global. This test itself mutates neither, but a concurrent
    // unannotated test could still be surprised by an in-flight guard from a
    // SERIAL test above it if it asserted on the resolved root; it doesn't
    // (panic-freedom only, and `list_available` tolerates any dir, existing
    // or not), so it's serial here purely for auditability, not necessity.
    #[serial_test::serial]
    #[test]
    fn workloads_returns_strings_without_panicking() {
        // Just verify the function doesn't panic on a fresh user dir.
        let _ = lab_workloads();
    }

    /// (#2590) The workload USER tier must NOT be cwd-sensitive: a
    /// `.darkmux/workloads/<id>.json` planted in the shell's cwd must not
    /// resolve, and must not appear in `lab workload list` — matching
    /// `mission_config::load`'s `ForceUser` fix (#1012, #2432). Red-proved:
    /// reverting `lab_workloads`'s `ResolveScope::ForceUser` back to `Auto`
    /// makes `cwd-only-ghost` appear in this list.
    #[serial_test::serial]
    #[test]
    fn workload_listing_ignores_a_cwd_local_darkmux_dir() {
        let project = TempDir::new().unwrap();
        std::fs::create_dir_all(project.path().join(".darkmux").join("workloads")).unwrap();
        std::fs::write(
            project
                .path()
                .join(".darkmux")
                .join("workloads")
                .join("cwd-only-ghost.json"),
            r#"{"workload":{"id":"cwd-only-ghost","provider":"prompt","prompt":"hi"}}"#,
        )
        .unwrap();

        // An empty, isolated home — nothing here defines `cwd-only-ghost`,
        // so if it resolves at all, it can only have come from the cwd.
        let home = TempDir::new().unwrap();
        let _home_guard = RealHomeGuard::set(home.path());
        let _cwd_guard = CwdGuard::new(project.path());

        let ids = lab_workloads();

        assert!(
            !ids.contains(&"cwd-only-ghost".to_string()),
            "a cwd-local .darkmux/workloads/<id>.json must not resolve as a \
             workload — the user tier is forced home (#2590); ids={ids:?}"
        );
        // Sanity: the embedded set is still there — this isn't an empty
        // list masquerading as a pass.
        assert!(
            ids.contains(&"quick-q".to_string()),
            "the embedded built-ins must still be listed; ids={ids:?}"
        );
    }

    /// (#2590) The counterpart of `workload_listing_ignores_a_cwd_local_darkmux_dir`
    /// for the actual DISPATCH path, not just the listing: `lab_run` must
    /// also refuse to resolve a workload id that exists ONLY in a cwd-local
    /// `.darkmux/workloads/`. Red-proved: reverting `lab_run`'s
    /// `ResolveScope::ForceUser` (the workload-user-dir resolution, not the
    /// `paths` variable used for run-artifact/sandbox placement) back to
    /// `Auto` makes this resolve the cwd document and proceed instead of
    /// erroring "not found".
    #[serial_test::serial]
    #[test]
    fn run_ignores_a_cwd_local_darkmux_dir_for_workload_lookup() {
        let project = TempDir::new().unwrap();
        std::fs::create_dir_all(project.path().join(".darkmux").join("workloads")).unwrap();
        std::fs::write(
            project
                .path()
                .join(".darkmux")
                .join("workloads")
                .join("cwd-only-ghost.json"),
            r#"{"workload":{"id":"cwd-only-ghost","provider":"prompt","prompt":"hi"}}"#,
        )
        .unwrap();

        // An empty, isolated home — nothing here defines `cwd-only-ghost`
        // either, so the ONLY way it could resolve is via the cwd.
        let home = TempDir::new().unwrap();
        let _home_guard = RealHomeGuard::set(home.path());
        let _cwd_guard = CwdGuard::new(project.path());

        let err = lab_run(RunOpts {
            workload_id: "cwd-only-ghost".into(),
            profile_name: None,
            runs: 1,
            config_path: None,
            quiet: true,
            loop_override: None,
            inject_context: None,
        })
        .unwrap_err();

        assert!(
            err.to_string().contains("not found"),
            "lab_run must not resolve a cwd-only workload id — the user tier \
             is forced home (#2590); got: {err}"
        );
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
        let paths = paths::DarkmuxPaths::under_root(tmp.path().to_path_buf());
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
                    image: None,
                    extras: BTreeMap::new(),
                },
            },
            manifest_path: tmp.path().join("workloads/demo.json"),
            base_dir: tmp.path().to_path_buf(),
            source: WorkloadSource::OnDisk,
        };
        let resolved = resolve_source_sandbox(&loaded, &paths).unwrap();
        assert_eq!(resolved, paths.sandboxes.join("demo"));
    }

    /// (#871) A semver range operator in `requires_fixture` is rejected LOUDLY
    /// — matching is literal today (#496 tracks real semver), so a `>=`-style
    /// requirement that would silently never match a registered fixture must
    /// error with a clear pointer instead.
    #[test]
    fn resolver_rejects_semver_operator_in_requires_fixture() {
        use crate::workloads::types::{LoadedWorkload, WorkloadManifest, WorkloadSource, WorkloadSpec};
        use std::collections::BTreeMap;
        let tmp = TempDir::new().unwrap();
        let paths = paths::DarkmuxPaths::under_root(tmp.path().to_path_buf());
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
                    requires_fixture: Some("demo-fixture@>=1.0".into()),
                    verify: None,
                    expected: None,
                    image: None,
                    extras: BTreeMap::new(),
                },
            },
            manifest_path: tmp.path().join("workloads/demo.json"),
            base_dir: tmp.path().to_path_buf(),
            source: WorkloadSource::OnDisk,
        };
        let err = resolve_source_sandbox(&loaded, &paths)
            .expect_err("a semver range operator should be rejected");
        let msg = err.to_string();
        assert!(msg.contains("#496"), "error should point to #496: {msg}");
        assert!(
            msg.contains("range operator") || msg.contains("LITERAL"),
            "error should explain literal-matching: {msg}"
        );
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
        let paths = paths::DarkmuxPaths::under_root(tmp.path().to_path_buf());
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
                    image: None,
                    extras: BTreeMap::new(),
                },
            },
            manifest_path: tmp.path().join("workloads/demo.json"),
            base_dir: tmp.path().to_path_buf(),
            source: WorkloadSource::OnDisk,
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
        let paths = paths::DarkmuxPaths::under_root(tmp.path().to_path_buf());
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
                    image: None,
                    extras: BTreeMap::new(),
                },
            },
            manifest_path: tmp.path().join("workloads/demo.json"),
            base_dir: tmp.path().to_path_buf(),
            source: WorkloadSource::OnDisk,
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
        let paths = paths::DarkmuxPaths::under_root(tmp.path().to_path_buf());
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
                    image: None,
                    extras: BTreeMap::new(),
                },
            },
            manifest_path: tmp.path().join("workloads/demo.json"),
            base_dir: tmp.path().to_path_buf(),
            source: WorkloadSource::OnDisk,
        };
        let err = resolve_source_sandbox(&loaded, &paths).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("never-registered@1.0"), "got: {msg}");
        assert!(msg.contains("darkmux lab fixture register"), "got: {msg}");
        assert!(msg.contains("darkmux lab fixture list"), "got: {msg}");
        // (#2590 follow-up) The registry still resolves through `paths`
        // (project-local via `ResolveScope::Auto`) while the workload
        // DOCUMENT now resolves home-only via `ForceUser` — a split that's
        // newly reachable and newly confusing (a fixture registered at the
        // home root looks "missing" from a directory with its own
        // `.darkmux/`). Naming the exact registry path consulted is the
        // minimum fix that makes the split visible instead of silent.
        assert!(
            msg.contains(&paths.root.join("lab-registry.json").display().to_string()),
            "error must name the registry path actually consulted, so the \
             project/home split is visible instead of silent: got: {msg}"
        );
    }

    /// (#2590 follow-up, review round 2 finding 2 — THIRD-ROUND FIX) The
    /// explanatory clause added by the fix above claimed the registry "is
    /// looked up project-locally when the current directory has its own
    /// `.darkmux/` AND DARKMUX_HOME is not set, the home tier otherwise" —
    /// which still mislabeled this exact state: with an explicit
    /// `DARKMUX_HOME` set AND a project-local `.darkmux/` also sitting in
    /// cwd, that wording named the right PATH but folded the override into
    /// the same "otherwise" bucket as the genuine home tier, so it called
    /// the decided branch "the home tier" when the actual root came from
    /// `DARKMUX_HOME`, not from `dirs::home_dir()`. `DARKMUX_HOME` appeared
    /// in the sentence only as a NEGATIVE condition on the other branch
    /// ("...and DARKMUX_HOME is not set") — so a prior assertion checking
    /// only `msg.contains("DARKMUX_HOME")` passed on that negative clause
    /// alone, without ever checking which branch the message claimed was
    /// actually taken. The message now names all three branches in
    /// decision order and says which one fired for THIS run; the
    /// assertion below is tightened to the taken-branch phrasing instead
    /// of bare substring presence. Red-proved: reverting the `decided_by`
    /// computation back to the two-branch hedge keeps the path assertions
    /// green while failing the taken-branch assertion below.
    #[test]
    #[serial_test::serial]
    fn resolver_fixture_error_names_dark_home_not_the_bypassed_project_root() {
        use crate::workloads::types::{LoadedWorkload, WorkloadManifest, WorkloadSource, WorkloadSpec};
        use std::collections::BTreeMap;

        let dark_home = TempDir::new().unwrap();
        let _home_guard = HomeGuard::set(dark_home.path());

        // A project-local `.darkmux/` ALSO exists in cwd — exactly the
        // state the finding names ("an explicit state root set and a
        // project-local registry also present"). `DARKMUX_HOME` must win
        // regardless.
        let project = TempDir::new().unwrap();
        std::fs::create_dir_all(project.path().join(".darkmux")).unwrap();
        let _cwd_guard = CwdGuard::new(project.path());

        let paths = paths::resolve(ResolveScope::Auto);
        assert_eq!(
            paths.root, dark_home.path(),
            "sanity: DARKMUX_HOME must win over the project-local .darkmux/ \
             in cwd, or this test isn't exercising the state under test"
        );

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
                    image: None,
                    extras: BTreeMap::new(),
                },
            },
            manifest_path: project.path().join("workloads/demo.json"),
            base_dir: project.path().to_path_buf(),
            source: WorkloadSource::OnDisk,
        };
        let err = resolve_source_sandbox(&loaded, &paths).unwrap_err();
        let msg = format!("{err:#}");

        let dark_home_registry = dark_home.path().join("lab-registry.json");
        let bypassed_project_registry = project.path().join(".darkmux").join("lab-registry.json");

        assert!(
            msg.contains(&dark_home_registry.display().to_string()),
            "must name the DARKMUX_HOME-rooted registry actually consulted: {msg}"
        );
        assert!(
            !msg.contains(&bypassed_project_registry.display().to_string()),
            "must not name the project-local registry that was never consulted: {msg}"
        );
        assert!(
            msg.contains("DARKMUX_HOME is set, so its override root above is what was actually \
                          consulted"),
            "the explanation must AFFIRMATIVELY name DARKMUX_HOME as the branch that decided \
             this path — a message that only mentions DARKMUX_HOME as a negative condition on \
             the project-local branch (\"...and DARKMUX_HOME is not set\") would still contain \
             the substring \"DARKMUX_HOME\" without ever claiming it was the decider: {msg}"
        );
        assert!(
            !msg.contains("so this run resolved to the home tier"),
            "must not mislabel this DARKMUX_HOME-decided run as having resolved to the home \
             tier — the override root is neither the home tier nor the bypassed project-local \
             `.darkmux/`: {msg}"
        );
        assert!(
            !msg.contains("so this run resolved project-local"),
            "must not claim the bypassed project-local `.darkmux/` decided this run: {msg}"
        );
    }

    /// (#489) Phase 2 — `enrich_manifest_with_fixture_info` adds the
    /// `fixture` section to a provider-written manifest.json, bumps
    /// schema_version to 4, preserves all existing fields.
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
            "schema_version": 3,
                "run_id": "test-run-1",
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
        assert_eq!(parsed["schema_version"], 4);
        // Existing fields preserved.
        assert_eq!(parsed["run_id"], "test-run-1");
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
            r#"{"schema_version": 3, "run_id": "r1"}"#,
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
            r#"{"schema_version": 3, "run_id": "r1"}"#,
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
        // (#2590) The workload user tier is forced HOME now, not cwd — a
        // `.darkmux/workloads/` planted in the shell's cwd (this test's
        // pre-#2590 setup) no longer resolves. Plant the workload manifest
        // under a `DARKMUX_HOME`-scoped home dir instead; `DARKMUX_HOME` IS
        // the darkmux root directly (no nested `.darkmux/`), so the
        // manifest lives at `<home>/workloads/q.json`. The RAII guard also
        // isolates `paths::resolve(Auto)`'s root (used by `lab_run` for
        // run-artifact placement + `paths::ensure`), since `DARKMUX_HOME`
        // wins over both `Auto` and `ForceUser` identically.
        let home = tmp.path().join("home");
        fs::create_dir_all(home.join("workloads")).unwrap();
        fs::write(
            home.join("workloads").join("q.json"),
            r#"{"workload":{"id":"q","provider":"prompt","prompt":"hi"}}"#,
        )
        .unwrap();
        let _home_guard = HomeGuard::set(&home);
        let err = lab_run(RunOpts {
            workload_id: "q".into(),
            profile_name: None,
            runs: 1,
            config_path: Some(cfg.to_str().unwrap().into()),
            quiet: true,
            loop_override: None,
            inject_context: None,
        })
        .unwrap_err();
        assert!(err.to_string().contains("default_profile"));
    }

    /// (#2511) End-to-end wiring proof, with zero dispatch/docker/LMStudio
    /// involvement: a stub provider (registered the same way
    /// `workloads::registry`'s own test module registers one) reports a
    /// session id via `on_session_id`, and `lab_run` must have wired that
    /// callback all the way to `RunLifecycle::set_session_id` — so the
    /// run's `lifecycle.json` carries it once the run finishes. This is the
    /// one link `lifecycle_tests.rs` (the method itself) and
    /// `darkmux-serve`'s `scan_lab_runs` tests (the read side) can't cover
    /// on their own: the closure plumbing inside `lab_run` between the two.
    #[test]
    #[serial_test::serial]
    fn lab_run_wires_the_providers_session_id_to_the_lifecycle_record() {
        use crate::workloads::types::{
            InspectionReport, LoadedWorkload, RunResult, VerifyOutcome, WorkloadProvider,
        };

        struct StubProvider2511;
        impl WorkloadProvider for StubProvider2511 {
            fn id(&self) -> &'static str {
                "stub-2511-session-join"
            }
            fn description(&self) -> &'static str {
                "stub for #2511's end-to-end wiring proof"
            }
            fn setup(&self, _: &LoadedWorkload, _: &Path, _: &Path) -> Result<()> {
                Ok(())
            }
            fn run(
                &self,
                _: &LoadedWorkload,
                _: &Path,
                _: &Path,
                _: &darkmux_types::Profile,
                _: &str,
                _: Option<&str>,
                _: Option<&crate::lab::loop_report::LoopCompactionOverride>,
                on_session_id: &mut dyn FnMut(&str),
            ) -> Result<RunResult> {
                // Mirrors what `coding-task`/`prompt` do for real: mint,
                // report, THEN would dispatch. No real dispatch here.
                on_session_id("darkmux-stub-2511-session-join-test");
                Ok(RunResult {
                    ok: true,
                    duration_ms: 1,
                    payload_text: Some("stub".into()),
                    trajectory_path: None,
                    verify: Some(VerifyOutcome { passed: true, details: "stub".into() }),
                    error: None,
                })
            }
            fn inspect(&self, _: &LoadedWorkload, _: &Path) -> Result<InspectionReport> {
                Ok(InspectionReport::default())
            }
        }
        // Registration is process-global and errors on a second call with
        // the same id — harmless if some earlier run of this same test left
        // it registered (the global registry never unregisters), so an
        // `Err` here is ignored rather than unwrapped.
        let _ = crate::workloads::registry::register(Box::new(StubProvider2511));

        let tmp = TempDir::new().unwrap();
        let cfg = tmp.path().join("profiles.json");
        fs::write(
            &cfg,
            r#"{"default_profile":"fast","profiles":{"fast":{"models":[{"id":"model-a","n_ctx":32000,"role":"primary"}]}}}"#,
        )
        .unwrap();

        let home = tmp.path().join("home");
        fs::create_dir_all(home.join("workloads")).unwrap();
        fs::write(
            home.join("workloads").join("stub-2511-workload.json"),
            r#"{"workload":{"id":"stub-2511-workload","provider":"stub-2511-session-join","prompt":"hi"}}"#,
        )
        .unwrap();
        let _home_guard = HomeGuard::set(&home);

        let outcomes = lab_run(RunOpts {
            workload_id: "stub-2511-workload".into(),
            profile_name: None,
            runs: 1,
            config_path: Some(cfg.to_str().unwrap().into()),
            quiet: true,
            loop_override: None,
            inject_context: None,
        })
        .unwrap();

        assert_eq!(outcomes.len(), 1, "{outcomes:?}");
        let rec = lifecycle::read(&outcomes[0].run_dir).expect("lifecycle record must exist");
        assert_eq!(rec.status, lifecycle::LifecycleStatus::Complete);
        assert_eq!(
            rec.session_id.as_deref(),
            Some("darkmux-stub-2511-session-join-test"),
            "lab_run must wire the provider's on_session_id callback through to the \
             lifecycle record: {rec:?}"
        );
    }

    /// (#2511 review CONSIDER 4) The test above only reads the lifecycle
    /// record AFTER `lab_run` has fully returned — it cannot tell "reported
    /// before dispatching" from "reported only at the terminal write",
    /// because its stub never dispatches at all. `coding_task.rs`/
    /// `prompt.rs` call `on_session_id` BEFORE their real dispatch fires
    /// (see `coding_task.rs`'s own `run()`), which is what makes a live
    /// lab row joinable to its session for the run's WHOLE duration rather
    /// than only its last instant — but nothing before this test would
    /// have caught a regression that moved the call to AFTER dispatching:
    /// every existing test, including the one above, would stay green,
    /// because they only ever observe the record post-completion.
    ///
    /// This stub stands in for the real dispatch with a blocking wait
    /// instead — mint, report, THEN block (simulating "the dispatch is
    /// still in flight") — and the test reads `lifecycle.json` from disk
    /// WHILE the stub is still blocked, on a separate thread. If the
    /// report call were moved to after the block (mirroring "after
    /// dispatch"), the mid-flight read below would time out with no
    /// session id ever landing before completion.
    #[test]
    #[serial_test::serial]
    fn lab_run_wires_the_session_id_before_the_simulated_dispatch_completes_not_after() {
        use crate::workloads::types::{
            InspectionReport, LoadedWorkload, RunResult, VerifyOutcome, WorkloadProvider,
        };
        use std::sync::{Condvar, Mutex, OnceLock};
        use std::time::{Duration, Instant};

        // Process-global, matching the process-global provider registry
        // this stub also lives in. Reset to "held" at the top of the test
        // rather than relying on Drop, since a prior failed run of this
        // same test could have left it released.
        static GATE: OnceLock<(Mutex<bool>, Condvar)> = OnceLock::new();
        fn gate() -> &'static (Mutex<bool>, Condvar) {
            GATE.get_or_init(|| (Mutex::new(false), Condvar::new()))
        }
        *gate().0.lock().unwrap() = false;

        struct OrderingStubProvider2511;
        impl WorkloadProvider for OrderingStubProvider2511 {
            fn id(&self) -> &'static str {
                "stub-2511-session-join-ordering"
            }
            fn description(&self) -> &'static str {
                "stub for #2511's report-before-dispatch ordering proof"
            }
            fn setup(&self, _: &LoadedWorkload, _: &Path, _: &Path) -> Result<()> {
                Ok(())
            }
            fn run(
                &self,
                _: &LoadedWorkload,
                _: &Path,
                _: &Path,
                _: &darkmux_types::Profile,
                _: &str,
                _: Option<&str>,
                _: Option<&crate::lab::loop_report::LoopCompactionOverride>,
                on_session_id: &mut dyn FnMut(&str),
            ) -> Result<RunResult> {
                // Mirrors the REAL order in `coding_task.rs`/`prompt.rs`:
                // mint + report, THEN dispatch. The block below stands in
                // for the dispatch — real code would be calling into
                // `dispatch_via_internal` at exactly this point.
                on_session_id("darkmux-stub-2511-ordering-test");
                let (lock, cvar) = gate();
                let released = lock.lock().unwrap();
                let (_released, timeout) =
                    cvar.wait_timeout_while(released, Duration::from_secs(5), |r| !*r).unwrap();
                assert!(!timeout.timed_out(), "test thread never released the ordering gate");
                Ok(RunResult {
                    ok: true,
                    duration_ms: 1,
                    payload_text: Some("stub".into()),
                    trajectory_path: None,
                    verify: Some(VerifyOutcome { passed: true, details: "stub".into() }),
                    error: None,
                })
            }
            fn inspect(&self, _: &LoadedWorkload, _: &Path) -> Result<InspectionReport> {
                Ok(InspectionReport::default())
            }
        }
        let _ = crate::workloads::registry::register(Box::new(OrderingStubProvider2511));

        let tmp = TempDir::new().unwrap();
        let cfg = tmp.path().join("profiles.json");
        fs::write(
            &cfg,
            r#"{"default_profile":"fast","profiles":{"fast":{"models":[{"id":"model-a","n_ctx":32000,"role":"primary"}]}}}"#,
        )
        .unwrap();

        let home = tmp.path().join("home");
        fs::create_dir_all(home.join("workloads")).unwrap();
        let workload_id = "stub-2511-ordering-workload";
        fs::write(
            home.join("workloads").join(format!("{workload_id}.json")),
            format!(
                r#"{{"workload":{{"id":"{workload_id}","provider":"stub-2511-session-join-ordering","prompt":"hi"}}}}"#
            ),
        )
        .unwrap();
        let _home_guard = HomeGuard::set(&home);

        // `HomeGuard` mutates a process-global env var, so the background
        // thread below inherits it without needing its own guard — the two
        // threads share one process environment either way.
        let cfg_str = cfg.to_str().unwrap().to_string();
        let handle = std::thread::spawn(move || {
            lab_run(RunOpts {
                workload_id: workload_id.into(),
                profile_name: None,
                runs: 1,
                config_path: Some(cfg_str),
                quiet: true,
                loop_override: None,
                inject_context: None,
            })
        });

        // Poll for the run directory (name carries a wall-clock second
        // stamp this test doesn't control) rather than computing it, then
        // poll for the session id landing on its lifecycle record — both
        // races against the background thread, bounded so a genuine
        // regression fails the test instead of hanging the suite.
        let lab_dir = darkmux_types::config_access::lab_dir();
        let prefix = format!("{workload_id}-fast-");
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut run_dir = None;
        while run_dir.is_none() && Instant::now() < deadline {
            if let Ok(entries) = std::fs::read_dir(&lab_dir) {
                for e in entries.flatten() {
                    let name = e.file_name().to_string_lossy().to_string();
                    if name.starts_with(&prefix) && name.ends_with("-1") && e.path().join("lifecycle.json").is_file()
                    {
                        run_dir = Some(e.path());
                        break;
                    }
                }
            }
            if run_dir.is_none() {
                std::thread::sleep(Duration::from_millis(10));
            }
        }
        let run_dir = run_dir.expect("run directory with a lifecycle record must appear");

        let mid_flight_deadline = Instant::now() + Duration::from_secs(5);
        let mut mid_flight = None;
        while mid_flight.is_none() && Instant::now() < mid_flight_deadline {
            if let Some(rec) = lifecycle::read(&run_dir) {
                if rec.session_id.is_some() {
                    mid_flight = Some(rec);
                }
            }
            if mid_flight.is_none() {
                std::thread::sleep(Duration::from_millis(10));
            }
        }
        let mid_flight = mid_flight.expect(
            "the session id must land on the lifecycle record WHILE the simulated dispatch is \
             still blocked — a regression that reports it only at the terminal write would \
             time out here instead",
        );
        assert_eq!(
            mid_flight.status,
            lifecycle::LifecycleStatus::Running,
            "the record must still read Running at the moment the session id is observed: {mid_flight:?}"
        );
        assert_eq!(mid_flight.session_id.as_deref(), Some("darkmux-stub-2511-ordering-test"));

        // Release the stub so it can finish and the background thread joins.
        *gate().0.lock().unwrap() = true;
        gate().1.notify_all();

        let outcomes = handle.join().unwrap().unwrap();
        assert_eq!(outcomes.len(), 1, "{outcomes:?}");
    }

    /// (#1004) `apply_inject_context` prepends the engagement-context in front
    /// of the workload's own prompt (the A/B "with" arm), and is a no-op for a
    /// `None`/blank context (every other path, and the baseline arm).
    #[test]
    fn apply_inject_context_prepends_or_noops() {
        use crate::workloads::types::{
            LoadedWorkload, WorkloadManifest, WorkloadSource, WorkloadSpec,
        };
        use std::collections::BTreeMap;
        let tmp = TempDir::new().unwrap();
        let mk = || LoadedWorkload {
            manifest: WorkloadManifest {
                workload: WorkloadSpec {
                    id: "demo".into(),
                    provider: "prompt".into(),
                    description: None,
                    role: None,
                    prompt: Some("Do the task.".into()),
                    prompt_file: None,
                    sandbox_seed: None,
                    setup_content: BTreeMap::new(),
                    requires_external_sandbox: false,
                    requires_fixture: None,
                    verify: None,
                    expected: None,
                    image: None,
                    extras: BTreeMap::new(),
                },
            },
            manifest_path: tmp.path().join("w.json"),
            base_dir: tmp.path().to_path_buf(),
            source: WorkloadSource::OnDisk,
        };

        // With context → prepended ahead of the original prompt.
        let mut w = mk();
        apply_inject_context(&mut w, Some("<lessons>\n- rule\n</lessons>"));
        let p = w.manifest.workload.prompt.as_deref().unwrap();
        assert!(p.starts_with("<lessons>"), "context goes first: {p:?}");
        assert!(p.ends_with("Do the task."), "original prompt preserved: {p:?}");

        // None / blank → untouched (baseline arm + every non-A/B path).
        let mut w2 = mk();
        apply_inject_context(&mut w2, None);
        assert_eq!(w2.manifest.workload.prompt.as_deref(), Some("Do the task."));
        apply_inject_context(&mut w2, Some("   "));
        assert_eq!(w2.manifest.workload.prompt.as_deref(), Some("Do the task."));
    }
}
