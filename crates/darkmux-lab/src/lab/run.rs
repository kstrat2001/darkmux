//! `darkmux lab run <workload> [opts]` — execute a workload and capture output.

use crate::lab::artifact_dirs;
use crate::lab::cow_clone::cow_clone_dir_excluding;
use crate::lab::lifecycle;
use crate::lab::manifest::{ManifestFixture, RunManifest, MANIFEST_FILE};
use darkmux_types::session_id::{RunId, SessionId};
use crate::lab::paths::{self, ResolveScope};
use crate::lab::sandbox_hash::hash_sandbox_dir;
use darkmux_profiles::engine;
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

/// One run of `lab_run`.
#[derive(Debug, Clone)]
pub struct RunOutcome {
    pub run_id: String,
    /// The run's own directory, created by this run alone (#2981).
    pub run_dir: std::path::PathBuf,
    /// Did the dispatch itself complete successfully (runtime exit 0, reply
    /// payload received)? Distinct from `verify_passed` — a dispatch can
    /// succeed but its reply may not pass keyword/test verification.
    pub ok: bool,
    /// Did the workload's verify spec pass? `None` if no verify was defined.
    pub verify_passed: Option<bool>,
    pub duration_ms: u128,
    pub notes: Vec<String>,
    /// (#2986) The error the provider raised, when the run ended before the
    /// provider returned a result. `None` for a run that completed, whether
    /// or not its dispatch succeeded. An errored run is a failed run: `ok`
    /// is false.
    pub provider_error: Option<String>,
    /// (F2) The runtime's `escalation_*` result when the dispatch stopped on
    /// purpose and handed the work to a higher tier. `ok` is false then, but
    /// the run did not error: the lab summary counts it in its own bucket.
    pub escalation: Option<String>,
}

impl RunOutcome {
    /// (#2986) The provider ran to completion and returned a result, so the
    /// run's timing and verify are real measurements.
    pub fn completed(&self) -> bool {
        self.provider_error.is_none()
    }

    /// How this run's dispatch ended, for the surfaces that must tell an
    /// escalation from an error (F2). Every lab surface that LABELS a run
    /// (`characterize`, `tune`, `lab loop`, the per-run notes, the batch
    /// summary) reads this instead of `ok`, which is false for both. The only
    /// remaining `ok` readers are [`Self::passed`] (so the exit code) and the
    /// serve scan of the manifest's `ok`, which reads the manifest's
    /// `escalation` first.
    pub fn end(&self) -> crate::lab::dispatch_end::DispatchEnd {
        use crate::lab::dispatch_end::DispatchEnd;
        match (&self.escalation, self.ok) {
            (Some(reason), _) => DispatchEnd::Escalated { reason: reason.clone() },
            (None, true) => DispatchEnd::Completed,
            (None, false) => DispatchEnd::Failed,
        }
    }

    /// The workload's verify ran and failed. A verify nothing declared
    /// (`None`) is not a failure.
    pub fn verify_failed(&self) -> bool {
        self.verify_passed == Some(false)
    }

    /// (#2494) The run passed: the dispatch completed AND no declared verify
    /// failed. `ok` alone lets a run whose tests failed read green.
    pub fn passed(&self) -> bool {
        self.ok && !self.verify_failed()
    }
}

/// (#2986) The one-line count `lab run` prints over its per-run lines. An
/// errored run is counted as errored, never as complete.
pub fn batch_summary(outcomes: &[RunOutcome]) -> String {
    let completed = outcomes.iter().filter(|o| o.completed()).count();
    // (F2) An escalation completed its run (the provider returned) and did
    // not error; it gets its own clause, only when there is one, so the line
    // never says "0 errored" over a run that printed an error.
    let escalated = outcomes.iter().filter(|o| o.escalation.is_some()).count();
    let escalated = if escalated > 0 { format!(", {escalated} escalated") } else { String::new() };
    format!(
        "{} run(s): {completed} completed, {} errored{escalated}",
        outcomes.len(),
        outcomes.len() - completed
    )
}

/// (#2494, #2982) The process exit code for every lab verb that runs
/// workloads (`lab run`, `lab characterize`, `lab tune`): 0 when every run
/// passed, 1 otherwise. An escalated run did not pass (its work is unfinished),
/// so it exits 1 as well, but it is labeled an escalation, never an error.
pub fn exit_code(outcomes: &[RunOutcome]) -> i32 {
    if outcomes.iter().all(RunOutcome::passed) {
        0
    } else {
        1
    }
}

/// `replyRoom` and `verify.expect` are the prompt provider's, and
/// `verify.grade` the coding-task provider's: any other provider would run
/// without them, so a workload that sets one there is refused rather than run
/// as if it had been checked.
fn refuse_keys_another_provider_ignores(loaded: &crate::workloads::types::LoadedWorkload) -> Result<()> {
    let w = &loaded.manifest.workload;
    // (#3353) The grader is wired into the coding-task provider only so far:
    // anywhere else it would never run.
    if w.provider != "coding-task" && w.verify.as_ref().is_some_and(|v| v.grade.is_some()) {
        return Err(anyhow!(
            "workload \"{}\": verify.grade is graded for the `coding-task` provider only so far; this workload's provider is `{}`",
            w.id,
            w.provider
        ));
    }
    if w.provider == "prompt" {
        return Ok(());
    }
    let set: Vec<&str> = [
        w.reply_room.is_some().then_some("replyRoom"),
        w.verify.as_ref().and_then(|v| v.expect.as_ref()).map(|_| "verify.expect"),
    ]
    .into_iter()
    .flatten()
    .collect();
    if set.is_empty() {
        return Ok(());
    }
    Err(anyhow!(
        "workload \"{}\": {} applies only to the `prompt` provider; this workload's provider is `{}`",
        w.id,
        set.join(" and "),
        w.provider
    ))
}

/// (#1004) Loop-lab A/B "with-context" arm: splice the caller-built
/// engagement-context blocks in FRONT of the workload's own prompt, so the
/// dispatch carries the same context a real coder brief would. Resolves the
/// workload's effective prompt (inline or from `promptFile`) first, then inlines
/// the combined text into `prompt` (clearing `prompt_file`) — both providers read
/// `manifest.workload.prompt` via `resolve_prompt`, so this one splice covers
/// prompt + coding-task workloads with no provider change. A `None`/blank context
/// (every non-A/B path, and the baseline arm) leaves the workload untouched.
/// `prompt_dir` is where `promptFile` resolves: the bound fixture's checkout
/// for a workload that binds one (the prompt provider reads it from the run's
/// copy of that checkout), else the workload document's own directory.
fn apply_inject_context(
    loaded: &mut crate::workloads::types::LoadedWorkload,
    ctx: Option<&str>,
    prompt_dir: &Path,
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
            .and_then(|rel| std::fs::read_to_string(prompt_dir.join(rel)).ok())
    });
    if let Some(base) = base {
        loaded.manifest.workload.prompt = Some(format!("{ctx}\n\n{base}"));
        loaded.manifest.workload.prompt_file = None;
    }
}

/// The workload checks and edits that run before any run directory exists:
/// refuse keys this provider would ignore, then splice an A/B context arm
/// in front of the prompt, read from beside the workload document. A prompt
/// workload that binds a fixture reads its `promptFile` from the run's copy
/// of that fixture, which does not exist yet, so its splice waits for
/// [`workload_for_run`].
fn prepare_workload(loaded: &mut crate::workloads::types::LoadedWorkload, ctx: Option<&str>) -> Result<()> {
    refuse_keys_another_provider_ignores(loaded)?;
    // (#3386) A criteria registry the grader cannot grade is refused before any
    // model runs; its anchors are checked once the fixture is resolved
    // (`check_criteria_anchors`).
    if let Some(spec) = loaded.manifest.workload.verify.as_ref().and_then(|v| v.grade.as_ref()) {
        crate::lab::grade::check_criteria(spec, None)
            .with_context(|| format!("workload \"{}\"", loaded.manifest.workload.id))?;
    }
    if !context_reads_the_run_copy(loaded) {
        let base_dir = loaded.base_dir.clone();
        apply_inject_context(loaded, ctx, &base_dir);
    }
    Ok(())
}

/// (#3386) A criteria registry's anchors, checked against the fixture the
/// run will copy before any model runs (the grader checks them again).
fn check_criteria_anchors(loaded: &crate::workloads::types::LoadedWorkload, paths: &paths::DarkmuxPaths) -> Result<()> {
    let Some(spec) = loaded.manifest.workload.verify.as_ref().and_then(|v| v.grade.as_ref()) else {
        return Ok(());
    };
    if spec.criteria.is_none() {
        return Ok(());
    }
    let id = &loaded.manifest.workload.id;
    let fixture = resolve_source_sandbox(loaded, paths)?;
    crate::lab::grade::check_criteria(spec, Some(&fixture)).with_context(|| format!("workload \"{id}\""))
}

/// The pre-run checks against the fixture the run will copy: the criteria
/// anchors (#3386), then the eval's role, read once (#3363).
fn check_fixture_and_read_role(
    loaded: &mut crate::workloads::types::LoadedWorkload,
    paths: &paths::DarkmuxPaths,
) -> Result<()> {
    check_criteria_anchors(loaded, paths)?;
    read_lab_inputs(loaded, paths)
}

/// (#3363) An eval owns its roles. The role the workload dispatches as is
/// read ONCE, here, before any run starts: for a workload bound to a
/// fixture, `roles/<id>.json` and `.md` from the fixture checkout every run
/// copies, kept as bytes that every run's dispatch uses (nothing reads
/// `roles/` again, so nothing a run writes into its sandbox can change
/// them); for one bound to none, a role darkmux compiles in. Refused before
/// any model loads. Stored on the workload (`lab_inputs`), which every run's
/// provider reads; left `None` when the provider names no role.
fn read_lab_inputs(loaded: &mut crate::workloads::types::LoadedWorkload, paths: &paths::DarkmuxPaths) -> Result<()> {
    use darkmux_crew::dispatch::LabInputs;
    let Some(role) = workload_role(loaded) else {
        return Ok(());
    };
    let inputs = match loaded.manifest.workload.requires_fixture {
        Some(_) => resolve_source_sandbox(loaded, paths).and_then(|dir| LabInputs::load_eval(&dir, &role)),
        None => darkmux_crew::dispatch::check_lab_role(&LabInputs::Builtin, &role).map(|()| LabInputs::Builtin),
    };
    loaded.lab_inputs = Some(inputs.with_context(|| format!("workload \"{}\"", loaded.manifest.workload.id))?);
    Ok(())
}

/// Whether a workload's prompt comes from its run's copy of a fixture.
fn context_reads_the_run_copy(loaded: &crate::workloads::types::LoadedWorkload) -> bool {
    let w = &loaded.manifest.workload;
    w.provider == "prompt" && w.requires_fixture.is_some()
}

/// The workload one run dispatches: for a prompt workload bound to a
/// fixture, with the A/B context spliced in front of the prompt file in that
/// run's own sandbox (the copy whose hash the manifest records); otherwise
/// the workload as prepared.
fn workload_for_run<'a>(
    loaded: &'a crate::workloads::types::LoadedWorkload,
    ctx: Option<&str>,
    sandbox: &Path,
) -> std::borrow::Cow<'a, crate::workloads::types::LoadedWorkload> {
    if !context_reads_the_run_copy(loaded) || !ctx.is_some_and(|c| !c.trim().is_empty()) {
        return std::borrow::Cow::Borrowed(loaded);
    }
    let mut owned = loaded.clone();
    apply_inject_context(&mut owned, ctx, sandbox);
    std::borrow::Cow::Owned(owned)
}

pub fn lab_run(opts: RunOpts) -> Result<Vec<RunOutcome>> {
    // (#2947) Bad enum config refuses before a run directory is claimed.
    // Every lab verb that runs a workload (`lab run`, `lab loop`, and the
    // benches built on this function) comes through here.
    //
    // (#2590) The workload document resolves at the darkmux root. It is read
    // (never written) before the preflight, which checks the fixture it binds; a workload that does
    // not load still gets the preflight's precise refusal first.
    let user_workloads_root = paths::resolve(ResolveScope::ForceUser).root;
    let loaded = load(&opts.workload_id, Some(user_workloads_root.as_path()));
    crate::user_files::preflight_workload(
        darkmux_types::config_enum::Scope::LabRun,
        opts.config_path.as_deref(),
        loaded.as_ref().ok(),
    )?;
    let mut loaded_workload = loaded?;
    let paths = paths::resolve(ResolveScope::ForceUser);
    paths::ensure(&paths)?;
    prepare_workload(&mut loaded_workload, opts.inject_context.as_deref())?;
    check_fixture_and_read_role(&mut loaded_workload, &paths)?;

    let registry_loaded = load_registry(opts.config_path.as_deref())?;
    // (#2902) With no `--profile`, the run is on the profile bound to the
    // role the PROVIDER dispatches as, else `default_profile`: the same
    // precedence `darkmux dispatch <role>` resolves.
    let run_role = workload_role(&loaded_workload);
    let mapped = run_role.as_deref().and_then(darkmux_types::config_access::role_profile);
    let profile_name = run_profile_name(
        opts.profile_name.as_deref(),
        run_role.as_deref(),
        mapped,
        &registry_loaded.registry,
        &registry_loaded.path,
    )?;
    let profile = get_profile(&registry_loaded.registry, &profile_name)?;
    // (#3397) Which machine serves each of the profile's models, read now,
    // before any run starts: an endpoint edited later never rewrites a run.
    let model_hardware = crate::lab::manifest::model_hardware_of(profile);

    let run = OneRun {
        opts: &opts,
        paths: &paths,
        workload: &loaded_workload,
        profile,
        profile_name: &profile_name,
        model_hardware: &model_hardware,
    };
    let runs = opts.runs.max(1);
    let mut envelope = EnvelopeWarnings::default();
    (1..=runs).map(|i| run.run(i, runs, &mut envelope)).collect()
}

/// Everything one run of a `lab_run` invocation shares with the others.
struct OneRun<'a> {
    opts: &'a RunOpts,
    paths: &'a paths::DarkmuxPaths,
    workload: &'a crate::workloads::types::LoadedWorkload,
    profile: &'a darkmux_types::Profile,
    profile_name: &'a str,
    /// (#3397) The profile's models' hardware as the registry said at start.
    model_hardware: &'a [crate::lab::manifest::ModelHardware],
}

impl OneRun<'_> {
    /// Run `i` of `runs`: claim a run dir, materialize its sandbox, run the
    /// provider, settle the verify verdict, and record the run complete.
    fn run(&self, i: u32, runs: u32, envelope: &mut EnvelopeWarnings) -> Result<RunOutcome> {
        let workload_id = &self.opts.workload_id;
        let source_sandbox_dir = resolve_source_sandbox(self.workload, self.paths)
            .with_context(|| format!("resolving source sandbox for workload `{workload_id}`"))?;
        let stamp = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
        // Through `lab_dir()`, not `paths.runs`: the lab READER scans that
        // root, it honors DARKMUX_LAB_DIR / config.dirs.lab, and it is
        // cfg-isolated in test builds (#994).
        let (run_id, run_dir) = claim_run_dir(
            &darkmux_types::config_access::lab_dir(),
            workload_id,
            self.profile_name,
            stamp,
            i,
        )?;
        if !self.opts.quiet {
            self.announce(i, runs, &run_id, envelope);
        }
        // The lifecycle bookend comes directly after the dir exists and
        // before the first fallible step, so a live run is visible from its
        // first moment (#1937) and every `?` below leaves a terminal record
        // (#1930).
        let lifecycle = lifecycle::RunLifecycle::start(&run_dir, &run_id, workload_id, self.profile_name)?;
        // (#488) Each run works on a sandbox of its own under its run dir.
        let per_run_sandbox_dir = run_dir.join("sandbox");
        let baseline_hash = materialize_sandbox(&source_sandbox_dir, &per_run_sandbox_dir, self.opts.quiet)?;
        let started = std::time::Instant::now();
        let run = RunId::lab(run_id.clone())?;
        let (mut result, lifecycle) = match self.run_provider(&run, &run_dir, &per_run_sandbox_dir, lifecycle)? {
            ProviderRun::Completed(done) => *done,
            ProviderRun::Errored(e) => {
                let provider_id = &self.workload.manifest.workload.provider;
                return Ok(errored_outcome(provider_id, run_id, run_dir, started.elapsed(), &e));
            }
        };

        self.record_fixture_and_gate(&run_dir, &source_sandbox_dir, &per_run_sandbox_dir, baseline_hash.as_deref(), &mut result);

        let provider_id = &self.workload.manifest.workload.provider;
        let notes = run_notes(provider_id, &result);
        if !self.opts.quiet {
            println!("  {}", notes.join(" | "));
        }
        // `result.ok` is the WORK's outcome, carried in the outcome; the
        // lifecycle records that the run itself ran to completion.
        lifecycle.finish_complete();
        Ok(RunOutcome {
            run_id,
            run_dir,
            ok: result.ok,
            verify_passed: result.verify.as_ref().map(|v| v.passed),
            duration_ms: result.duration_ms,
            notes,
            provider_error: None,
            escalation: result.escalation.clone(),
        })
    }

    /// After the provider completed: enrich its manifest with the fixture's
    /// provenance, then apply the work gate and settle the verify verdict.
    fn record_fixture_and_gate(
        &self,
        run_dir: &Path,
        source: &Path,
        sandbox: &Path,
        baseline_hash: Option<&str>,
        result: &mut crate::workloads::types::RunResult,
    ) {
        // (#489) Best-effort: fixture provenance is observability, not
        // correctness, so a manifest that cannot be enriched never fails
        // the run.
        if let Err(e) = enrich_manifest_with_fixture_info(run_dir, baseline_hash, source) {
            warn(self.opts.quiet, &format!("enriching manifest with fixture info skipped: {e}"));
        }
        if let Err(e) = record_model_hardware(run_dir, self.model_hardware) {
            warn(self.opts.quiet, &format!("recording which machine served the models skipped: {e}"));
        }
        if let Some(home) = crate::workloads::load::eval_home_of(self.workload) {
            if let Err(e) = record_eval_workload(run_dir, &home, &self.workload.manifest.workload.id) {
                warn(self.opts.quiet, &format!("recording the eval's workload skipped: {e}"));
            }
        }
        // (#2833) The write-the-tests work gate runs after the enrichment,
        // so both hashes it compares are on disk.
        let coverage_min_pct = self.workload.manifest.workload.verify.as_ref().and_then(|v| v.coverage_min_pct);
        let gate = crate::lab::verify_gate::apply(
            run_dir,
            source,
            sandbox,
            result.end(),
            coverage_min_pct,
        );
        // (#3353) The grader, when the workload declares one, on top of the gate.
        let gate = crate::lab::grade::apply(
            run_dir,
            source,
            sandbox,
            &result.end(),
            result.verify.as_ref(),
            self.workload,
            gate,
        );
        // A graded run has a verdict even with no verify command of its own.
        if result.verify.is_none() && self.workload.manifest.workload.verify.as_ref().is_some_and(|v| v.grade.is_some()) {
            result.verify = Some(crate::workloads::types::VerifyOutcome { passed: true, details: String::new() });
        }
        if let Some(w) = settle_verify(result.verify.as_mut(), gate) {
            warn(self.opts.quiet, &w);
        }
    }

    /// The run banner, naming the tier the workload resolved from (#2553),
    /// and (#365/#544) a warning when the loaded models do not match the
    /// profile this run is stamped with. Checked per run, since a model can
    /// be evicted between runs.
    fn announce(&self, i: u32, runs: u32, run_id: &str, envelope: &mut EnvelopeWarnings) {
        println!(
            "[lab] run {i}/{runs}: workload={} ({} tier) profile={} → {run_id}",
            self.opts.workload_id, self.workload.source, self.profile_name
        );
        if let Some(note) = shadowed_note(self.workload, &paths::resolve(ResolveScope::ForceUser).root) {
            eprintln!("[lab] note: {note}");
        }
        for w in envelope.fresh(envelope_check(self.profile, self.profile_name)) {
            eprintln!("[lab] warn: {w}");
        }
    }

    /// Set the provider up and run it against the per-run sandbox, handing
    /// the lifecycle back to finish. A provider error is recorded on the
    /// run's lifecycle (#1930) and fails this run only (#2986): the batch
    /// goes on. A caught signal is the one error that ends the batch, recorded
    /// as interrupted rather than as a provider failure (#2462).
    fn run_provider(
        &self,
        run: &RunId,
        run_dir: &Path,
        sandbox: &Path,
        mut lifecycle: lifecycle::RunLifecycle,
    ) -> Result<ProviderRun> {
        let workload = workload_for_run(self.workload, self.opts.inject_context.as_deref(), sandbox);
        let result = with_provider(&self.workload.manifest.workload.provider, |p| {
            p.setup(&workload, run_dir, sandbox)?;
            p.run(
                &workload,
                run_dir,
                sandbox,
                self.profile,
                self.profile_name,
                self.opts.config_path.as_deref(),
                self.opts.loop_override.as_ref(),
                // (#2511) Called right after the provider mints its dispatch
                // session id, before the dispatch fires, so a live run is
                // joinable to its flow session for its whole dispatch phase.
                run,
                &mut |sid: &SessionId| lifecycle.set_session_id(sid),
            )
        })
        .and_then(|r| r);
        match result {
            Ok(r) => Ok(ProviderRun::Completed(Box::new((r, lifecycle)))),
            // The operator's stop, by the one decider every record of a
            // run's ending names it from (never the run's own bound), so this
            // record and the run's flow terminal read the same ending.
            Err(e) if darkmux_types::interrupt::stop_reason().is_some() => {
                lifecycle.finish_interrupted(&e);
                Err(e)
            }
            Err(e) => {
                lifecycle.finish_error(&e);
                let id = run_dir.file_name().unwrap_or_default().to_string_lossy();
                eprintln!("[lab] run {id} failed: {}", lifecycle::error_summary(&e));
                Ok(ProviderRun::Errored(e))
            }
        }
    }
}

/// How the provider's part of one run ended, short of a signal.
enum ProviderRun {
    Completed(Box<(crate::workloads::types::RunResult, lifecycle::RunLifecycle)>),
    Errored(anyhow::Error),
}

/// (#2986) The outcome of a run whose provider raised an error: a failed run,
/// kept alongside the batch's other outcomes rather than discarding them.
fn errored_outcome(
    provider_id: &str,
    run_id: String,
    run_dir: std::path::PathBuf,
    elapsed: std::time::Duration,
    e: &anyhow::Error,
) -> RunOutcome {
    let error = lifecycle::error_summary(e);
    RunOutcome {
        notes: vec![format!("provider={provider_id}"), format!("error: {error}")],
        run_id,
        run_dir,
        ok: false,
        verify_passed: None,
        duration_ms: elapsed.as_millis(),
        provider_error: Some(error),
        escalation: None,
    }
}

/// Envelope warnings already printed, so a mismatch that holds across
/// `--repeat N` warns once rather than once per run.
#[derive(Default)]
struct EnvelopeWarnings {
    last: Option<Vec<String>>,
}

impl EnvelopeWarnings {
    /// The warnings to print for this run: all of them when they differ from
    /// the last run's, none when they are the same.
    fn fresh(&mut self, warns: Vec<String>) -> Vec<String> {
        if self.last.as_ref() == Some(&warns) {
            return Vec::new();
        }
        self.last = Some(warns.clone());
        warns
    }
}

/// The loaded models against the profile's declared ones. A failed `lms ps`
/// is its own warning, so a run whose profile tag went unverified says so.
fn envelope_check(profile: &darkmux_types::Profile, profile_name: &str) -> Vec<String> {
    // (#316) A profile with nothing on LM Studio (every model on the default
    // engine or another unmanaged endpoint) has nothing there to verify.
    if !profile.models.iter().any(|m| m.is_managed()) {
        return Vec::new();
    }
    match engine::lmstudio().loaded() {
        Ok(loaded) => crate::lab::profile_check::envelope_warnings(profile, profile_name, &loaded),
        Err(e) => vec![format!(
            "could not verify profile-load match: `lms ps` failed ({e}); \
             this run's `profile={profile_name}` tag is unverified. (#365)"
        )],
    }
}

fn warn(quiet: bool, msg: &str) {
    if !quiet {
        eprintln!("[lab] warn: {msg}");
    }
}

/// (#488, #489, #496) Materialize the per-run sandbox. With a source, it is
/// a COW clone of it with run-artifact dirs pruned, and the returned
/// baseline hash is of the CLONE (byte-identical, and immune to a writer
/// racing the source). Without one, the provider's setup populates an empty
/// dir and there is no baseline hash. A hash failure warns and yields none.
fn materialize_sandbox(source: &Path, per_run: &Path, quiet: bool) -> Result<Option<String>> {
    if !source.exists() {
        fs::create_dir_all(per_run)
            .with_context(|| format!("creating empty per-run sandbox {}", per_run.display()))?;
        return Ok(None);
    }
    // (#3361) The eval's own folder (its workloads and grading data) never
    // reaches the sandbox: the model must not see the grader.
    let eval_dir = crate::lab::fixture::eval_dir_of(source);
    if let Some(dir) = &eval_dir {
        if fs::symlink_metadata(source.join(dir)).is_ok_and(|m| m.file_type().is_symlink()) {
            anyhow::bail!(
                "the fixture's eval_dir `{dir}` is a symlink; the clone cannot keep the grader out of the sandbox through a link. Make it a real folder"
            );
        }
    }
    let mut excluded: Vec<&str> = artifact_dirs::RUN_ARTIFACT_DIRS.to_vec();
    excluded.extend(eval_dir.as_deref());
    cow_clone_dir_excluding(source, per_run, &excluded).with_context(|| {
        format!("cow-cloning source sandbox {} → {}", source.display(), per_run.display())
    })?;
    match hash_sandbox_dir(per_run) {
        Ok(h) => Ok(Some(h)),
        Err(e) => {
            warn(quiet, &format!("baseline_hash for {} skipped: {e}", per_run.display()));
            Ok(None)
        }
    }
}

/// (#2833) Bring the in-memory verify in line with the work gate, so the
/// outcome and the notes agree with what the gate wrote to `manifest.json`.
/// A gate that could not be applied at all fails the verify CLOSED, even
/// though nothing could be persisted. Returns the warning to show for that
/// case.
fn settle_verify(
    verify: Option<&mut crate::workloads::types::VerifyOutcome>,
    gate: Result<Option<crate::lab::verify_gate::WorkGateResult>>,
) -> Option<String> {
    let (passed, details, warning) = match gate {
        Ok(None) => return None,
        Ok(Some(g)) => (g.passed, g.details, None),
        Err(e) => (
            false,
            format!("verify gate could not be applied: {e}"),
            Some(format!(
                "applying verify work gate failed ({e}): failing verify closed rather than trusting the raw result"
            )),
        ),
    };
    if let Some(v) = verify {
        v.passed = passed;
        v.details = details;
    }
    warning
}

/// The one-line summary `lab run` prints per run.
fn run_notes(provider_id: &str, result: &crate::workloads::types::RunResult) -> Vec<String> {
    let mut notes = vec![
        format!("provider={provider_id}"),
        format!("wall={}s", result.duration_ms / 1000),
        match (&result.escalation, result.ok) {
            (Some(reason), _) => format!("escalated: {reason}"),
            (None, true) => "ok".to_string(),
            (None, false) => format!("error: {}", result.error.as_deref().unwrap_or("unknown")),
        },
    ];
    if let Some(v) = &result.verify {
        notes.push(format!("verify={} ({})", if v.passed { "pass" } else { "fail" }, v.details));
    }
    notes
}

/// A run id: `<workload>-<profile>-<epoch_secs>-<n>`. `stats` reads the
/// stamp back as the second-to-last dash-separated segment, so `n` never
/// contains a dash.
fn run_id(workload: &str, profile: &str, stamp: u64, n: &str) -> String {
    format!("{workload}-{profile}-{stamp}-{n}")
}

/// Claim a run directory of this run's own under `root`, created
/// EXCLUSIVELY, and return its id with it (#2981). The first candidate's
/// last segment is the run's ordinal `i`; when that directory already exists
/// (an earlier run of the same workload and profile in the same second, from
/// this process or another), the next candidates are `i.2`, `i.3`, and so
/// on. A run id therefore names exactly one run, and a run never writes into
/// a directory it did not create.
fn claim_run_dir(
    root: &Path,
    workload: &str,
    profile: &str,
    stamp: u64,
    i: u32,
) -> Result<(String, std::path::PathBuf)> {
    fs::create_dir_all(root).with_context(|| format!("creating {}", root.display()))?;
    let mut claim = 1u32;
    loop {
        let n = if claim == 1 { i.to_string() } else { format!("{i}.{claim}") };
        let id = run_id(workload, profile, stamp, &n);
        let dir = root.join(&id);
        match fs::create_dir(&dir) {
            Ok(()) => return Ok((id, dir)),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => claim += 1,
            Err(e) => return Err(e).with_context(|| format!("creating {}", dir.display())),
        }
    }
}

pub fn lab_workloads() -> Vec<String> {
    // Matches `lab_run`'s workload user-tier resolution above.
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
    let manifest_path = run_dir.join(MANIFEST_FILE);
    if !manifest_path.exists() {
        return Err(anyhow!("manifest.json not present at {}", manifest_path.display()));
    }
    let mut manifest = RunManifest::read(run_dir)?;

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
    //     the provider's setup()) → `null`, an explicit "no
    //     source" signal rather than a non-canonical raw path that
    //     would spuriously mismatch a canonicalized run.
    let source_path = source_sandbox_dir.exists().then(|| {
        source_sandbox_dir
            .canonicalize()
            .unwrap_or_else(|_| source_sandbox_dir.to_path_buf())
            .display()
            .to_string()
    });
    manifest.fixture = Some(ManifestFixture {
        source_path: Some(source_path),
        baseline_hash: Some(baseline_hash.map(str::to_string)),
        extras: Default::default(),
    });
    // (#2494) RAISE to 4, never lower. This enricher mints v4 to mean
    // "has a `fixture` block", but it runs AFTER the provider, which
    // may already have written a HIGHER version for a field of its own
    // (v5 = "has `verify`"). An unconditional insert here silently
    // downgraded that, stamping v4 onto a manifest that really is v5.
    manifest.raise_schema_version(4);
    // What built this darkmux, so a shared result names it (`lab submit`).
    manifest.darkmux = Some(darkmux_types::build_info());
    // The OS name at the run. A rolling release has no install time, so a
    // later submit cannot recover the name from the machine (#3367).
    if manifest.os.is_none() {
        manifest.os = darkmux_platform::current().os_release().map(|r| r.name).filter(|n| !n.is_empty());
    }
    manifest.write(run_dir)?;
    Ok(())
}

/// (#3397) Write the snapshot taken when the run started into its manifest,
/// with the model its replies said answered, for `lab submit` (run manifest
/// 1.3). Best-effort like the fixture
/// enrichment: a run whose provider wrote no manifest cannot be submitted
/// anyway.
fn record_model_hardware(run_dir: &Path, snapshot: &[crate::lab::manifest::ModelHardware]) -> Result<()> {
    if !run_dir.join(MANIFEST_FILE).exists() {
        return Ok(());
    }
    let mut manifest = RunManifest::read(run_dir)?;
    manifest.model_hardware = Some(snapshot.to_vec());
    // The model the run's replies said answered, now that they are all in.
    let trajectory = fs::read_to_string(run_dir.join(darkmux_trajectory::TRAJECTORY_FILE)).unwrap_or_default();
    manifest.served_model = crate::lab::manifest::served_model_in(&trajectory);
    manifest.write(run_dir)
}

/// (#3361) Name the eval's own workload document in the run's `fixture`
/// block: `eval_dir`, the document's path relative to the fixture, and its
/// sha256, so `lab submit` can prove the run used the document at the
/// fixture commit it names.
fn record_eval_workload(run_dir: &Path, home: &crate::workloads::load::EvalHome, id: &str) -> Result<()> {
    let doc = home.document(id).ok_or_else(|| anyhow!("the eval no longer defines workload `{id}`"))?;
    let rel = doc
        .strip_prefix(&home.fixture_dir)
        .map_err(|_| anyhow!("{} is not inside the fixture {}", doc.display(), home.fixture_dir.display()))?
        .components()
        .map(|c| c.as_os_str().to_string_lossy().into_owned())
        .collect::<Vec<_>>()
        .join("/");
    let sha = crate::lab::submit::sha256::file_hex(&doc).with_context(|| format!("hashing {}", doc.display()))?;
    let mut manifest = RunManifest::read(run_dir)?;
    let fixture = manifest.fixture.get_or_insert_with(Default::default);
    fixture.extras.insert("eval_dir".into(), serde_json::json!(home.eval_dir));
    fixture.extras.insert("workload_path".into(), serde_json::json!(rel));
    fixture.extras.insert("workload_sha256".into(), serde_json::json!(sha));
    manifest.write(run_dir)
}

/// (#490, #2613) Phase 3 — resolve the source sandbox for a workload.
///
/// Resolution order:
///   1. If `workload.requires_fixture` is set, look up a registered
///      fixture satisfying it via the lab registry. The registry is
///      consulted at the darkmux root — `ResolveScope::ForceUser`, independent
///      of `paths` below. With `DARKMUX_HOME` unset, or set to an absolute
///      path, a fixture registered once is visible from every directory. A
///      `DARKMUX_HOME` set to a RELATIVE path
///      is a separate, pre-existing gap `paths::resolve` still has: the
///      value is used verbatim after tilde expansion with no
///      canonicalization, so it resolves against cwd and reproduces the
///      exact #2613 symptom (register from one directory, "vanish" from
///      another) even with the override set. Not fixed here — tracked
///      separately for `paths::resolve` itself as #2664. If found, use
///      that fixture's path as the source. If no fixture satisfies,
///      return an operator-actionable error pointing at
///      `dm lab fixture register`.
///   2. Otherwise fall back to `{paths.sandboxes}/<workload-id>/` (the
///      default location for workloads with `setupContent` or no external
///      dependency).
///
/// The pre-#490 `DARKMUX_SANDBOX_<WORKLOAD-ID>` env-var path has been
/// removed cleanly per the `no-compat-baggage-pre-1.0` doctrine. The
/// fixture registry (Phase 2 + 4) is the only persistent binding.
pub(crate) fn resolve_source_sandbox(
    loaded: &crate::workloads::types::LoadedWorkload,
    paths: &paths::DarkmuxPaths,
) -> Result<std::path::PathBuf> {
    // (#3361) A workload its eval defines runs on that eval's fixture (the
    // load already checked it binds it), never on another that satisfies
    // the same requirement.
    if let Some(home) = crate::workloads::load::eval_home_of(loaded) {
        return Ok(home.fixture_dir);
    }
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
        let registry_paths = paths::resolve(ResolveScope::ForceUser);
        let reg_path = crate::lab::registry::default_registry_path(&registry_paths);
        let registry = crate::lab::registry::LabRegistry::load(&reg_path)
            .with_context(|| format!("loading {}", reg_path.display()))?;
        match registry.find_satisfying(requires) {
            Some((_name, fixture)) => Ok(fixture.path.clone()),
            None => Err(anyhow!(
                "workload `{}` requires a fixture satisfying `{}` but no registered \
                 fixture matches in the lab registry at {} (the registry lives \
                 at DARKMUX_HOME when set, else ~/.darkmux, whatever directory you run from).\n\
                 \n\
                 Fix:\n\
                   1. Register an existing fixture that satisfies this requirement:\n\
                      darkmux lab fixture register /path/to/your/fixture\n\
                   2. Or inspect what's registered (same registry from any directory):\n\
                      darkmux lab fixture list\n\
                   3. Or update the fixture's `.fixture.json` to set:\n\
                      \"satisfies\": \"{}\"",
                loaded.manifest.workload.id,
                requires,
                reg_path.display(),
                requires,
            )),
        }
    } else {
        Ok(paths.sandboxes.join(&loaded.manifest.workload.id))
    }
}

/// (#3361) When the eval's own document won over a copy in the user tier,
/// say so: the copy is not what ran.
fn shadowed_note(loaded: &crate::workloads::types::LoadedWorkload, user_root: &Path) -> Option<String> {
    if loaded.source != crate::workloads::types::WorkloadSource::Eval {
        return None;
    }
    let id = &loaded.manifest.workload.id;
    let copy = [user_root.join("workloads").join(format!("{id}.json")), user_root.join("workloads").join(id).join("workload.json")]
        .into_iter()
        .find(|p| p.is_file())?;
    Some(format!(
        "{} is ignored: the eval defines workload `{id}` itself (loaded from {}). Delete the copy, or give it another id to experiment with it",
        copy.display(),
        loaded.base_dir.display()
    ))
}

/// (#2902 re-review C3) The role `loaded`'s provider dispatches as; `None`
/// when the provider is not registered or dispatches no single role.
fn workload_role(loaded: &crate::workloads::types::LoadedWorkload) -> Option<String> {
    with_provider(&loaded.manifest.workload.provider, |p| p.dispatch_role(loaded)).ok().flatten()
}

/// (#2902 re-review C4) The role workload `workload_id` dispatches as, for a
/// caller that sizes something for that dispatch before the run (the loop
/// lab's A/B brief). Loads the workload the way `lab run` does.
pub fn workload_dispatch_role(workload_id: &str) -> Option<String> {
    let user_workloads_root = paths::resolve(ResolveScope::ForceUser).root;
    let loaded = load(workload_id, Some(user_workloads_root.as_path())).ok()?;
    workload_role(&loaded)
}

/// The profile a lab run is on (see the call site): an explicit
/// `--profile`, else the role's `role_profiles` binding (`mapped`, which must
/// name a defined profile: a loud error otherwise, contract 7), else
/// `default_profile`. Pure over `mapped` so the bound arm is testable
/// (`config_access` is empty under test builds, #811).
fn run_profile_name(
    requested: Option<&str>,
    role: Option<&str>,
    mapped: Option<String>,
    registry: &darkmux_types::ProfileRegistry,
    registry_path: &std::path::Path,
) -> Result<String> {
    if let Some(p) = requested {
        return Ok(p.to_string());
    }
    if let (Some(role), Some(mapped)) = (role, mapped) {
        let binding = darkmux_profiles::profiles::RoleBinding::Mapped(mapped);
        return Ok(darkmux_profiles::profiles::resolve_role_profile_with(role, &binding, registry)?.profile_name);
    }
    registry
        .default_profile
        .clone()
        .ok_or_else(|| {
            anyhow!(
                "darkmux lab run: {}",
                darkmux_profiles::profiles::no_profile_message(role, None, Some(registry_path))
            )
        })
}

#[cfg(test)]
mod tests {
    /// (#2947) `lab run` (via `lab_run`) refuses an unregistered value in
    /// every setting the `LabRun` scope consumes, under its OWN label,
    /// before writing anything: a refusal from the dispatch primitive
    /// further down would read `dispatch:` and would mean a run directory
    /// was already minted.
    #[serial_test::serial]
    #[test]
    fn lab_run_refuses_bad_enum_config_before_minting() {
        use darkmux_types::config_enum::{Scope, ENUM_SETTINGS};
        let mut exercised = 0;
        for s in ENUM_SETTINGS.iter().filter(|s| s.scopes.contains(&Scope::LabRun)) {
            let var = s.env.expect("every LabRun-scope setting today has an env var");
            let state = darkmux_types::test_isolation::IsolatedState::new();
            unsafe { std::env::set_var(var, "zz-bad-lab") };
            let run_err = super::lab_run(super::RunOpts {
                workload_id: "quick-q".into(),
                profile_name: None,
                runs: 1,
                config_path: None,
                quiet: true,
                loop_override: None,
                inject_context: None,
            })
            .unwrap_err()
            .to_string();
            unsafe { std::env::remove_var(var) };
            assert!(run_err.contains("lab run: refusing to start: bad config"), "{}: {run_err}", s.key);
            assert!(run_err.contains("`zz-bad-lab`") && run_err.contains(var), "{}: {run_err}", s.key);
            let mut written = Vec::new();
            fn walk(d: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
                for e in std::fs::read_dir(d).into_iter().flatten().flatten() {
                    if e.path().is_dir() { walk(&e.path(), out) } else { out.push(e.path()) }
                }
            }
            walk(state.path(), &mut written);
            assert!(written.is_empty(), "{}: lab wrote state before refusing: {written:?}", s.key);
            exercised += 1;
        }
        assert!(exercised >= 3, "only {exercised} LabRun-scope settings exercised");
    }

    /// (#2902 re-review C3) `lab run` picks the profile for the role the
    /// PROVIDER reports it dispatches, not the manifest role or
    /// `runtime.default_role` guessed on its behalf.
    #[test]
    fn the_run_role_is_the_providers_dispatch_role() {
        use crate::workloads::types::{InspectionReport, LoadedWorkload, RunResult, WorkloadProvider};
        struct RoleStub2902;
        impl WorkloadProvider for RoleStub2902 {
            fn id(&self) -> &'static str {
                "stub-2902-role"
            }
            fn setup(&self, _: &LoadedWorkload, _: &std::path::Path, _: &std::path::Path) -> anyhow::Result<()> {
                Ok(())
            }
            fn dispatch_role(&self, _: &LoadedWorkload) -> Option<String> {
                Some("provider-role".to_string())
            }
            fn run(
                &self,
                _: &LoadedWorkload,
                _: &std::path::Path,
                _: &std::path::Path,
                _: &darkmux_types::Profile,
                _: &str,
                _: Option<&str>,
                _: Option<&crate::lab::loop_report::LoopCompactionOverride>,
                _: &darkmux_types::session_id::RunId,
                _: &mut dyn FnMut(&darkmux_types::session_id::SessionId),
            ) -> anyhow::Result<RunResult> {
                unreachable!("never run")
            }
            fn inspect(&self, _: &LoadedWorkload, _: &std::path::Path) -> anyhow::Result<InspectionReport> {
                Ok(InspectionReport::default())
            }
        }
        let _ = crate::workloads::registry::register(Box::new(RoleStub2902));
        let manifest: crate::workloads::types::WorkloadManifest = serde_json::from_str(
            r#"{"workload":{"id":"w","provider":"stub-2902-role","role":"manifest-role","prompt":"hi"}}"#,
        )
        .unwrap();
        let loaded = LoadedWorkload {
            manifest,
            base_dir: std::path::PathBuf::new(),
            source: crate::workloads::types::WorkloadSource::Embedded,
            lab_inputs: None,
        };
        assert_eq!(super::workload_role(&loaded).as_deref(), Some("provider-role"));
    }

    /// (#2902) A lab run with no `--profile` is stamped (and run) on the
    /// profile its role is bound to, like `darkmux dispatch <role>`; an
    /// explicit `--profile` still wins; unbound falls to `default_profile`;
    /// a binding to an undefined profile is loud.
    #[test]
    fn a_lab_run_honors_the_roles_profile_binding() {
        let reg: darkmux_types::ProfileRegistry = serde_json::from_str(
            r#"{"profiles":{"fast":{"models":[{"id":"a","n_ctx":1}]},"big":{"models":[{"id":"b","n_ctx":2}]}},
                "default_profile":"fast"}"#,
        )
        .unwrap();
        let name = |req: Option<&str>, mapped: Option<&str>| {
            super::run_profile_name(req, Some("coder"), mapped.map(str::to_string), &reg, std::path::Path::new("profiles.json"))
        };
        assert_eq!(name(None, Some("big")).unwrap(), "big");
        assert_eq!(name(Some("fast"), Some("big")).unwrap(), "fast");
        assert_eq!(name(None, None).unwrap(), "fast");
        assert!(format!("{:#}", name(None, Some("ghost")).unwrap_err()).contains("ghost"));
    }

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

    /// RAII guard for the darkmux state root, for the same
    /// red-proof-must-still-clean-up reason as `CwdGuard`.
    ///
    /// (#2718) Backed by
    /// [`darkmux_types::test_isolation::IsolatedState`], which pins EVERY
    /// darkmux write destination under one throwaway root — then re-points
    /// the ones this fixture stages at `dir`.
    ///
    /// The `DARKMUX_HOME`-only version this replaces was the guard class
    /// #2693 retired in `darkmux-crew` and left standing here: correct
    /// about its one variable and silent about the other twelve. It is
    /// insufficient in BOTH directions. `lab_dir()` reads
    /// `env(DARKMUX_LAB_DIR) > config.dirs.runs > <root>/runs`, so an
    /// operator with `DARKMUX_LAB_DIR` exported had `list_runs` scan THAT
    /// directory rather than the fixture — the same shape as the ambient
    /// `DARKMUX_HOME` failure the old doc comment describes, one variable
    /// over. And in the other direction, every destination the test never
    /// names stayed pointed at the operator's real tree.
    ///
    /// No `Drop` of its own: `IsolatedState` recorded the pre-guard value
    /// of each variable at construction, including the three re-pointed
    /// below, so its own restore hands all of them back exactly.
    pub(super) struct HomeGuard {
        _isolated: darkmux_types::test_isolation::IsolatedState,
    }

    impl HomeGuard {
        pub(super) fn set(dir: &Path) -> Self {
            let isolated = darkmux_types::test_isolation::IsolatedState::new();
            // SAFETY: every caller holds `#[serial_test::serial]`.
            unsafe {
                // OUTRANKS `DARKMUX_HOME` in `user_state_root()`.
                std::env::set_var("DARKMUX_HOME", dir);
                // OUTRANKS it in `lab_dir()`, which is what `list_runs`
                // and `run` actually resolve through.
                std::env::set_var("DARKMUX_LAB_DIR", dir.join("runs"));
            }
            Self { _isolated: isolated }
        }
    }

    /// RAII guard that scopes the REAL `HOME` env var (which
    /// `dirs::home_dir()` reads) and force-clears `DARKMUX_HOME` for the
    /// duration, restoring both on drop. `HomeGuard` above sets
    /// `DARKMUX_HOME`, the bootstrap override that short-circuits
    /// `paths::resolve` — the wrong tool for a test that wants the default
    /// `~/.darkmux` root. Setting `HOME` instead moves `dirs::home_dir()`'s
    /// answer without pre-empting that default. Clearing
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

    /// The workload USER tier is not cwd-sensitive: a
    /// `.darkmux/workloads/<id>.json` planted in the shell's cwd must not
    /// resolve, and must not appear in `lab workload list`.
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

    /// The counterpart of `workload_listing_ignores_a_cwd_local_darkmux_dir`
    /// for the actual DISPATCH path, not just the listing: `lab_run` must
    /// also refuse to resolve a workload id that exists ONLY in a cwd-local
    /// `.darkmux/workloads/`.
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
                schema_version: None,
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
                    trials: None,
                    task_timeout_seconds: None,
                    chain_depths: None,
                    reply_room: None,
                    seed: None,
                    extras: BTreeMap::new(),
                },
            },
            base_dir: tmp.path().to_path_buf(),
            source: WorkloadSource::OnDisk,
            lab_inputs: None,
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
                schema_version: None,
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
                    trials: None,
                    task_timeout_seconds: None,
                    chain_depths: None,
                    reply_room: None,
                    seed: None,
                    extras: BTreeMap::new(),
                },
            },
            base_dir: tmp.path().to_path_buf(),
            source: WorkloadSource::OnDisk,
            lab_inputs: None,
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
    /// (#3361) A registered fixture with an `eval_dir` at `dir`, defining
    /// workload `id` (bound to `satisfies` unless `requires` says otherwise).
    fn eval_fixture(dir: &Path, satisfies: &str, id: &str, requires: Option<&str>) {
        std::fs::create_dir_all(dir.join("eval/workloads")).unwrap();
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::write(
            dir.join(".fixture.json"),
            format!(r#"{{"name": "x", "satisfies": "{satisfies}", "eval_dir": "eval"}}"#),
        )
        .unwrap();
        std::fs::write(dir.join("src/a.js"), "a").unwrap();
        std::fs::write(dir.join("eval/mutants.txt"), "secret").unwrap();
        std::fs::write(
            dir.join(format!("eval/workloads/{id}.json")),
            format!(
                r#"{{"workload":{{"id":"{id}","provider":"prompt","prompt":"from the eval","requires_fixture":"{}"}}}}"#,
                requires.unwrap_or(satisfies)
            ),
        )
        .unwrap();
    }

    fn register_all(root: &Path, dirs: &[(&str, &Path)]) {
        use crate::lab::registry::{default_registry_path, LabRegistry};
        let mut registry = LabRegistry::default();
        for (name, d) in dirs {
            registry.register(d, Some((*name).to_string()), false).unwrap();
        }
        registry.save(&default_registry_path(&paths::DarkmuxPaths::under_root(root.to_path_buf()))).unwrap();
    }

    /// (#3386) A criterion's anchor is checked against the fixture the run
    /// will copy before any model runs, not first when the run is graded.
    #[test]
    #[serial_test::serial]
    fn a_criterion_anchor_missing_from_the_fixture_is_refused_before_the_run() {
        let tmp = TempDir::new().unwrap();
        let _home_guard = HomeGuard::set(tmp.path());
        let eval = tmp.path().join("eval-fx");
        eval_fixture(&eval, "pg@2.1", "pg", None);
        register_all(tmp.path(), &[("eval-fx", &eval)]);
        let write = |anchor: &str| {
            let doc = serde_json::json!({"workload": {"id": "pg", "provider": "coding-task", "prompt": "p",
                "requires_fixture": "pg@2.1", "verify": {"grade": {
                    "behaviors": {"asks": ["p"], "mutants": [
                        {"behavior": "a", "file": "src/a.js", "find": "a", "replace": "b", "criterion": "a-kept"}]},
                    "criteria": [{"id": "a-kept", "name": "A kept", "description": "a is kept",
                        "source": {"path": "src/a.js", "anchor": anchor}}]}}}});
            std::fs::write(eval.join("eval/workloads/pg.json"), doc.to_string()).unwrap();
        };
        let paths = paths::DarkmuxPaths::under_root(tmp.path().to_path_buf());
        write("a");
        check_criteria_anchors(&load("pg", Some(tmp.path())).unwrap(), &paths).unwrap();
        write("not in the file");
        let err = check_criteria_anchors(&load("pg", Some(tmp.path())).unwrap(), &paths).unwrap_err();
        let err = format!("{err:#}");
        assert!(err.contains("workload \"pg\"") && err.contains("occurs 0 times in src/a.js"), "{err}");
    }

    /// (#3363) An eval owns its roles: a workload bound to a fixture whose
    /// checkout lacks `roles/<id>.json` or `.md` is refused before any run
    /// directory is claimed or any model chosen, naming the missing file.
    /// Once the eval carries both, the check passes.
    #[test]
    #[serial_test::serial]
    fn a_workload_whose_eval_lacks_its_role_is_refused_before_the_run() {
        // The role is the provider's to name, as `main` registers them.
        let _ = crate::providers::register_builtins();
        let tmp = TempDir::new().unwrap();
        let _home_guard = HomeGuard::set(tmp.path());
        let eval = tmp.path().join("eval-fx");
        eval_fixture(&eval, "pg@2.1", "pg", None);
        register_all(tmp.path(), &[("eval-fx", &eval)]);
        let run_opts = || RunOpts {
            workload_id: "pg".into(),
            profile_name: None,
            runs: 1,
            config_path: None,
            quiet: true,
            loop_override: None,
            inject_context: None,
        };

        let err = format!("{:#}", lab_run(run_opts()).unwrap_err());
        // The prompt provider's default role; the registry canonicalizes the checkout's path.
        assert!(err.contains("eval-fx/roles/code-reviewer.json is missing"), "names the missing role file: {err}");
        assert!(err.contains("the eval must carry its roles"), "{err}");
        assert!(!tmp.path().join("runs").exists(), "refused before a run directory is claimed");

        std::fs::create_dir_all(eval.join("roles")).unwrap();
        let builtin = |file: &str| {
            std::fs::read_to_string(
                std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../templates/builtin/roles").join(file),
            )
            .unwrap()
        };
        std::fs::write(eval.join("roles/code-reviewer.json"), builtin("code-reviewer.json")).unwrap();
        std::fs::write(eval.join("roles/code-reviewer.md"), builtin("code-reviewer.md")).unwrap();
        let paths = paths::DarkmuxPaths::under_root(tmp.path().to_path_buf());
        let mut loaded = load("pg", Some(tmp.path())).unwrap();
        read_lab_inputs(&mut loaded, &paths).expect("the eval carries its role");
        assert!(matches!(loaded.lab_inputs, Some(darkmux_crew::dispatch::LabInputs::Eval(_))), "{:?}", loaded.lab_inputs);
    }

    #[test]
    #[serial_test::serial]
    fn an_eval_defined_workload_wins_over_a_user_copy_and_runs_on_its_own_fixture() {
        let tmp = TempDir::new().unwrap();
        let _home_guard = HomeGuard::set(tmp.path());
        // Another checkout satisfying the same requirement, registered first by name.
        let other = tmp.path().join("a-other");
        std::fs::create_dir_all(&other).unwrap();
        std::fs::write(other.join(".fixture.json"), r#"{"name": "a", "satisfies": "pg@2.1"}"#).unwrap();
        let eval = tmp.path().join("b-eval");
        eval_fixture(&eval, "pg@2.1", "pg", None);
        register_all(tmp.path(), &[("a-other", &other), ("b-eval", &eval)]);
        std::fs::create_dir_all(tmp.path().join("workloads")).unwrap();
        std::fs::write(
            tmp.path().join("workloads/pg.json"),
            r#"{"workload":{"id":"pg","provider":"prompt","prompt":"stale copy"}}"#,
        )
        .unwrap();

        let loaded = load("pg", Some(tmp.path())).unwrap();
        assert_eq!(loaded.source, crate::workloads::types::WorkloadSource::Eval);
        assert_eq!(loaded.manifest.workload.prompt.as_deref(), Some("from the eval"));
        let note = shadowed_note(&loaded, tmp.path()).expect("the user copy is named");
        assert!(note.contains("workloads/pg.json is ignored"), "{note}");
        let paths = paths::DarkmuxPaths::under_root(tmp.path().to_path_buf());
        assert_eq!(resolve_source_sandbox(&loaded, &paths).unwrap(), eval.canonicalize().unwrap());
    }

    #[test]
    #[serial_test::serial]
    fn an_eval_workload_must_bind_its_own_fixture() {
        let tmp = TempDir::new().unwrap();
        let _home_guard = HomeGuard::set(tmp.path());
        let eval = tmp.path().join("eval-fx");
        eval_fixture(&eval, "pg@2.1", "pg", Some("pg@2.0"));
        register_all(tmp.path(), &[("eval-fx", &eval)]);
        let err = load("pg", Some(tmp.path())).unwrap_err().to_string();
        assert!(err.contains("must bind its own fixture") && err.contains("\"pg@2.1\""), "{err}");
    }

    #[test]
    #[serial_test::serial]
    fn two_evals_defining_one_workload_are_refused() {
        let tmp = TempDir::new().unwrap();
        let _home_guard = HomeGuard::set(tmp.path());
        let (one, two) = (tmp.path().join("one"), tmp.path().join("two"));
        eval_fixture(&one, "pg@2.1", "pg", None);
        eval_fixture(&two, "pg@2.2", "pg", None);
        register_all(tmp.path(), &[("one", &one), ("two", &two)]);
        let err = load("pg", Some(tmp.path())).unwrap_err().to_string();
        assert!(err.contains("more than one registered fixture") && err.contains("`one`, `two`"), "{err}");
    }

    #[test]
    #[serial_test::serial]
    fn the_sandbox_never_holds_the_eval_folder_and_the_run_names_the_workload_it_used() {
        let tmp = TempDir::new().unwrap();
        let _home_guard = HomeGuard::set(tmp.path());
        let eval = tmp.path().join("eval-fx");
        eval_fixture(&eval, "pg@2.1", "pg", None);
        register_all(tmp.path(), &[("eval-fx", &eval)]);
        let per_run = tmp.path().join("run/sandbox");
        std::fs::create_dir_all(tmp.path().join("run")).unwrap();
        materialize_sandbox(&eval, &per_run, true).unwrap();
        assert!(per_run.join("src/a.js").is_file());
        assert!(!per_run.join("eval").exists(), "the grader's folder reached the model's sandbox");

        let run_dir = tmp.path().join("run");
        std::fs::write(run_dir.join(MANIFEST_FILE), r#"{"run_id": "r", "fixture": {"baseline_hash": "h"}}"#).unwrap();
        let home = crate::workloads::load::eval_homes().pop().unwrap();
        record_eval_workload(&run_dir, &home, "pg").unwrap();
        let written: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(run_dir.join(MANIFEST_FILE)).unwrap()).unwrap();
        let doc = std::fs::read(eval.join("eval/workloads/pg.json")).unwrap();
        assert_eq!(written["fixture"]["eval_dir"], "eval");
        assert_eq!(written["fixture"]["workload_path"], "eval/workloads/pg.json");
        assert_eq!(written["fixture"]["workload_sha256"], crate::lab::submit::sha256::hex(&doc));
        assert_eq!(written["fixture"]["baseline_hash"], "h");
    }

    /// (#3361 review) A checkout cannot point the eval's workload at a file
    /// outside it, or its eval folder at a folder the sandbox would keep.
    #[cfg(unix)]
    #[test]
    #[serial_test::serial]
    fn an_eval_cannot_reach_through_a_symlink() {
        use std::os::unix::fs::symlink;
        let tmp = TempDir::new().unwrap();
        let _home_guard = HomeGuard::set(tmp.path());
        let outside = tmp.path().join("outside.json");
        std::fs::write(&outside, r#"{"workload":{"id":"pg","provider":"prompt","prompt":"outside","requires_fixture":"pg@2.1"}}"#).unwrap();

        // The document itself is a link out of the checkout.
        let linked_doc = tmp.path().join("linked-doc");
        eval_fixture(&linked_doc, "pg@2.1", "pg", None);
        std::fs::remove_file(linked_doc.join("eval/workloads/pg.json")).unwrap();
        symlink(&outside, linked_doc.join("eval/workloads/pg.json")).unwrap();
        register_all(tmp.path(), &[("linked-doc", &linked_doc)]);
        let loaded = load("pg", Some(tmp.path()));
        assert!(
            loaded.as_ref().map_or(true, |l| l.source != crate::workloads::types::WorkloadSource::Eval),
            "a symlinked document must not load as the eval's"
        );
        let home = crate::workloads::load::eval_homes().pop().unwrap();
        assert!(home.document("pg").is_none());
        for id in ["../outside", "/etc/passwd", "a/b", ""] {
            assert!(home.document(id).is_none(), "{id}");
        }

        // The eval folder itself is a link: the clone refuses it.
        let linked_dir = tmp.path().join("linked-dir");
        std::fs::create_dir_all(linked_dir.join("secret")).unwrap();
        std::fs::write(linked_dir.join("secret/mutant.txt"), "secret").unwrap();
        std::fs::write(linked_dir.join(".fixture.json"), r#"{"name": "x", "satisfies": "pg@2.1", "eval_dir": "eval"}"#).unwrap();
        symlink("secret", linked_dir.join("eval")).unwrap();
        let per_run = tmp.path().join("run2/sandbox");
        std::fs::create_dir_all(tmp.path().join("run2")).unwrap();
        let err = materialize_sandbox(&linked_dir, &per_run, true).unwrap_err().to_string();
        assert!(err.contains("eval_dir") && err.contains("symlink"), "{err}");
        assert!(!per_run.join("secret").exists());
    }

    /// (#3361 review) The document that runs is the one that is recorded: a
    /// file must be named for the id it declares.
    #[test]
    #[serial_test::serial]
    fn an_eval_document_must_be_named_for_its_id() {
        let tmp = TempDir::new().unwrap();
        let _home_guard = HomeGuard::set(tmp.path());
        let eval = tmp.path().join("eval-fx");
        eval_fixture(&eval, "pg@2.1", "pg", None);
        std::fs::write(
            eval.join("eval/workloads/pg.json"),
            r#"{"workload":{"id":"other","provider":"prompt","prompt":"x","requires_fixture":"pg@2.1"}}"#,
        )
        .unwrap();
        register_all(tmp.path(), &[("eval-fx", &eval)]);
        let err = load("pg", Some(tmp.path())).unwrap_err().to_string();
        assert!(err.contains("declares id \"other\"") && err.contains("named for \"pg\""), "{err}");
    }

    /// (#3361 review) The preflight checks the fixture the eval workload runs
    /// on, not the first registered one satisfying the same requirement.
    #[test]
    #[serial_test::serial]
    fn the_preflight_checks_the_evals_own_fixture() {
        let tmp = TempDir::new().unwrap();
        let _home_guard = HomeGuard::set(tmp.path());
        let other = tmp.path().join("a-other");
        std::fs::create_dir_all(&other).unwrap();
        std::fs::write(other.join(".fixture.json"), r#"{"name": "a", "satisfies": "pg@2.1", "bogus_key": 1}"#).unwrap();
        let eval = tmp.path().join("b-eval");
        eval_fixture(&eval, "pg@2.1", "pg", None);
        register_all(tmp.path(), &[("a-other", &other), ("b-eval", &eval)]);
        let scope = darkmux_types::config_enum::Scope::LabRun;
        let loaded = load("pg", Some(tmp.path())).unwrap();
        assert_eq!(crate::user_files::preflight_workload(scope, None, Some(&loaded)), Ok(()), "a bad manifest on a fixture the run does not use refused it");

        std::fs::write(eval.join(".fixture.json"), r#"{"name": "x", "satisfies": "pg@2.1", "eval_dir": "eval", "bogus_key": 1}"#).unwrap();
        let refusal = crate::user_files::preflight_workload(scope, None, Some(&loaded)).unwrap_err().to_string();
        assert!(refusal.contains("bogus_key"), "the eval's own bad manifest went unchecked: {refusal}");
    }

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
        // The registry lookup inside `resolve_source_sandbox` resolves
        // independently via `ResolveScope::ForceUser`, NOT through the `paths`
        // argument below. Point `DARKMUX_HOME` at this same tmp root so the fixture this test registers at
        // `default_registry_path(&paths)` is the SAME file the resolver
        // actually reads.
        let _home_guard = HomeGuard::set(tmp.path());
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
                schema_version: None,
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
                    trials: None,
                    task_timeout_seconds: None,
                    chain_depths: None,
                    reply_room: None,
                    seed: None,
                    extras: BTreeMap::new(),
                },
            },
            base_dir: tmp.path().to_path_buf(),
            source: WorkloadSource::OnDisk,
            lab_inputs: None,
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
                schema_version: None,
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
                    trials: None,
                    task_timeout_seconds: None,
                    chain_depths: None,
                    reply_room: None,
                    seed: None,
                    extras: BTreeMap::new(),
                },
            },
            base_dir: tmp.path().to_path_buf(),
            source: WorkloadSource::OnDisk,
            lab_inputs: None,
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
        // (#2613) See the comment in `resolver_returns_registered_fixture_path_when_requires_matches`
        // — the registry lookup resolves independently via `ForceUser`, so
        // point `DARKMUX_HOME` at this test's tmp root.
        let _home_guard = HomeGuard::set(tmp.path());
        let paths = paths::DarkmuxPaths::under_root(tmp.path().to_path_buf());
        let loaded = LoadedWorkload {
            manifest: WorkloadManifest {
                schema_version: None,
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
                    trials: None,
                    task_timeout_seconds: None,
                    chain_depths: None,
                    reply_room: None,
                    seed: None,
                    extras: BTreeMap::new(),
                },
            },
            base_dir: tmp.path().to_path_buf(),
            source: WorkloadSource::OnDisk,
            lab_inputs: None,
        };
        let err = resolve_source_sandbox(&loaded, &paths).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("never-registered@1.0"), "got: {msg}");
        assert!(msg.contains("darkmux lab fixture register"), "got: {msg}");
        assert!(msg.contains("darkmux lab fixture list"), "got: {msg}");
        // (#2613) The registry error still names the exact path consulted —
        // that visibility guarantee from the #2590 follow-up is preserved,
        // it just now always names the home-tier registry rather than
        // hedging on which of two roots was consulted.
        assert!(
            msg.contains(&paths.root.join("lab-registry.json").display().to_string()),
            "error must name the registry path actually consulted: got: {msg}"
        );
    }

    /// (#2613) THE core regression: the fixture registry must resolve the
    /// SAME root regardless of a project-local `.darkmux/` in cwd — no
    /// `DARKMUX_HOME` override involved, just a genuine `HOME` plus a
    /// project directory that happens to have its own `.darkmux/`. Before
    /// this fix, `resolve_source_sandbox`'s registry lookup could land on a
    /// project-local registry; it must not.
    #[test]
    #[serial_test::serial]
    fn resolver_fixture_registry_ignores_project_local_darkmux_in_cwd() {
        use crate::workloads::types::{LoadedWorkload, WorkloadManifest, WorkloadSource, WorkloadSpec};
        use std::collections::BTreeMap;

        let real_home = TempDir::new().unwrap();
        let _real_home_guard = RealHomeGuard::set(real_home.path());

        // A project-local `.darkmux/` exists in cwd; it must change nothing.
        let project = TempDir::new().unwrap();
        std::fs::create_dir_all(project.path().join(".darkmux")).unwrap();
        let _cwd_guard = CwdGuard::new(project.path());

        let auto_paths = paths::resolve(ResolveScope::ForceUser);
        assert_eq!(auto_paths.scope, paths::Scope::User, "cwd's .darkmux/ is not the root");

        let loaded = LoadedWorkload {
            manifest: WorkloadManifest {
                schema_version: None,
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
                    trials: None,
                    task_timeout_seconds: None,
                    chain_depths: None,
                    reply_room: None,
                    seed: None,
                    extras: BTreeMap::new(),
                },
            },
            base_dir: project.path().to_path_buf(),
            source: WorkloadSource::OnDisk,
            lab_inputs: None,
        };
        // The registry lookup lands on the home tier.
        let err = resolve_source_sandbox(&loaded, &auto_paths).unwrap_err();
        let msg = format!("{err:#}");

        let home_registry = real_home.path().join(".darkmux").join("lab-registry.json");
        let bypassed_project_registry = project.path().join(".darkmux").join("lab-registry.json");

        assert!(
            msg.contains(&home_registry.display().to_string()),
            "must name the home-tier registry actually consulted: {msg}"
        );
        assert!(
            !msg.contains(&bypassed_project_registry.display().to_string()),
            "must not name the project-local registry — it was never consulted: {msg}"
        );
    }

    /// (#2613) The positive twin of the not-found test above: a fixture
    /// genuinely registered at the home root must actually RESOLVE — not
    /// merely fail-to-be-not-found — when the caller's cwd has its own
    /// project-local `.darkmux/`. The not-found test alone can't catch a
    /// bug where the registry lookup starts consulting the right PATH in
    /// its error text but still reads an empty/different registry at that
    /// path (e.g. a stray canonicalization mismatch between where
    /// `cmd_register` writes and where `resolve_source_sandbox` reads);
    /// only an assertion on the successful resolved path closes that gap.
    #[test]
    #[serial_test::serial]
    fn resolver_fixture_registry_resolves_from_project_local_cwd_when_registered_at_home() {
        use crate::lab::registry::{default_registry_path, LabRegistry};
        use crate::workloads::types::{LoadedWorkload, WorkloadManifest, WorkloadSource, WorkloadSpec};
        use std::collections::BTreeMap;

        let real_home = TempDir::new().unwrap();
        let _real_home_guard = RealHomeGuard::set(real_home.path());

        // Register the fixture at the home root BEFORE the project cwd is
        // set up — mimics `dm lab fixture register` run earlier from
        // anywhere, then a later dispatch from a project directory.
        let fixture_dir = real_home.path().join("my-fx");
        std::fs::create_dir_all(&fixture_dir).unwrap();
        std::fs::write(
            fixture_dir.join(".fixture.json"),
            r#"{"name": "my-fx", "satisfies": "demo-shape@1.0"}"#,
        )
        .unwrap();
        std::fs::write(fixture_dir.join("source.txt"), "baseline").unwrap();

        let home_paths = paths::resolve(ResolveScope::ForceUser);
        let mut registry = LabRegistry::default();
        registry.register(&fixture_dir, None, false).unwrap();
        registry.save(&default_registry_path(&home_paths)).unwrap();

        // NOW a project-local `.darkmux/` exists in cwd; it must change nothing.
        let project = TempDir::new().unwrap();
        std::fs::create_dir_all(project.path().join(".darkmux")).unwrap();
        let _cwd_guard = CwdGuard::new(project.path());

        let auto_paths = paths::resolve(ResolveScope::ForceUser);
        assert_eq!(auto_paths.scope, paths::Scope::User, "cwd's .darkmux/ is not the root");

        let loaded = LoadedWorkload {
            manifest: WorkloadManifest {
                schema_version: None,
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
                    requires_fixture: Some("demo-shape@1.0".into()),
                    verify: None,
                    expected: None,
                    image: None,
                    trials: None,
                    task_timeout_seconds: None,
                    chain_depths: None,
                    reply_room: None,
                    seed: None,
                    extras: BTreeMap::new(),
                },
            },
            base_dir: project.path().to_path_buf(),
            source: WorkloadSource::OnDisk,
            lab_inputs: None,
        };

        // The registry lookup finds the home-registered fixture.
        let resolved = resolve_source_sandbox(&loaded, &auto_paths).unwrap();
        let expected = fixture_dir.canonicalize().unwrap();
        assert_eq!(resolved, expected);
    }

    /// (#2613) `DARKMUX_HOME` still wins for the registry when set —
    /// even with a project-local `.darkmux/` also sitting in cwd. This is
    /// pinned here so a future change to the registry's resolution can't
    /// silently drop the override tier.
    #[test]
    #[serial_test::serial]
    fn resolver_fixture_registry_honors_darkmux_home_override() {
        use crate::workloads::types::{LoadedWorkload, WorkloadManifest, WorkloadSource, WorkloadSpec};
        use std::collections::BTreeMap;

        let dark_home = TempDir::new().unwrap();
        let _home_guard = HomeGuard::set(dark_home.path());

        let project = TempDir::new().unwrap();
        std::fs::create_dir_all(project.path().join(".darkmux")).unwrap();
        let _cwd_guard = CwdGuard::new(project.path());

        let paths = paths::resolve(ResolveScope::ForceUser);
        assert_eq!(
            paths.root, dark_home.path(),
            "sanity: DARKMUX_HOME must win over the project-local .darkmux/ \
             in cwd, or this test isn't exercising the state under test"
        );

        let loaded = LoadedWorkload {
            manifest: WorkloadManifest {
                schema_version: None,
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
                    trials: None,
                    task_timeout_seconds: None,
                    chain_depths: None,
                    reply_room: None,
                    seed: None,
                    extras: BTreeMap::new(),
                },
            },
            base_dir: project.path().to_path_buf(),
            source: WorkloadSource::OnDisk,
            lab_inputs: None,
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
    }

    /// (#2494) The enricher RAISES schema_version, never lowers it. It
    /// mints v4 to mean "has a `fixture` block", but runs AFTER the
    /// provider, which writes v5 when it recorded a `verify` object. An
    /// unconditional `insert(4)` silently downgraded those manifests, so a
    /// consumer gating on `>= 5` stopped believing a `verify` that was
    /// right there in the file.
    #[test]
    fn enrich_never_lowers_a_higher_schema_version_the_provider_wrote() {
        let tmp = TempDir::new().unwrap();
        let run_dir = tmp.path().join("run-v5");
        std::fs::create_dir_all(&run_dir).unwrap();
        let source_sandbox = tmp.path().join("source-sandbox");
        std::fs::create_dir_all(&source_sandbox).unwrap();
        std::fs::write(
            run_dir.join("manifest.json"),
            r#"{"schema_version": 5, "run_id": "r", "verify": {"passed": false, "details": "3 failing"}}"#,
        )
        .unwrap();

        enrich_manifest_with_fixture_info(&run_dir, Some("blake3:h"), &source_sandbox).unwrap();

        let parsed: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(run_dir.join("manifest.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(
            parsed["schema_version"], 5,
            "the enricher must not stamp its own v4 over a provider's v5"
        );
        assert_eq!(
            parsed["verify"]["passed"], false,
            "the verify object must survive enrichment"
        );
        assert!(parsed.get("fixture").is_some(), "fixture still added");
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

    /// (run manifest 1.1) The enricher records the darkmux that ran the run.
    #[test]
    fn enrich_records_the_darkmux_build_that_ran() {
        let tmp = TempDir::new().unwrap();
        let run_dir = tmp.path().join("run-1");
        std::fs::create_dir_all(&run_dir).unwrap();
        std::fs::write(run_dir.join("manifest.json"), r#"{"schema_version": 6, "run_id": "r"}"#).unwrap();
        enrich_manifest_with_fixture_info(&run_dir, None, tmp.path()).unwrap();
        let parsed: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(run_dir.join("manifest.json")).unwrap()).unwrap();
        assert_eq!(parsed["darkmux"]["version"], env!("CARGO_PKG_VERSION"));
        assert_eq!(
            serde_json::from_value::<darkmux_types::BuildInfo>(parsed["darkmux"].clone()).unwrap(),
            darkmux_types::build_info()
        );
        let live = darkmux_platform::current().os_release().map(|r| r.name);
        assert_eq!(parsed["os"].as_str().map(str::to_string), live, "the OS name is recorded with the run");
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
            r#"{"profiles":{"fast":{"models":[{"id":"model-a","n_ctx":32000}]}}}"#,
        )
        .unwrap();
        // (#2590) The workload user tier is forced HOME now, not cwd — a
        // `.darkmux/workloads/` planted in the shell's cwd (this test's
        // pre-#2590 setup) no longer resolves. Plant the workload manifest
        // under a `DARKMUX_HOME`-scoped home dir instead; `DARKMUX_HOME` IS
        // the darkmux root directly (no nested `.darkmux/`), so the
        // manifest lives at `<home>/workloads/q.json`. The RAII guard also
        // isolates `paths::resolve`'s root (used by `lab_run` for
        // run-artifact placement + `paths::ensure`).
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
        // The same fix wording `darkmux dispatch` gives (one builder).
        assert!(err.to_string().contains("Set `\"default_profile\": \"<name>\"`"), "{err}");
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
                run: &darkmux_types::session_id::RunId,
                on_session_id: &mut dyn FnMut(&darkmux_types::session_id::SessionId),
            ) -> Result<RunResult> {
                // Mirrors what `coding-task`/`prompt` do for real: mint,
                // report, THEN would dispatch. No real dispatch here.
                on_session_id(&darkmux_types::session_id::SessionId::adhoc(run.clone(), "stub", "darkmux-stub-2511-session-join-test"));
                Ok(RunResult {
                    escalation: None,
                    ok: true,
                    duration_ms: 1,
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
            r#"{"default_profile":"fast","profiles":{"fast":{"models":[{"id":"model-a","n_ctx":32000}]}}}"#,
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
            Some(darkmux_types::session_id::SessionId::adhoc(darkmux_types::session_id::RunId::lab(rec.run_id.clone()).unwrap(), "stub", "darkmux-stub-2511-session-join-test").wire().as_str()),
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
                run: &darkmux_types::session_id::RunId,
                on_session_id: &mut dyn FnMut(&darkmux_types::session_id::SessionId),
            ) -> Result<RunResult> {
                // Mirrors the REAL order in `coding_task.rs`/`prompt.rs`:
                // mint + report, THEN dispatch. The block below stands in
                // for the dispatch — real code would be calling into
                // `dispatch_via_internal` at exactly this point.
                on_session_id(&darkmux_types::session_id::SessionId::adhoc(run.clone(), "stub", "darkmux-stub-2511-ordering-test"));
                let (lock, cvar) = gate();
                let released = lock.lock().unwrap();
                let (_released, timeout) =
                    cvar.wait_timeout_while(released, Duration::from_secs(5), |r| !*r).unwrap();
                assert!(!timeout.timed_out(), "test thread never released the ordering gate");
                Ok(RunResult {
                    escalation: None,
                    ok: true,
                    duration_ms: 1,
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
            r#"{"default_profile":"fast","profiles":{"fast":{"models":[{"id":"model-a","n_ctx":32000}]}}}"#,
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
        assert_eq!(mid_flight.session_id.as_deref(), Some(darkmux_types::session_id::SessionId::adhoc(darkmux_types::session_id::RunId::lab(mid_flight.run_id.clone()).unwrap(), "stub", "darkmux-stub-2511-ordering-test").wire().as_str()));

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
                schema_version: None,
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
                    trials: None,
                    task_timeout_seconds: None,
                    chain_depths: None,
                    reply_room: None,
                    seed: None,
                    extras: BTreeMap::new(),
                },
            },
            base_dir: tmp.path().to_path_buf(),
            source: WorkloadSource::OnDisk,
            lab_inputs: None,
        };

        // With context → prepended ahead of the original prompt.
        let mut w = mk();
        let dir = w.base_dir.clone();
        apply_inject_context(&mut w, Some("<lessons>\n- rule\n</lessons>"), &dir);
        let p = w.manifest.workload.prompt.as_deref().unwrap();
        assert!(p.starts_with("<lessons>"), "context goes first: {p:?}");
        assert!(p.ends_with("Do the task."), "original prompt preserved: {p:?}");

        // None / blank → untouched (baseline arm + every non-A/B path).
        let mut w2 = mk();
        let dir2 = w2.base_dir.clone();
        apply_inject_context(&mut w2, None, &dir2);
        assert_eq!(w2.manifest.workload.prompt.as_deref(), Some("Do the task."));
        apply_inject_context(&mut w2, Some("   "), &dir2);
        assert_eq!(w2.manifest.workload.prompt.as_deref(), Some("Do the task."));
    }

    /// `replyRoom` and `verify.expect` are refused on any provider but
    /// `prompt`, naming each key set; a prompt workload passes.
    #[test]
    fn keys_only_the_prompt_provider_applies_are_refused_elsewhere() {
        let doc = |provider: &str| -> crate::workloads::types::LoadedWorkload {
            let text = serde_json::json!({"workload": {"id": "w", "provider": provider, "prompt": "x",
                "replyRoom": "context", "verify": {"expect": {"a": "1"}}}})
            .to_string();
            crate::workloads::types::LoadedWorkload {
                manifest: serde_json::from_str(&text).unwrap(),
                base_dir: std::path::PathBuf::from("/nonexistent"),
                source: crate::workloads::types::WorkloadSource::OnDisk,
                lab_inputs: None,
            }
        };
        assert!(refuse_keys_another_provider_ignores(&doc("prompt")).is_ok());
        let err = refuse_keys_another_provider_ignores(&doc("coding-task")).unwrap_err().to_string();
        assert!(err.contains("replyRoom and verify.expect applies only to the `prompt` provider"), "{err}");
    }

    /// (#3353) `verify.grade` is wired into the coding-task provider only, so
    /// any other provider refuses it rather than running ungraded.
    #[test]
    fn a_grader_is_refused_outside_the_coding_task_provider() {
        let doc = |provider: &str| -> crate::workloads::types::LoadedWorkload {
            let text = serde_json::json!({"workload": {"id": "w", "provider": provider, "prompt": "x",
                "verify": {"grade": {"reply": {"asks": ["x"], "required": true}}}}})
            .to_string();
            crate::workloads::types::LoadedWorkload {
                manifest: serde_json::from_str(&text).unwrap(),
                base_dir: std::path::PathBuf::from("/nonexistent"),
                source: crate::workloads::types::WorkloadSource::OnDisk,
                lab_inputs: None,
            }
        };
        assert!(refuse_keys_another_provider_ignores(&doc("coding-task")).is_ok());
        for provider in ["prompt", "tool-bench"] {
            let err = refuse_keys_another_provider_ignores(&doc(provider)).unwrap_err().to_string();
            assert!(err.contains("verify.grade is graded for the `coding-task` provider only so far"), "{err}");
        }
    }

    /// The with-context arm reads `promptFile` from the directory it is given
    /// (the bound fixture's checkout), not the workload document's.
    #[test]
    fn apply_inject_context_reads_the_prompt_file_where_it_is_told() {
        let tmp = TempDir::new().unwrap();
        let fixture = tmp.path().join("fixture");
        std::fs::create_dir_all(&fixture).unwrap();
        std::fs::write(fixture.join("in.txt"), "from the fixture").unwrap();
        let text = serde_json::json!({"workload": {"id": "w", "provider": "prompt", "promptFile": "in.txt",
            "requiresFixture": "f@1.0"}})
        .to_string();
        let mut w = crate::workloads::types::LoadedWorkload {
            manifest: serde_json::from_str(&text).unwrap(),
            base_dir: tmp.path().join("doc"),
            source: crate::workloads::types::WorkloadSource::OnDisk,
            lab_inputs: None,
        };
        apply_inject_context(&mut w, Some("CTX"), &fixture);
        assert_eq!(w.manifest.workload.prompt.as_deref(), Some("CTX\n\nfrom the fixture"));
    }

    /// A prompt workload bound to a fixture splices the A/B context into the
    /// run's own copy of the prompt file; any other workload splices it, as
    /// before, from beside the workload document.
    #[test]
    fn the_context_arm_reads_the_run_copy_only_for_a_fixture_bound_prompt_workload() {
        let tmp = TempDir::new().unwrap();
        let (doc, sandbox) = (tmp.path().join("doc"), tmp.path().join("run/sandbox"));
        std::fs::create_dir_all(&doc).unwrap();
        std::fs::create_dir_all(&sandbox).unwrap();
        std::fs::write(doc.join("in.txt"), "beside the document").unwrap();
        std::fs::write(sandbox.join("in.txt"), "the run's copy").unwrap();
        let load = |provider: &str| crate::workloads::types::LoadedWorkload {
            manifest: serde_json::from_value(serde_json::json!({"workload": {"id": "w", "provider": provider,
                "promptFile": "in.txt", "requires_fixture": "f@1.0"}}))
            .unwrap(),
            base_dir: doc.clone(),
            source: crate::workloads::types::WorkloadSource::OnDisk,
            lab_inputs: None,
        };
        let mut coding = load("coding-task");
        prepare_workload(&mut coding, Some("CTX")).unwrap();
        assert_eq!(coding.manifest.workload.prompt.as_deref(), Some("CTX\n\nbeside the document"));

        let mut prompt = load("prompt");
        prepare_workload(&mut prompt, Some("CTX")).unwrap();
        assert_eq!(prompt.manifest.workload.prompt, None, "nothing to read until the run's copy exists");
        let run = workload_for_run(&prompt, Some("CTX"), &sandbox);
        assert_eq!(run.manifest.workload.prompt.as_deref(), Some("CTX\n\nthe run's copy"));
        assert!(matches!(workload_for_run(&prompt, None, &sandbox), std::borrow::Cow::Borrowed(_)));
    }
}

#[cfg(test)]
#[path = "run_tests.rs"]
pub(crate) mod run_tests;
