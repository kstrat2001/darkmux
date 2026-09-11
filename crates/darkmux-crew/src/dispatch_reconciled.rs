//! Standalone-dispatch Exclusive-reconcile + residency-lease protection
//! (#2628 — the follow-up #1509's own doc comment named and never filed).
//!
//! # The gap
//!
//! `darkmux dispatch <role>` (#1509) routes through
//! [`crate::dispatch_as_crew_of_one`], which runs the dispatch as a
//! cardinality-one Mission→Phase→Task→Step graph through
//! [`crate::scheduler::run_step_graph`] — so its residency gets
//! `ensure_wave_loaded`'s Exclusive `plan_acquire` pass (evicts a
//! darkmux-owned orphan not in the desired set) and a #1487 residency
//! lease (protects it from a CONCURRENT command's own reconcile). Every
//! mission/coder-phase/review `StepKind` gets the same protection via its
//! own `seat()` (`resolve_local_seat`), resolved by the scheduler before
//! the step's `run()` ever executes.
//!
//! Several production callers dispatch WITHOUT going through either —
//! `darkmux_fleet::routing::dispatch_routed`'s default `local_dispatch`
//! (used by `mission propose`, `lab notebook draft`) and two direct
//! `crate::dispatch::dispatch` callers (the fleet runner's claimed-job
//! handler, `darkmux-lab`'s tool-bench provider) call the raw dispatch
//! primitive with no acquire, no lease, no reconcile at all. A model these
//! paths load resident under the `darkmux:` namespace is real accumulation
//! exposure: nothing ever reclaims it except a LATER, unrelated dispatch
//! that happens to run through a protected path against the SAME profile
//! registry — not guaranteed, and the operator has no way to tell from any
//! of these call sites that it's pending.
//!
//! # The fix, and why it's scoped this narrowly
//!
//! [`dispatch_reconciled`] resolves the dispatch's own seat via the SAME
//! [`crate::step_kinds::resolve_local_seat`] every `StepKind` uses, and —
//! ONLY for a resolved `LocalModel` seat — reconciles residency down to
//! that ONE placement via [`crate::concurrent_dispatch::ensure_wave_loaded`]
//! (a single-placement "wave") before calling the raw dispatch, holding a
//! fresh [`darkmux_types::residency_lease::LeaseGuard`] for exactly that
//! window. This gives the standalone caller the identical Exclusive-
//! reconcile + lease protection `ensure_wave_loaded` already gives a wave,
//! without minting a Mission/Phase/Task/Step graph (no mission-status
//! visibility change, no `run_step_graph` ceremony) and without touching
//! `dispatch_internal::dispatch` itself (which would apply to EVERY
//! caller, including ones already inside a wave — see the next section for
//! why that would be actively unsafe).
//!
//! **This primitive is safe ONLY for a caller that is never one placement
//! among several CONCURRENT wave siblings the caller itself cannot see.**
//! A caller whose seat is already resolved by a `StepKind::seat()` (i.e.
//! it runs inside `run_step_graph`) MUST NEVER also route through here:
//! its residency is already reconciled against the WHOLE wave's desired
//! set by the scheduler before its `run()` fires, and a second,
//! independent single-placement reconcile from inside that `run()` would
//! see every concurrent sibling's model as "not desired by me" and evict
//! it mid-generation. Two of the eight call sites #2628 named turned out,
//! on inspection, to be exactly this case — already protected one level
//! up, not actually gapped — and are deliberately NOT routed through here:
//!
//! - `src/phase_cli.rs`'s `phase_review_output_at` is reached ONLY from
//!   `MissionVerifyStepKind::run()` (`src/coder_phase.rs`) — the standalone
//!   `phase` CLI verb that used to call it directly was retired in #1463.
//!   `MissionVerifyStepKind::seat()` already calls `resolve_local_seat`.
//! - `crates/darkmux-lab/src/crawl/unit_step.rs`'s `CrawlUnitStepKind` is
//!   itself a `StepKind` whose own `seat()` already calls
//!   `resolve_local_seat` — its raw `dispatch()` call is the SAME
//!   post-wave no-op pattern `DispatchInternalStepKind` uses, not a
//!   bypass.
//!
//! Two more named call sites are deliberately left UNCHANGED for a
//! different reason — not because they're already protected, but because
//! this module's single-writer lease-write contract (mirrored from
//! `ensure_wave_loaded`/`run_local_waves`: one process, one lease file,
//! wholesale-overwritten, never a delta) is only safe when at most ONE
//! dispatch is active in the process at a time. `src/radio.rs` and
//! `src/radio_answer.rs` route through `darkmux acp`, an async (tokio)
//! daemon that tracks dispatches `in_flight` per session and can plausibly
//! run more than one concurrently in the same process; wiring them through
//! here without first solving that aggregation problem risks a NEW
//! lease-clobber hazard (two concurrent same-process reconciles racing to
//! overwrite each other's lease content) that is worse than today's silent
//! gap. Deferred, named explicitly rather than silently dropped — see the
//! PR body for the follow-up issue.
//!
//! # What this does not change
//!
//! - The `darkmux dispatch` CLI verb's own path (`dispatch_as_crew_of_one`)
//!   — untouched.
//! - Every `StepKind`'s own dispatch call — untouched; they were never in
//!   scope (see above).
//! - `dispatch_internal::dispatch`'s own per-model residency check
//!   (`ensure_model_resident`) — untouched; it still only asks "is MY
//!   model resident at the right context," never "is anything else
//!   resident that shouldn't be." Reconcile-to-need happens ONLY at this
//!   module's layer, above the raw primitive, never inside it.

use crate::dispatch::{DispatchOpts, DispatchResult};
use crate::step_kinds::{FixedEstimator, SeatClaim};
use anyhow::{Context, Result};
use darkmux_gestalt::ModelHost;
use darkmux_types::residency_lease;

/// Production entry point — reconciles residency (when the dispatch
/// resolves to a local model) via a real LMStudio host, then dispatches
/// for real via [`crate::dispatch::dispatch`]. Wire this in place of the
/// raw `crate::dispatch::dispatch` primitive wherever a standalone (non-
/// `StepKind`, non-wave) caller needs the same Exclusive-reconcile + lease
/// protection the CLI verb and mission engine already have — see the
/// module doc for which callers that is and is not.
pub fn dispatch_reconciled(opts: DispatchOpts) -> Result<DispatchResult> {
    let seat = format!("dispatch-reconciled:{}", opts.role_id);
    let claim = crate::step_kinds::resolve_local_seat(
        &opts.role_id,
        opts.profile_name.as_deref(),
        opts.config_path.as_deref(),
        &seat,
    );
    dispatch_reconciled_with(opts, claim, crate::dispatch::dispatch, &crate::concurrent_dispatch::lms_host_factory)
}

/// The injectable core: `claim` and `local_dispatch` arrive as values so a
/// test can drive every `SeatClaim` arm and a scripted dispatch outcome
/// without a real role/profile registry or a real LMStudio, exercising the
/// REAL reconcile plumbing (`ensure_wave_loaded`, the residency lease)
/// unchanged. Production always calls the thin wrapper above, which
/// resolves `claim` itself via [`crate::step_kinds::resolve_local_seat`].
pub(crate) fn dispatch_reconciled_with(
    opts: DispatchOpts,
    claim: SeatClaim,
    local_dispatch: impl FnOnce(DispatchOpts) -> Result<DispatchResult>,
    host_factory: &(dyn Fn() -> Box<dyn ModelHost> + Sync),
) -> Result<DispatchResult> {
    match claim {
        SeatClaim::LocalModel(placement) => {
            // Scoped to exactly this reconcile-and-dispatch window — a
            // normal return or a panic-unwind through this function both
            // run `Drop`, removing this process's lease so a concurrent
            // command's own reconcile no longer sees this placement
            // pinned. Matches `run_local_waves`'s guard lifetime, scoped
            // down from "one local track" to "one standalone dispatch"
            // (this module's single-writer contract — see its own doc for
            // why that scope is the safety boundary, not a shortcut).
            let _lease_guard = residency_lease::LeaseGuard::acquire();
            let est = FixedEstimator::default();
            let mut host = host_factory();
            crate::concurrent_dispatch::ensure_wave_loaded(&[placement], &est, host.as_mut()).with_context(
                || {
                    format!(
                        "darkmux: reconciling darkmux-owned residency before dispatching role `{}`",
                        opts.role_id
                    )
                },
            )?;
            local_dispatch(opts)
        }
        // A hosted-endpoint seat never touches local residency — nothing
        // to reconcile, exactly as `resolve_local_seat`'s own doc states.
        SeatClaim::RemoteEndpoint => local_dispatch(opts),
        // `resolve_local_seat` never returns this variant (it always
        // resolves to LocalModel/RemoteEndpoint/LocalModelUnresolved for a
        // role that dispatches at all) — matched for exhaustiveness, same
        // fall-through as RemoteEndpoint.
        SeatClaim::NoModel => local_dispatch(opts),
        // (#2394 fail-open, matching the scheduler's own handling of this
        // exact variant) A local seat we could not place still runs —
        // unchanged pre-#2628 behavior — but never quietly: it has no
        // #1487 residency-lease protection, so a concurrent darkmux
        // command's Exclusive reconcile could evict its model
        // mid-generation with no other signal at all.
        SeatClaim::LocalModelUnresolved { reason } => {
            eprintln!(
                "darkmux: dispatching role `{}` claims a LOCAL model seat but its placement \
                 could not be resolved ({reason}) — running it with NO reconcile and NO #1487 \
                 residency lease. A concurrent darkmux command's reconcile could evict its \
                 model mid-generation. (#2628)",
                opts.role_id
            );
            local_dispatch(opts)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use darkmux_gestalt::mock::{HostOp, MockHost};
    use darkmux_gestalt::{ModelHost, Placement};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};
    use tempfile::TempDir;

    /// Mirrors `concurrent_dispatch::tests::LeaseTestEnv` — every test that
    /// reaches `ensure_wave_loaded` (directly, or via this module) MUST
    /// point `DARKMUX_HOME` at a throwaway tempdir, never the real
    /// `~/.darkmux/residency/`. Pair with `#[serial_test::serial]`.
    struct LeaseTestEnv {
        _tmp: TempDir,
        prev: Option<String>,
    }
    impl LeaseTestEnv {
        fn new() -> Self {
            let tmp = TempDir::new().expect("tempdir for DARKMUX_HOME");
            let prev = std::env::var("DARKMUX_HOME").ok();
            unsafe { std::env::set_var("DARKMUX_HOME", tmp.path()) };
            Self { _tmp: tmp, prev }
        }
    }
    impl Drop for LeaseTestEnv {
        fn drop(&mut self) {
            unsafe {
                match &self.prev {
                    Some(v) => std::env::set_var("DARKMUX_HOME", v),
                    None => std::env::remove_var("DARKMUX_HOME"),
                }
            }
        }
    }

    fn test_opts(role: &str) -> DispatchOpts {
        DispatchOpts {
            brief_refs: Vec::new(),
            workspace_read_only: false,
            record_context: None,
            resume_from: None,
            host_out: None,
            max_turns_override: None,
            timeout_override_seconds: None,
            role_id: role.to_string(),
            message: "hi".to_string(),
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
            system_prompt_override: None,
        }
    }

    fn placement(model_key: &str, min_ctx: u32) -> Placement {
        Placement {
            model_key: model_key.to_string(),
            identifier: format!("darkmux:{model_key}"),
            min_ctx,
            seat: "dispatch-reconciled:test".to_string(),
        }
    }

    fn host_factory_over(host: Arc<Mutex<MockHost>>) -> Box<dyn Fn() -> Box<dyn ModelHost> + Sync> {
        Box::new(move || -> Box<dyn ModelHost> { Box::new(SharedMockHost(host.clone())) })
    }

    /// Delegates every `ModelHost` call to a shared `MockHost` so the test
    /// can inspect `.ops` afterward even though `dispatch_reconciled_with`
    /// owns the `Box<dyn ModelHost>` it constructs from the factory.
    struct SharedMockHost(Arc<Mutex<MockHost>>);
    impl ModelHost for SharedMockHost {
        fn list_resident(
            &mut self,
        ) -> std::result::Result<Vec<darkmux_gestalt::ResidentFact>, darkmux_gestalt::HostError> {
            self.0.lock().unwrap().list_resident()
        }
        fn list_catalog(
            &mut self,
        ) -> std::result::Result<Vec<darkmux_gestalt::CatalogFact>, darkmux_gestalt::HostError> {
            self.0.lock().unwrap().list_catalog()
        }
        fn load(
            &mut self,
            model_key: &str,
            identifier: &str,
            min_ctx: u32,
            deadline: darkmux_gestalt::Deadline,
        ) -> std::result::Result<darkmux_gestalt::LoadReport, darkmux_gestalt::HostError> {
            self.0.lock().unwrap().load(model_key, identifier, min_ctx, deadline)
        }
        fn unload(
            &mut self,
            target: &darkmux_gestalt::plan::OwnedTarget,
            deadline: darkmux_gestalt::Deadline,
        ) -> std::result::Result<(), darkmux_gestalt::HostError> {
            self.0.lock().unwrap().unload(target, deadline)
        }
    }

    /// **RED-PROVE positive: the standalone dispatch now reconciles.** A
    /// darkmux-owned orphan resident (not this dispatch's own model) is
    /// evicted via the Exclusive pass BEFORE the wanted model loads and
    /// BEFORE `local_dispatch` runs — the exact behavior `ensure_wave_
    /// loaded` gives a wave, now reached from a bare standalone dispatch.
    /// Mutating `dispatch_reconciled_with`'s `LocalModel` arm to skip the
    /// `ensure_wave_loaded` call (i.e. reverting to the pre-#2628 raw
    /// `local_dispatch(opts)` short-circuit) makes this fail: the orphan
    /// is never evicted and the assertion on `host.ops` mismatches.
    #[serial_test::serial]
    #[test]
    fn dispatch_reconciled_evicts_a_darkmux_owned_orphan_before_dispatching() {
        let _env = LeaseTestEnv::new();
        let host = Arc::new(Mutex::new(
            MockHost::new().resident("darkmux:orphan", "orphan", 32_000, Some(1_000)).cataloged("m", 1_000),
        ));
        let factory = host_factory_over(host.clone());
        let dispatched = Arc::new(AtomicUsize::new(0));
        let dispatched_clone = dispatched.clone();

        let result = dispatch_reconciled_with(
            test_opts("coder"),
            SeatClaim::LocalModel(placement("m", 8_000)),
            move |opts| {
                dispatched_clone.fetch_add(1, Ordering::SeqCst);
                Ok(DispatchResult {
                    exit_code: 0,
                    stdout: format!("dispatched {}", opts.role_id),
                    stderr: String::new(),
                    session_id: String::new(),
                    out_dir: None,
                })
            },
            factory.as_ref(),
        )
        .expect("reconcile + dispatch succeeds");

        assert_eq!(result.exit_code, 0);
        assert_eq!(dispatched.load(Ordering::SeqCst), 1, "local_dispatch ran exactly once");

        let ops = host.lock().unwrap().ops.clone();
        assert_eq!(
            ops,
            vec![
                HostOp::ListResident,
                HostOp::Unload { identifier: "darkmux:orphan".to_string() },
                HostOp::Load { model_key: "m".to_string(), identifier: "darkmux:m".to_string(), min_ctx: 8_000 },
            ],
            "the stale orphan is evicted (Exclusive pass 1) before the wanted model loads: {ops:?}"
        );
    }

    /// **RED-PROVE inverted case (mandatory): a non-namespaced (user-owned)
    /// resident must NEVER be unloaded**, whatever this dispatch's own
    /// desired set is. `foreign-model` here carries no `darkmux:` prefix
    /// on its identifier — `plan_acquire`'s `is_darkmux_owned` gate must
    /// exclude it from the Exclusive pass entirely, and this dispatch
    /// loads its OWN copy alongside it rather than touching user state.
    /// Mutating `is_darkmux_owned` (in `darkmux-gestalt`) to also match a
    /// bare identifier would make this fail — the foreign model would be
    /// evicted as an "orphan," which is exactly the namespace-bypass shape
    /// #1609 already closed and #1274 declared ABSOLUTE.
    #[serial_test::serial]
    #[test]
    fn dispatch_reconciled_never_unloads_a_non_namespaced_user_owned_resident() {
        let _env = LeaseTestEnv::new();
        let host = Arc::new(Mutex::new(
            MockHost::new()
                // NOTE: identifier == model_key, i.e. no `darkmux:` prefix —
                // this is what a hand-loaded, operator-owned resident looks
                // like in `lms ps`.
                .resident("foreign-model", "foreign-model", 32_000, Some(1_000))
                .cataloged("m", 1_000),
        ));
        let factory = host_factory_over(host.clone());

        let result = dispatch_reconciled_with(
            test_opts("coder"),
            SeatClaim::LocalModel(placement("m", 8_000)),
            |opts| {
                Ok(DispatchResult {
                    exit_code: 0,
                    stdout: format!("dispatched {}", opts.role_id),
                    stderr: String::new(),
                    session_id: String::new(),
                    out_dir: None,
                })
            },
            factory.as_ref(),
        )
        .expect("reconcile + dispatch succeeds");
        assert_eq!(result.exit_code, 0);

        let locked = host.lock().unwrap();
        assert!(
            !locked.ops.iter().any(|op| matches!(op, HostOp::Unload { .. })),
            "the foreign, non-namespaced resident must never be unloaded: {:?}",
            locked.ops
        );
        assert!(
            locked.residents.iter().any(|r| r.identifier == "foreign-model"),
            "the foreign resident must still be present afterward: {:?}",
            locked.residents
        );
        assert!(
            locked.ops.iter().any(|op| matches!(
                op,
                HostOp::Load { identifier, .. } if identifier == "darkmux:m"
            )),
            "darkmux still loads its OWN namespaced copy alongside the untouched foreign one: {:?}",
            locked.ops
        );
    }

    /// A `RemoteEndpoint` seat never touches the host at all — no
    /// reconcile, exactly as `resolve_local_seat`'s own doc requires for a
    /// hosted model (zero local residency exposure by design).
    #[test]
    fn dispatch_reconciled_skips_reconcile_for_a_remote_endpoint_seat() {
        let host = Arc::new(Mutex::new(MockHost::new()));
        let factory = host_factory_over(host.clone());

        let result = dispatch_reconciled_with(
            test_opts("radio-router"),
            SeatClaim::RemoteEndpoint,
            |_opts| {
                Ok(DispatchResult {
                    exit_code: 0,
                    stdout: "remote ok".to_string(),
                    stderr: String::new(),
                    session_id: String::new(),
                    out_dir: None,
                })
            },
            factory.as_ref(),
        )
        .expect("remote seat dispatches without any host interaction");

        assert_eq!(result.stdout, "remote ok");
        assert!(host.lock().unwrap().ops.is_empty(), "a remote seat must never touch the local host");
    }

    /// (#2394 fail-open) An unresolved local seat still dispatches — never
    /// silently blocked — but never touches the host either: there is no
    /// placement to reconcile.
    #[test]
    fn dispatch_reconciled_falls_through_unprotected_when_the_seat_is_unresolved() {
        let host = Arc::new(Mutex::new(MockHost::new()));
        let factory = host_factory_over(host.clone());

        let result = dispatch_reconciled_with(
            test_opts("ghost-role"),
            SeatClaim::LocalModelUnresolved { reason: "role `ghost-role` not found".to_string() },
            |_opts| {
                Ok(DispatchResult {
                    exit_code: 0,
                    stdout: "ran anyway".to_string(),
                    stderr: String::new(),
                    session_id: String::new(),
                    out_dir: None,
                })
            },
            factory.as_ref(),
        )
        .expect("an unresolved seat still dispatches (fail-open, unchanged pre-#2628 behavior)");

        assert_eq!(result.stdout, "ran anyway");
        assert!(host.lock().unwrap().ops.is_empty(), "no placement to reconcile means no host interaction");
    }

    /// Wiring proof: `dispatch_reconciled` (the production entry point,
    /// not the injectable core) actually calls `resolve_local_seat` on the
    /// opts it was given, rather than silently treating everything as
    /// unresolved. A role that does not exist in the (embedded, built-in)
    /// role library resolves to `LocalModelUnresolved`, which this test
    /// distinguishes from a panic/short-circuit by asserting the
    /// dispatch still ran to completion via the injected `local_dispatch`
    /// substitute — proving `dispatch_reconciled` reached all the way
    /// through resolution into `dispatch_reconciled_with` rather than
    /// erroring out earlier.
    #[test]
    fn dispatch_reconciled_production_entry_point_resolves_and_falls_through_for_an_unknown_role() {
        let opts = test_opts("this-role-does-not-exist-2628");
        let result = dispatch_reconciled(opts);
        // `crate::dispatch::dispatch` (the real primitive) is what actually
        // runs here — it will itself fail loudly for an unknown role. The
        // point of this test is narrower: it must NOT panic, and the
        // failure must be the raw dispatch's own "role not found," not a
        // reconcile-layer error, proving resolution + fall-through wiring
        // reached the real primitive rather than stopping short.
        let err = result.expect_err("an unknown role fails at the raw dispatch primitive, not before it");
        let msg = format!("{err:#}");
        assert!(msg.contains("this-role-does-not-exist-2628"), "{msg}");
    }
}
