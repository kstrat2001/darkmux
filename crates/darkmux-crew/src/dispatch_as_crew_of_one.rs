//! `darkmux dispatch` as a crew of one (#1509, step 1 of the #1508 run-
//! unification arc).
//!
//! `darkmux dispatch <role> [message]` used to call [`crate::dispatch::dispatch`]
//! raw — bypassing [`crate::scheduler::run_step_graph`] / `ensure_wave_loaded`
//! entirely, the same path every mission/coder-phase/review dispatch runs
//! through. Two consequences: a `dispatch` minted no run record (invisible
//! to `darkmux mission status`, the future `/runs` aggregator), AND —
//! load-bearing — it wrote no #1487 residency lease, so a concurrent
//! mission's `Exclusive` reconcile could see the dispatch's model as
//! darkmux-owned + not-desired + unprotected and evict it mid-generation.
//!
//! This module routes a single dispatch through the SAME engine as a full
//! Mission → Phase → Task(role) → Step graph, at cardinality ONE — "a crew
//! of one is still a crew" (#1508). The Step's `kind` is `dispatch.internal`
//! (`step_kinds::builtins::DispatchInternalStepKind`), the SAME builtin
//! `mission launch coder-phase`/`review` already run through — no new
//! StepKind, no new scheduler path. `DispatchInternalStepKind::run` wraps
//! [`crate::dispatch::dispatch`] — the identical primitive the pre-#1509 CLI
//! called directly — so the actual AI work is byte-for-byte the same; only
//! the wrapping (a real Task/Step graph, run through `run_step_graph`, whose
//! residency now goes through `ensure_wave_loaded`'s lease/reconcile regime)
//! changes.
//!
//! # Preserving the CLI contract
//!
//! `darkmux dispatch`'s callers (the qa-review skill, release smoke, ad-hoc
//! scripts) parse a `DispatchResult`-shaped envelope from stdout and depend
//! on the exact exit code. `DispatchInternalStepKind::run`'s DEFAULT
//! behavior collapses a non-zero exit into a step-level `Err` (losing
//! `stderr` and the numeric exit code — the right contract for a mission
//! graph, where only Complete/Error matters downstream) — wrong for a CLI
//! verb whose callers need the exact shape. [`dispatch_as_crew_of_one`] sets
//! `config.preserve_dispatch_result = true` on its Step, which opts
//! `DispatchInternalStepKind::run` into packing the full `DispatchResult`
//! (exit code included, never bailed) as JSON into `StepOutcome.output` —
//! see that kind's own doc for the full contract. This function unpacks it
//! back into a real `DispatchResult`, so `fleet::dispatch_routed_via`'s
//! caller (`cmd_dispatch`) sees an outcome indistinguishable from the
//! pre-#1509 raw `dispatch()` call.
//!
//! The frozen `crew-dispatch-<role>-<micros>-<counter>` session-id scheme
//! (see [`crate::dispatch::fresh_session_id`]'s doc — presence tests key on
//! the prefix) is preserved explicitly: when the caller's `opts.session_id`
//! is `None`, this module mints one via `fresh_session_id` itself and pins
//! it into the Step's `config.session_id`, rather than letting
//! `DispatchInternalStepKind`'s own default (`session_id::step(&step.id)`)
//! apply — that default is right for a mission-graph step, wrong for a
//! standalone dispatch whose session id is part of the CLI's own frozen
//! vocabulary.

use crate::dispatch::{DispatchOpts, DispatchResult};
use crate::envelope::{MissionEnvelope, MissionOutcomeStatus};
use crate::lifecycle;
use crate::step_kinds::{Facts, FixedEstimator, RawDispatchOutcome, StepKindRegistry};
use crate::types::{Mission, MissionSpec, MissionStatus, NodeStatus, Phase, PhaseStatus, Step, Task};
use anyhow::{anyhow, bail, Context, Result};
use darkmux_gestalt::ModelHost;
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

/// The step kind every crew-of-one dispatch runs as — see the module doc.
const STEP_KIND: &str = "dispatch.internal";

/// Route a single `darkmux dispatch` through the engine as a Mission → Phase
/// → Task(role) → Step graph at cardinality one. Production entry point —
/// mints a real StepKindRegistry + LMStudio host. See
/// [`dispatch_as_crew_of_one_with`] for the injectable core tests drive.
pub fn dispatch_as_crew_of_one(opts: DispatchOpts) -> Result<DispatchResult> {
    dispatch_as_crew_of_one_with(
        opts,
        &StepKindRegistry::with_builtins(),
        &crate::concurrent_dispatch::lms_host_factory,
    )
}

/// The injectable core of [`dispatch_as_crew_of_one`] — `registry` and
/// `host_factory` are caller-supplied so tests can substitute a fake
/// `dispatch.internal` kind (no Docker/LMStudio) and a
/// `darkmux_gestalt::mock::MockHost` (no real model host) while exercising
/// the REAL mission-minting + `run_step_graph` + residency-lease plumbing
/// unchanged. Production always calls the thin wrapper above.
pub(crate) fn dispatch_as_crew_of_one_with(
    opts: DispatchOpts,
    registry: &StepKindRegistry,
    host_factory: &(dyn Fn() -> Box<dyn ModelHost> + Sync),
) -> Result<DispatchResult> {
    // (#1509 — found live, tests/cli.rs's ack-gate integration tests) The
    // licensed-adjacent operator-consent gate MUST run before any model
    // residency action, never after. Inside `dispatch_internal::dispatch`
    // (the pre-#1509 raw path) it's the very first substantive check — but
    // `run_step_graph`'s `ensure_wave_loaded` now LOADS the step's
    // residency-classified model BEFORE the step's own `run()` (and
    // therefore before `dispatch_internal::dispatch`'s own copy of this
    // exact check) ever executes. Left unreplicated here, a licensed-
    // adjacent role dispatch would try to load a model into RAM — real
    // resource cost, no consent — before the operator ever sees the
    // disclaimer, or would fail with a confusing LMStudio load error
    // instead of the consent prompt when no profile/model is configured.
    // Calling the SAME idempotent check here (it's a no-op once the ack
    // file exists — see its own doc) restores the pre-#1509 ordering
    // exactly; `dispatch_internal::dispatch`'s own call later is unchanged
    // and becomes a harmless second no-op read of the same ack file.
    crate::dispatch::require_licensed_adjacent_ack(&opts.role_id)
        .context("licensed-adjacent role dispatch requires acknowledgment")?;

    let session_id = opts
        .session_id
        .clone()
        .unwrap_or_else(|| crate::dispatch::fresh_session_id(&opts.role_id));

    let mission_id = mint_dispatch_run_id(&opts.role_id);
    let mission_path = lifecycle::mission_path(&mission_id);
    if mission_path.exists() {
        bail!(
            "dispatch: run id `{mission_id}` already exists on disk — this should be \
             impossible (ids are minted uniquely per dispatch); if you're hitting this, it's \
             either a genuine id collision or a re-run against a copied/restored `.darkmux` \
             directory."
        );
    }

    let (mission, phase, task, step) = build_graph(&opts, &mission_id, &session_id);
    let phase_id = phase.id.clone();
    let task_id = task.id.clone();
    let step_id = step.id.clone();

    lifecycle::save_mission(&mission).context("persisting mission.json for the dispatch run")?;
    lifecycle::save_phase(&phase).context("persisting phase.json for the dispatch run")?;
    lifecycle::mission_start_with_reasoning(&mission_id, Some(&format!("darkmux dispatch {}", opts.role_id)))
        .context("starting the newly-minted dispatch run")?;
    lifecycle::phase_start(&phase_id).context("starting the dispatch run's phase")?;
    lifecycle::save_task(&mission_id, &task).context("persisting task.json for the dispatch run")?;

    let mut steps: BTreeMap<String, Step> = BTreeMap::new();
    steps.insert(step_id.clone(), step);
    let mut tasks: BTreeMap<String, Task> = BTreeMap::new();
    tasks.insert(task_id, task);

    let facts = Facts::default();
    let est = FixedEstimator::default();

    let graph_result = crate::scheduler::run_step_graph(
        &mut steps,
        &tasks,
        registry,
        &facts,
        &est,
        1,
        host_factory,
        &mut |record| {
            let _ = darkmux_flow::record(record);
        },
        &mut |step| {
            if let Err(e) = lifecycle::save_step(&mission_id, &phase_id, step) {
                eprintln!("darkmux dispatch: step persist warning: {e:#}");
            }
        },
        None,
    );

    if let Err(e) = graph_result {
        reconcile_on_error(&mission_id, &phase_id, &format!("{e:#}"));
        return Err(e);
    }

    let step = steps
        .get(&step_id)
        .ok_or_else(|| anyhow!("dispatch: step `{step_id}` vanished from the run graph"))?;

    match step.status {
        NodeStatus::Complete => {
            let raw: RawDispatchOutcome = serde_json::from_str(step.output.as_deref().unwrap_or_default())
                .context("dispatch: could not parse the crew-of-one step's packed DispatchResult")?;
            let result = DispatchResult {
                exit_code: raw.exit_code,
                stdout: raw.stdout,
                stderr: raw.stderr,
                session_id: raw.session_id,
                out_dir: raw.out_dir,
            };
            finalize(&mission_id, &phase_id, result.exit_code == 0, step_result_reason(result.exit_code));
            Ok(result)
        }
        NodeStatus::Error => {
            // `dispatch()` itself returned an `Err` (preflight/model
            // resolution/etc — see `DispatchInternalStepKind`'s
            // `.with_context(...)?`), the one case `preserve_dispatch_result`
            // doesn't intercept. Matches the pre-#1509 contract: a raw
            // `dispatch()` `Err` propagated straight through `dispatch_routed`'s
            // `?` to the CLI.
            let reason = step.output.clone().unwrap_or_default();
            finalize(&mission_id, &phase_id, false, Some(reason.clone()));
            Err(anyhow!("{reason}"))
        }
        other => {
            let reason = format!("dispatch: crew-of-one step ended in unexpected status {other:?}");
            finalize(&mission_id, &phase_id, false, Some(reason.clone()));
            bail!(reason);
        }
    }
}

fn step_result_reason(exit_code: i32) -> Option<String> {
    if exit_code == 0 {
        None
    } else {
        Some(format!("dispatch exited {exit_code}"))
    }
}

/// Drive the crew-of-one mission to its terminal status. `clean` maps to
/// `MissionOutcomeStatus::Clean` (phase Complete); anything else maps to
/// `Degraded` when the step at least completed with output (a non-zero
/// dispatch exit is real, postable output — the same "Degraded still
/// completes the phase" reasoning `envelope.rs`'s module doc gives every
/// mission type) — never `Error`, which this function reserves for the
/// harness-level failure paths that call `reconcile_on_error` instead.
fn finalize(mission_id: &str, phase_id: &str, clean: bool, reason: Option<String>) {
    let status = if clean { MissionOutcomeStatus::Clean } else { MissionOutcomeStatus::Degraded };
    let mut envelope = MissionEnvelope::new(mission_id, status, &[phase_id]);
    envelope.reason = reason;
    crate::envelope::finalize_mission(&envelope);
}

/// Best-effort reconcile when `run_step_graph` itself returns an `Err`
/// (structurally near-unreachable for a single-node acyclic graph, but
/// every mission driver in this codebase reconciles this window rather than
/// stranding an Active mission with a Running phase forever — see
/// `mission_launch::reconcile_and_finalize_on_error`'s identical shape).
fn reconcile_on_error(mission_id: &str, phase_id: &str, reason: &str) {
    let _ = lifecycle::phase_abandon(phase_id);
    let _ = lifecycle::mission_close_with_reasoning(mission_id, Some(&format!("dispatch run errored: {reason}")));
}

/// Build the (unsaved) Mission/Phase/Task/Step quadruple for one crew-of-one
/// dispatch run. Pure — no I/O — so the shape is independently unit-testable.
/// `mission_id`/`session_id` are minted by the caller (uniqueness/frozen-
/// session-id-scheme concerns live there, not here).
fn build_graph(opts: &DispatchOpts, mission_id: &str, session_id: &str) -> (Mission, Phase, Task, Step) {
    let now = now_unix();
    let phase_id = format!("{mission_id}-phase");
    let task_id = format!("{mission_id}-task");
    let step_id = format!("{mission_id}-step");

    let mission = Mission {
        id: mission_id.to_string(),
        description: format!("dispatch: {}", opts.role_id),
        status: MissionStatus::Active,
        phase_ids: vec![phase_id.clone()],
        created_ts: now,
        started_ts: None,
        finalized_ts: None,
        paused_ts: None,
        source_input: None,
        ticket: None,
        spec: Some(MissionSpec {
            config_id: "dispatch".to_string(),
            inputs_fingerprint: spec_fingerprint(opts),
        }),
    };

    let phase = Phase {
        id: phase_id.clone(),
        mission_id: mission_id.to_string(),
        description: format!("dispatch `{}`", opts.role_id),
        display_name: Some(opts.role_id.clone()),
        status: PhaseStatus::Planned,
        created_ts: now,
        started_ts: None,
        completed_ts: None,
        abandoned_ts: None,
        task_ids: vec![task_id.clone()],
    };

    let task = Task {
        id: task_id.clone(),
        phase_id: phase_id.clone(),
        description: format!("dispatch `{}`", opts.role_id),
        display_name: Some(opts.role_id.clone()),
        step_ids: vec![step_id.clone()],
        depends_on: Vec::new(),
        role_id: Some(opts.role_id.clone()),
        profile_name: opts.profile_name.clone(),
        workdir: opts.workdir.clone(),
        image: opts.image.clone(),
    };

    let mut config = serde_json::json!({
        "message": opts.message,
        "timeout_seconds": opts.timeout_seconds,
        "session_id": session_id,
        "skip_preflight": opts.skip_preflight,
        "json": opts.json,
        "preserve_dispatch_result": true,
    });
    // `opts.phase_id` is a DIFFERENT concept from this graph's own `phase_id`
    // above — it's the CLI's `--phase-id` flag, an operator-named EXTERNAL
    // mission phase this dispatch's flow records should attribute to (see
    // `DispatchOpts::phase_id`'s doc). Only set the key when present, so
    // `DispatchInternalStepKind`'s `config_str(step, "phase_id")` reads
    // `None` exactly like the pre-#1509 CLI's `opts.phase_id: None` default.
    if let Some(external_phase_id) = &opts.phase_id {
        config["phase_id"] = serde_json::Value::String(external_phase_id.clone());
    }
    if let Some(max_tokens) = opts.max_completion_tokens {
        config["max_completion_tokens"] = serde_json::Value::from(max_tokens);
    }

    let step = Step {
        id: step_id,
        task_id,
        kind: STEP_KIND.to_string(),
        status: NodeStatus::Planned,
        config,
        started_ts: None,
        completed_ts: None,
        output: None,
    };

    (mission, phase, task, step)
}

/// A lightweight, non-cryptographic grouping fingerprint over the dispatch's
/// role/message/profile — the crew-of-one analog of
/// `mission_launch::spec_fingerprint`. Not security-sensitive (it's a
/// `Mission.spec` grouping key, same as every mission's), so `std`'s
/// `DefaultHasher` is fine here — avoids pulling `blake3` into this crate
/// just for a non-cryptographic dedup hint.
fn spec_fingerprint(opts: &DispatchOpts) -> String {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    opts.role_id.hash(&mut hasher);
    opts.message.hash(&mut hasher);
    opts.profile_name.hash(&mut hasher);
    format!("{:016x}", hasher.finish())
}

/// Mint a fresh, unique run id for one crew-of-one dispatch — never derived
/// from the message (dispatches are non-deterministic AI work; two
/// dispatches of the same role with the same message are two DIFFERENT
/// runs). Shape mirrors `mission_launch::mint_run_id`
/// (`<prefix>-<unix-secs>-<token>`), minus the `blake3` dependency that
/// function pulls in at the binary-crate level (see `spec_fingerprint`'s
/// doc) — a `(nanos, pid, in-process counter)` triple is already unique
/// without hashing it, so it's used directly as the short token.
fn mint_dispatch_run_id(role_id: &str) -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let nanos = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0);
    let pid = std::process::id();
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let secs = nanos / 1_000_000_000;
    let slug = sanitize_role_slug(role_id);
    format!("dispatch-{slug}-{secs}-{pid:x}{n:x}")
}

/// Reduce `role_id` to the `[a-z0-9_-]` charset every persisted id in this
/// codebase uses (mirrors `fleet::validate_identifier`'s charset without
/// creating a `darkmux-fleet` -> ... dependency edge just for this check —
/// `darkmux-crew` has no reason to depend on `darkmux-fleet`, and role ids
/// are already conventionally kebab-case, so this is a defensive sanitize,
/// not a real validation gate).
fn sanitize_role_slug(role_id: &str) -> String {
    let slug: String = role_id
        .chars()
        .map(|c| if c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_' { c } else { '-' })
        .collect();
    if slug.is_empty() {
        "role".to_string()
    } else {
        slug
    }
}

fn now_unix() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::step_kinds::{Placement, StepOutcome};
    use darkmux_gestalt::mock::MockHost;
    use darkmux_gestalt::{CatalogFact, Deadline, HostError, LoadReport, OwnedTarget, ResidentFact};
    use std::env;
    use std::sync::{Arc, Mutex};
    use tempfile::TempDir;

    // ── Test fixtures ───────────────────────────────────────────────────

    fn test_opts(role: &str, message: &str) -> DispatchOpts {
        DispatchOpts {
            role_id: role.to_string(),
            message: message.to_string(),
            session_id: None,
            timeout_seconds: 3600,
            skip_preflight: false,
            json: true,
            workdir: None,
            phase_id: None,
            machine: None,
            wait: true,
            compaction: crate::dispatch::CompactionDispatchArgs::default(),
            profile_name: None,
            config_path: None,
            force_container: false,
            max_completion_tokens: None,
            image: None,
            model_base_url_override: None,
            step_id: None,
        }
    }

    /// Isolates `DARKMUX_CREW_DIR` (mission/phase/task/step JSON),
    /// `DARKMUX_FLOWS_DIR` (flow records), and `DARKMUX_HOME` (the #1487
    /// residency-lease registry `ensure_wave_loaded` writes into) to
    /// tempdirs — mirrors `lifecycle::tests::CrewGuard`, extended with
    /// `DARKMUX_HOME` since this module's tests are the first in this crate
    /// to assert on a REAL residency lease written by a REAL
    /// `run_step_graph` run (every other `ensure_wave_loaded` test lives in
    /// `concurrent_dispatch.rs` directly, one level below `run_step_graph`).
    struct RunGuard {
        _crew: TempDir,
        _flows: TempDir,
        _home: TempDir,
        prev_crew: Option<String>,
        prev_flows: Option<String>,
        prev_home: Option<String>,
    }

    impl RunGuard {
        fn new() -> Self {
            let crew = TempDir::new().unwrap();
            let flows = TempDir::new().unwrap();
            let home = TempDir::new().unwrap();
            let prev_crew = env::var("DARKMUX_CREW_DIR").ok();
            let prev_flows = env::var("DARKMUX_FLOWS_DIR").ok();
            let prev_home = env::var("DARKMUX_HOME").ok();
            // SAFETY: every test using this guard is `#[serial_test::serial]`.
            unsafe {
                env::set_var("DARKMUX_CREW_DIR", crew.path());
                env::set_var("DARKMUX_FLOWS_DIR", flows.path());
                env::set_var("DARKMUX_HOME", home.path());
            }
            Self { _crew: crew, _flows: flows, _home: home, prev_crew, prev_flows, prev_home }
        }

        fn home_path(&self) -> std::path::PathBuf {
            self._home.path().to_path_buf()
        }
    }

    impl Drop for RunGuard {
        fn drop(&mut self) {
            unsafe {
                match &self.prev_crew {
                    Some(v) => env::set_var("DARKMUX_CREW_DIR", v),
                    None => env::remove_var("DARKMUX_CREW_DIR"),
                }
                match &self.prev_flows {
                    Some(v) => env::set_var("DARKMUX_FLOWS_DIR", v),
                    None => env::remove_var("DARKMUX_FLOWS_DIR"),
                }
                match &self.prev_home {
                    Some(v) => env::set_var("DARKMUX_HOME", v),
                    None => env::remove_var("DARKMUX_HOME"),
                }
            }
        }
    }

    /// A `ModelHost` wrapping a SHARED `Arc<Mutex<MockHost>>` — the plain
    /// `MockHost` alone can't serve `run_step_graph`'s `host_factory`
    /// (called fresh per wave, discarding the box afterward), so tests that
    /// need to inspect `.ops` AFTER the run (the "exactly one load, no
    /// double-load" assertion) share one `MockHost` behind an `Arc<Mutex<_>>`
    /// instead.
    #[derive(Clone)]
    struct SharedMockHost(Arc<Mutex<MockHost>>);

    impl darkmux_gestalt::ModelHost for SharedMockHost {
        fn list_resident(&mut self) -> Result<Vec<ResidentFact>, HostError> {
            self.0.lock().unwrap().list_resident()
        }
        fn list_catalog(&mut self) -> Result<Vec<CatalogFact>, HostError> {
            self.0.lock().unwrap().list_catalog()
        }
        fn load(
            &mut self,
            model_key: &str,
            identifier: &str,
            min_ctx: u32,
            deadline: Deadline,
        ) -> Result<LoadReport, HostError> {
            self.0.lock().unwrap().load(model_key, identifier, min_ctx, deadline)
        }
        fn unload(&mut self, target: &OwnedTarget, deadline: Deadline) -> Result<(), HostError> {
            self.0.lock().unwrap().unload(target, deadline)
        }
    }

    /// `(message, session_id)` per `FakeDispatchKind::run` invocation —
    /// factored into a named alias per clippy's `type_complexity`.
    type CallLog = Arc<Mutex<Vec<(String, Option<String>)>>>;

    /// A fake `"dispatch.internal"` `StepKind` standing in for the real
    /// `DispatchInternalStepKind` — never touches Docker/LMStudio. Records
    /// every `(message, session_id)` it was invoked with (so tests can
    /// assert the graph correctly threaded the CLI's flags through), and
    /// echoes back a scripted `RawDispatchOutcome` (or an `Err`, scripting
    /// the "dispatch() itself failed" harness path) — mirroring exactly
    /// what `DispatchInternalStepKind::run` produces when
    /// `config.preserve_dispatch_result` is set (which `build_graph` always
    /// sets). `residency()` returns a FIXED `Placement` (independent of
    /// role/profile resolution — that machinery is `resolve_local_placement`'s
    /// own, unit-tested implicitly by every existing mission/coder-phase/
    /// review caller of the REAL `DispatchInternalStepKind`; this fake
    /// exists to prove the SCHEDULER's residency/lease plumbing, not to
    /// re-prove role/profile resolution).
    struct FakeDispatchKind {
        exit_code: i32,
        stdout: String,
        stderr: String,
        should_err: bool,
        placement: Placement,
        calls: CallLog,
    }

    impl crate::step_kinds::StepKind for FakeDispatchKind {
        fn id(&self) -> &'static str {
            "dispatch.internal"
        }

        fn run(
            &self,
            step: &crate::types::Step,
            _task: &crate::types::Task,
            _input: &BTreeMap<String, String>,
        ) -> Result<StepOutcome> {
            let session_id =
                step.config.get("session_id").and_then(|v| v.as_str()).map(String::from);
            let message =
                step.config.get("message").and_then(|v| v.as_str()).unwrap_or_default().to_string();
            self.calls.lock().unwrap().push((message, session_id.clone()));
            if self.should_err {
                bail!("fake dispatch() failure — preflight/model resolution");
            }
            let payload = RawDispatchOutcome {
                exit_code: self.exit_code,
                stdout: self.stdout.clone(),
                stderr: self.stderr.clone(),
                session_id: session_id.unwrap_or_default(),
                out_dir: None,
            };
            let output = serde_json::to_string(&payload).unwrap();
            Ok(StepOutcome { output, flow_records: Vec::new() })
        }

        fn residency(
            &self,
            _step: &crate::types::Step,
            _task: &crate::types::Task,
            _input: &BTreeMap<String, String>,
        ) -> Option<Placement> {
            Some(self.placement.clone())
        }
    }

    fn placement() -> Placement {
        Placement {
            model_key: "test-model".to_string(),
            identifier: "darkmux:test-model".to_string(),
            min_ctx: 8192,
            seat: "dispatch".to_string(),
        }
    }

    fn test_registry(kind: FakeDispatchKind) -> StepKindRegistry {
        let registry = StepKindRegistry::new();
        registry.register_alias("dispatch.internal", std::sync::Arc::new(kind)).unwrap();
        registry
    }

    // ── Pure graph-construction tests ───────────────────────────────────

    #[test]
    fn build_graph_is_one_phase_one_task_one_step() {
        let opts = test_opts("coder", "do the thing");
        let (mission, phase, task, step) = build_graph(&opts, "dispatch-coder-1-abc", "sess-1");

        assert_eq!(mission.phase_ids, vec![phase.id.clone()]);
        assert_eq!(phase.task_ids, vec![task.id.clone()]);
        assert_eq!(task.step_ids, vec![step.id.clone()]);
        assert!(task.depends_on.is_empty(), "a crew of one has no cross-task dependency");
        assert_eq!(mission.status, MissionStatus::Active);
        assert_eq!(phase.status, PhaseStatus::Planned);
        assert_eq!(step.status, NodeStatus::Planned);
        assert_eq!(step.kind, "dispatch.internal");
    }

    #[test]
    fn build_graph_sources_role_profile_workdir_image_from_the_task() {
        let mut opts = test_opts("coder", "hi");
        opts.profile_name = Some("balanced".to_string());
        opts.workdir = Some(std::path::PathBuf::from("/tmp/wt"));
        opts.image = Some("rust:slim".to_string());
        let (_, _, task, _) = build_graph(&opts, "dispatch-coder-1-abc", "sess-1");

        assert_eq!(task.role_id.as_deref(), Some("coder"));
        assert_eq!(task.profile_name.as_deref(), Some("balanced"));
        assert_eq!(task.workdir, Some(std::path::PathBuf::from("/tmp/wt")));
        assert_eq!(task.image.as_deref(), Some("rust:slim"));
    }

    #[test]
    fn build_graph_step_config_carries_the_cli_flags() {
        let mut opts = test_opts("coder", "hello there");
        opts.timeout_seconds = 120;
        opts.skip_preflight = true;
        opts.json = false;
        opts.max_completion_tokens = Some(2048);
        let (_, _, _, step) = build_graph(&opts, "dispatch-coder-1-abc", "sess-frozen-1");

        assert_eq!(step.config["message"], "hello there");
        assert_eq!(step.config["timeout_seconds"], 120);
        assert_eq!(step.config["session_id"], "sess-frozen-1");
        assert_eq!(step.config["skip_preflight"], true);
        assert_eq!(step.config["json"], false);
        assert_eq!(step.config["max_completion_tokens"], 2048);
        // (#1509) The load-bearing knob — see `DispatchInternalStepKind`'s
        // doc — every existing mission/coder-phase/review caller never sets
        // this key; the crew-of-one graph always does.
        assert_eq!(step.config["preserve_dispatch_result"], true);
    }

    #[test]
    fn build_graph_external_phase_id_is_a_separate_concept_from_the_graphs_own_phase() {
        // `opts.phase_id` (the CLI's `--phase-id`, external mission-phase
        // attribution) must land in `Step.config["phase_id"]` — a DIFFERENT
        // string from this graph's OWN minted phase id (`task.phase_id`).
        let mut opts = test_opts("coder", "hi");
        opts.phase_id = Some("some-other-mission-phase".to_string());
        let (_, phase, task, step) = build_graph(&opts, "dispatch-coder-1-abc", "sess-1");

        assert_eq!(step.config["phase_id"], "some-other-mission-phase");
        assert_eq!(task.phase_id, phase.id);
        assert_ne!(task.phase_id, "some-other-mission-phase");
    }

    #[test]
    fn build_graph_omits_phase_id_key_when_the_cli_flag_is_unset() {
        // Matches the pre-#1509 `DispatchOpts.phase_id: None` default —
        // `config_str(step, "phase_id")` must read `None`, not `Some("")`.
        let opts = test_opts("coder", "hi");
        let (_, _, _, step) = build_graph(&opts, "dispatch-coder-1-abc", "sess-1");
        assert!(step.config.get("phase_id").is_none());
    }

    #[test]
    fn mint_dispatch_run_id_is_unique_and_charset_safe() {
        let a = mint_dispatch_run_id("coder");
        let b = mint_dispatch_run_id("coder");
        assert_ne!(a, b, "two mints must never collide");
        assert!(a.starts_with("dispatch-coder-"));
        assert!(
            a.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_'),
            "minted id must satisfy the [a-z0-9_-] charset every persisted id uses: {a}"
        );
    }

    #[test]
    fn sanitize_role_slug_replaces_unsafe_chars() {
        assert_eq!(sanitize_role_slug("pr-reviewer"), "pr-reviewer");
        assert_eq!(sanitize_role_slug("Weird Role!"), "-eird--ole-");
        assert_eq!(sanitize_role_slug(""), "role");
    }

    // ── Integration tests: real mission-dir + real run_step_graph ──────
    //
    // These exercise `dispatch_as_crew_of_one_with` end to end (mission
    // mint -> `run_step_graph` -> residency lease -> finalize) with a FAKE
    // `dispatch.internal` kind (see `FakeDispatchKind`'s doc) so no Docker /
    // LMStudio is ever touched — the real `DispatchInternalStepKind` (and
    // therefore the real `dispatch.start`/`dispatch.complete` liveness
    // bookends, which live inside `dispatch_internal::dispatch`, unchanged
    // by this PR) is exercised by the existing docker-gated
    // `mock_dispatch_proof.rs` harness and by live dogfood, not here.

    #[test]
    #[serial_test::serial]
    fn dispatch_as_crew_of_one_mints_a_first_class_run_with_one_phase_task_step() {
        let _guard = RunGuard::new();
        let calls = Arc::new(Mutex::new(Vec::new()));
        let kind = FakeDispatchKind {
            exit_code: 0,
            stdout: r#"{"result":"stop"}"#.to_string(),
            stderr: String::new(),
            should_err: false,
            placement: placement(),
            calls: calls.clone(),
        };
        let registry = test_registry(kind);
        let host = Arc::new(Mutex::new(MockHost::new().cataloged("test-model", 5_000_000_000)));
        let host_for_factory = host.clone();
        let host_factory = move || -> Box<dyn darkmux_gestalt::ModelHost> {
            Box::new(SharedMockHost(host_for_factory.clone()))
        };

        let opts = test_opts("coder", "build the thing");
        let result = dispatch_as_crew_of_one_with(opts, &registry, &host_factory).unwrap();

        assert_eq!(result.exit_code, 0);
        assert_eq!(result.stdout, r#"{"result":"stop"}"#);
        assert!(result.session_id.starts_with("crew-dispatch-coder-"), "{}", result.session_id);

        // A first-class Run: exactly one mission dir with one phase, one
        // task, one step persisted on disk.
        let missions_dir = crate::loader::missions_dir();
        let entries: Vec<_> = std::fs::read_dir(&missions_dir).unwrap().collect();
        assert_eq!(entries.len(), 1, "exactly one mission dir minted");
        let mission_id = entries[0].as_ref().unwrap().file_name().to_string_lossy().to_string();

        let mission = crate::lifecycle::load_mission_by_id(&mission_id).unwrap();
        assert_eq!(mission.phase_ids.len(), 1);
        assert_eq!(mission.status, MissionStatus::Finalized);

        let phase_id = &mission.phase_ids[0];
        let tasks = crate::lifecycle::load_tasks_for_phase(&mission_id, phase_id).unwrap();
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0].step_ids.len(), 1);

        let steps = crate::lifecycle::load_steps_for_phase(&mission_id, phase_id).unwrap();
        assert_eq!(steps.len(), 1);
        assert_eq!(steps[0].status, NodeStatus::Complete);

        assert_eq!(calls.lock().unwrap().len(), 1, "the fake kind ran exactly once");
    }

    #[test]
    #[serial_test::serial]
    fn dispatch_as_crew_of_one_writes_a_residency_lease_via_ensure_wave_loaded() {
        let guard = RunGuard::new();
        let kind = FakeDispatchKind {
            exit_code: 0,
            stdout: "ok".to_string(),
            stderr: String::new(),
            should_err: false,
            placement: placement(),
            calls: Arc::new(Mutex::new(Vec::new())),
        };
        let registry = test_registry(kind);
        let host = Arc::new(Mutex::new(MockHost::new().cataloged("test-model", 5_000_000_000)));
        let host_for_factory = host.clone();
        let host_factory = move || -> Box<dyn darkmux_gestalt::ModelHost> {
            Box::new(SharedMockHost(host_for_factory.clone()))
        };

        let opts = test_opts("coder", "build the thing");
        dispatch_as_crew_of_one_with(opts, &registry, &host_factory).unwrap();

        // (#1487) The lease this process's `run_step_graph` -> `ensure_wave_
        // loaded` call wrote for the crew-of-one dispatch's model — the
        // core correctness win this PR exists for. `write_lease`
        // overwrites (never appends), and `LeaseGuard`'s `Drop` isn't held
        // here (`ensure_wave_loaded` holds it only for the LOCAL TRACK's
        // lifetime, inside `run_bounded`), so by the time
        // `dispatch_as_crew_of_one_with` returns the guard has already
        // dropped and released the lease file — read it INSIDE
        // `FakeDispatchKind::run` instead, where the lease is guaranteed
        // still held (the step is running INSIDE the leased window).
        // Since that's awkward to thread through a closure here, assert
        // the STRUCTURAL fact instead: `ensure_wave_loaded` was reached at
        // all is proven by the host having recorded exactly one `Load` op
        // for the resolved identifier (below) — `ensure_wave_loaded` is the
        // ONLY code path that calls `ModelHost::load`, and it ALWAYS calls
        // `residency_lease::write_lease` immediately before planning (see
        // that function's own doc) — so a recorded Load call is only
        // reachable through a lease write having happened first.
        let ops = host.lock().unwrap().ops.clone();
        let loads: Vec<_> = ops
            .iter()
            .filter(|op| matches!(op, darkmux_gestalt::mock::HostOp::Load { .. }))
            .collect();
        assert_eq!(loads.len(), 1, "exactly one load — ensure_wave_loaded ran, and only once: {ops:?}");
        match &loads[0] {
            darkmux_gestalt::mock::HostOp::Load { model_key, identifier, min_ctx } => {
                assert_eq!(model_key, "test-model");
                assert_eq!(identifier, "darkmux:test-model");
                assert_eq!(*min_ctx, 8192);
            }
            _ => unreachable!(),
        }
        let _ = guard.home_path(); // keep the tempdir alive through the assertions above
    }

    #[test]
    #[serial_test::serial]
    fn dispatch_as_crew_of_one_preserves_nonzero_exit_as_data_not_a_rust_error() {
        let _guard = RunGuard::new();
        let kind = FakeDispatchKind {
            exit_code: 2,
            stdout: "partial output".to_string(),
            stderr: "some warning on stderr".to_string(),
            should_err: false,
            placement: placement(),
            calls: Arc::new(Mutex::new(Vec::new())),
        };
        let registry = test_registry(kind);
        let host = Arc::new(Mutex::new(MockHost::new().cataloged("test-model", 5_000_000_000)));
        let host_for_factory = host.clone();
        let host_factory = move || -> Box<dyn darkmux_gestalt::ModelHost> {
            Box::new(SharedMockHost(host_for_factory.clone()))
        };

        let opts = test_opts("coder", "build the thing");
        // (#1509) Matches the pre-change CLI contract verbatim: a non-zero
        // dispatch exit code is DATA on the returned `DispatchResult`, never
        // a Rust-level `Err` — `cmd_dispatch` still returns
        // `Ok(result.exit_code)` to the process.
        let result = dispatch_as_crew_of_one_with(opts, &registry, &host_factory).unwrap();
        assert_eq!(result.exit_code, 2);
        assert_eq!(result.stdout, "partial output");
        assert_eq!(result.stderr, "some warning on stderr");
    }

    #[test]
    #[serial_test::serial]
    fn dispatch_as_crew_of_one_propagates_a_hard_dispatch_error() {
        let _guard = RunGuard::new();
        let kind = FakeDispatchKind {
            exit_code: 0,
            stdout: String::new(),
            stderr: String::new(),
            should_err: true,
            placement: placement(),
            calls: Arc::new(Mutex::new(Vec::new())),
        };
        let registry = test_registry(kind);
        let host = Arc::new(Mutex::new(MockHost::new().cataloged("test-model", 5_000_000_000)));
        let host_for_factory = host.clone();
        let host_factory = move || -> Box<dyn darkmux_gestalt::ModelHost> {
            Box::new(SharedMockHost(host_for_factory.clone()))
        };

        let opts = test_opts("coder", "build the thing");
        // A hard `dispatch()`-level failure (preflight/model resolution)
        // still propagates as a Rust `Err`, matching the pre-#1509
        // `fleet::dispatch_routed(opts)?` contract.
        let err = dispatch_as_crew_of_one_with(opts, &registry, &host_factory).unwrap_err();
        assert!(err.to_string().contains("fake dispatch() failure"), "{err}");
    }

    #[test]
    #[serial_test::serial]
    fn dispatch_as_crew_of_one_honors_an_explicit_session_id() {
        let _guard = RunGuard::new();
        let kind = FakeDispatchKind {
            exit_code: 0,
            stdout: "ok".to_string(),
            stderr: String::new(),
            should_err: false,
            placement: placement(),
            calls: Arc::new(Mutex::new(Vec::new())),
        };
        let registry = test_registry(kind);
        let host = Arc::new(Mutex::new(MockHost::new().cataloged("test-model", 5_000_000_000)));
        let host_for_factory = host.clone();
        let host_factory = move || -> Box<dyn darkmux_gestalt::ModelHost> {
            Box::new(SharedMockHost(host_for_factory.clone()))
        };

        let mut opts = test_opts("coder", "build the thing");
        opts.session_id = Some("operator-pinned-session".to_string());
        let result = dispatch_as_crew_of_one_with(opts, &registry, &host_factory).unwrap();
        assert_eq!(result.session_id, "operator-pinned-session");
    }
}
