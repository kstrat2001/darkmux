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
//! Two more named call sites were, until #2651 landed, left UNCHANGED for a
//! different reason — not because they're already protected, but because
//! this module's lease-write contract (mirrored from `ensure_wave_loaded`/
//! `run_local_waves`) used to be a bare pid-keyed wholesale overwrite,
//! which was only safe when at most ONE dispatch was active in the process
//! at a time. `src/radio.rs` and `src/radio_answer.rs` route through
//! `darkmux acp`, an async (tokio) daemon that tracks dispatches
//! `in_flight` per session and can plausibly run more than one concurrently
//! in the same process.
//!
//! **#2651 (reproduced live, then fixed):** `src/acp_panel.rs`'s
//! ephemeral-panel dispatch path calls `crate::scheduler::run_step_graph`
//! in-process, which reaches `run_local_waves` → the SAME
//! `LeaseGuard::acquire()` + write this module uses. Two concurrent ACP
//! sessions each running an ephemeral panel dispatch DID race to overwrite
//! and delete each other's lease content — confirmed with a live
//! reproduction against the pre-fix code (see the #2651 PR), independent of
//! anything in this module. `darkmux_types::residency_lease` now unions
//! every concurrent in-process `LeaseGuard`'s own contribution instead of a
//! single caller-agnostic overwrite (its module doc has the full design),
//! and [`ensure_wave_loaded`](crate::concurrent_dispatch::ensure_wave_loaded)
//! (and this module's own call into it, above) both thread their own guard
//! through explicitly rather than resolving one implicitly by pid — so the
//! on-disk lease FILE is now correct no matter how many guards this
//! process holds concurrently: no clobber, no premature deletion, for
//! every caller of that primitive, this module included. **That was not
//! the same claim as "concurrent same-process dispatches can no longer
//! evict each other's model" — #2651 left that half open, tracked as
//! #2663.** `live_leased_models` excludes `own_pid` by construction (see
//! that function's own doc, correct for a single-dispatch process), so a
//! SIBLING same-process holder's contribution — correctly written into
//! the union on disk — was never read back by THIS process's own
//! reconcile; the `pinned` set `ensure_wave_loaded` planned against only
//! ever contained OTHER processes' leases. Confirmed live (#2662 review):
//! a sibling guard writing `darkmux:sibling`, then `ensure_wave_loaded`
//! for a different model in the SAME process, produced `[ListResident,
//! Unload { identifier: "darkmux:sibling" }, Load { .. }]`.
//!
//! **#2663 (fixed):** [`ensure_wave_loaded`](crate::concurrent_dispatch::ensure_wave_loaded)
//! (and this module's own call into it) now compute `pinned` via
//! [`residency_lease::LeaseGuard::all_live_leased_models`] instead of the
//! bare `live_leased_models(own_pid)` free function — it unions
//! `live_leased_models` (other processes) with every OTHER same-process
//! holder's own live contribution to `ACTIVE_LEASES`, excluding the
//! CALLING guard's own token (so a guard never pins its own
//! about-to-be-superseded placement against itself). A live same-process
//! sibling is now protected from this process's own Exclusive reconcile
//! choosing to pass-1-evict its model as not-desired, and a sibling that
//! has actually withdrawn (dropped cleanly, or via panic-unwind — `Drop`
//! runs on every exit path, #2651) stops being pinned the moment it
//! withdraws, never "pinned forever." **This left one eviction path open
//! at the time: `plan_acquire`'s per-desired `Reconcile` arm (unload +
//! reload at a higher context for the SAME model key) never consulted
//! `pinned` at all, so a live sibling resident at the same model key but
//! insufficient context could still be unloaded out from under it — filed
//! as #2669.**
//!
//! **#2669 (fixed, narrowed #2672):** the `Reconcile` arm now checks the
//! SAME `claimed` set (seeded from `pinned`, plus every earlier decision in
//! the same plan) before committing to an unload-then-reload. A claimed
//! stale resident Blocks the placement instead (`Reason::
//! ClaimedResidentInsufficientCtx`), and — ONLY when the claim's own
//! `clearable` field says its cause is an external pin that can genuinely
//! resolve with time (the pinning command finishing its own dispatch), not
//! a same-plan collision that no amount of waiting ever resolves (#2672
//! CONSIDER 3) — `ensure_wave_loaded` gives that specific Block the SAME
//! bounded hold-not-fail retry the analogous `HostFailed`+pinned shortfall
//! already got, rather than failing the wave on the very first attempt.
//! That retry window is a maximum of `BLOCKED_BY_HOLDER_RETRY_ATTEMPTS - 1`
//! sleeps of `BLOCKED_BY_HOLDER_RETRY_DELAY` each (600ms at the current
//! 3-attempt/300ms constants, not the ~900ms an earlier draft of this
//! module doc claimed — the attempt counter is pre-incremented, so the
//! loop sleeps twice and bails on the third) — honest about what it
//! actually covers: against a claim held for a WHOLE dispatch (seconds to
//! minutes), this window only ever helps a claim that was already nearly
//! done, not a genuine wait-out. Two profiles naming the same catalog
//! model at different contexts are therefore not fully "no longer
//! mutually exclusive" in general — what #2672 actually closes is the
//! narrower, common case of two siblings BOTH still racing to acquire the
//! identical identifier (neither loaded yet): exactly one leads the real
//! reconcile and the other converges to a plain reuse once it lands,
//! typically well inside that same window (see `residency_lease::
//! LeaseGuard::identifiers_i_should_lead`'s own doc). **Known residual
//! gap (#2672 CONSIDER 6, inherited from earlier work):** this protection
//! is seeded from `pinned` identifiers that pass a bare `darkmux:` prefix
//! check — a pin naming a genuinely live EXPLICIT ALIAS (a non-namespaced
//! identifier) never enters `claimed` at all, so it is not yet protected
//! by this arm; see `AcquireOpts.pinned`'s own doc for the reproduction.
//! Wiring `radio.rs`/`radio_answer.rs` through `dispatch_reconciled` is no
//! longer blocked on either the pass-1
//! not-desired hazard #2663 closed or this Reconcile-arm hazard — that
//! wiring itself remains a separate, unattempted follow-up.
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
            // run `Drop`, removing ONLY this guard's own contribution to
            // the process's lease (#2651 — never the whole shared file; see
            // `residency_lease`'s module doc) so a concurrent command's own
            // reconcile no longer sees this placement pinned. Matches
            // `run_local_waves`'s guard lifetime, scoped down from "one
            // local track" to "one standalone dispatch". Passed explicitly
            // into `ensure_wave_loaded` (#2651) rather than resolved by pid,
            // since a concurrent SAME-PROCESS caller (e.g. an ephemeral ACP
            // panel dispatch's own `run_local_waves` track) may hold its
            // OWN `LeaseGuard` at the same time — see this module's own
            // doc for why that same-process aggregation keeps the on-disk
            // lease FILE consistent across them (never clobbered, never
            // prematurely deleted). A same-process sibling's dispatch is
            // ALSO now safe from eviction by THIS reconcile (#2663):
            // `ensure_wave_loaded` computes its `pinned` set via
            // `LeaseGuard::all_live_leased_models`, which unions a
            // same-process sibling's own live contribution into `pinned`
            // (never this guard's own — see that method's doc).
            let lease_guard = residency_lease::LeaseGuard::acquire();
            let est = FixedEstimator::default();
            let mut host = host_factory();
            crate::concurrent_dispatch::ensure_wave_loaded(&[placement], &est, host.as_mut(), &lease_guard).with_context(
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
    ///
    /// **Ordering is asserted, not just presence (#2628 MUST FIX 3).**
    /// `dispatched == 1` and the final `ops` vector are both
    /// order-insensitive: a regression that ran `local_dispatch(opts)`
    /// BEFORE `ensure_wave_loaded(...)` would produce the identical final
    /// state (same dispatch count, same eventual ops) while actually
    /// dispatching against un-reconciled residency. The
    /// `ops_at_dispatch_time` snapshot, taken from INSIDE the injected
    /// `local_dispatch` closure, catches exactly that reordering — proven
    /// by swapping the two statements in `dispatch_reconciled_with` and
    /// observing this test fail (`left: []`, no reconcile ops yet present
    /// when the dispatch closure ran).
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
        // (#2628 MUST FIX 3) Snapshot the host's ops AT THE MOMENT
        // `local_dispatch` is called, not after the whole function
        // returns. `dispatched == 1` and the final `ops` vector are both
        // ORDER-INSENSITIVE — swapping `ensure_wave_loaded(...)` and
        // `local_dispatch(opts)` in `dispatch_reconciled_with` produces
        // the identical final state (one dispatch call, one reconcile,
        // same ops vector) even though the dispatch would then run
        // against UN-reconciled residency. This closure captures the
        // host's op log as of its own invocation, so a reordering is
        // caught here even though the end-state assertions below cannot
        // see it.
        let host_for_snapshot = host.clone();
        let ops_at_dispatch_time = Arc::new(Mutex::new(Vec::new()));
        let ops_at_dispatch_time_clone = ops_at_dispatch_time.clone();

        let result = dispatch_reconciled_with(
            test_opts("coder"),
            SeatClaim::LocalModel(placement("m", 8_000)),
            move |opts| {
                *ops_at_dispatch_time_clone.lock().unwrap() = host_for_snapshot.lock().unwrap().ops.clone();
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

        // The reconcile ops must already be present WHEN `local_dispatch`
        // fires — proving the ORDER, not just that both eventually ran.
        assert_eq!(
            *ops_at_dispatch_time.lock().unwrap(),
            vec![
                HostOp::ListResident,
                HostOp::Unload { identifier: "darkmux:orphan".to_string() },
                HostOp::Load { model_key: "m".to_string(), identifier: "darkmux:m".to_string(), min_ctx: 8_000 },
            ],
            "reconcile must complete BEFORE local_dispatch runs, not merely before this function returns"
        );

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
    ///
    /// **Near-miss fixture (#2628 CONSIDER 5).** `"not-darkmux:m"` sits
    /// right at the boundary: it CONTAINS the `darkmux:` namespace string
    /// but does not START WITH it. A crude widening of `is_darkmux_owned`
    /// from `starts_with(DARKMUX_NAMESPACE)` to
    /// `.contains(DARKMUX_NAMESPACE)` would misclassify this resident as
    /// darkmux-owned and evict it — `foreign-model` alone doesn't contain
    /// the namespace string at all, so that mutation would slip past it.
    /// This makes the test self-sufficient rather than relying on
    /// `darkmux-gestalt`'s own `is_darkmux_owned_detects_namespace` to
    /// catch that class upstream.
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
                // Near-miss: contains the namespace string but doesn't
                // START WITH it — see the doc above.
                .resident("not-darkmux:m", "not-darkmux:m", 32_000, Some(1_000))
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
            "neither non-namespaced resident (foreign-model, not-darkmux:m) may ever be unloaded: {:?}",
            locked.ops
        );
        assert!(
            locked.residents.iter().any(|r| r.identifier == "foreign-model"),
            "the foreign resident must still be present afterward: {:?}",
            locked.residents
        );
        assert!(
            locked.residents.iter().any(|r| r.identifier == "not-darkmux:m"),
            "the near-miss resident must still be present afterward: {:?}",
            locked.residents
        );
        assert!(
            locked.ops.iter().any(|op| matches!(
                op,
                HostOp::Load { identifier, .. } if identifier == "darkmux:m"
            )),
            "darkmux still loads its OWN namespaced copy alongside the untouched foreign ones: {:?}",
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
    #[serial_test::serial]
    #[test]
    fn dispatch_reconciled_production_entry_point_resolves_and_falls_through_for_an_unknown_role() {
        // (#2638 audit) `dispatch_reconciled` -> `resolve_local_seat` reads
        // `DARKMUX_HOME` (+ the `DARKMUX_CREW_DIR`/`DARKMUX_PROFILES`
        // overrides it derives from) through the same chokepoints its
        // sibling tests below guard with `LeaseTestEnv` + `#[serial]` — this
        // test called the real production entry point unguarded, so it
        // could observe a sibling test's tempdir mid-flight, or the
        // operator's real `~/.darkmux` on a machine with none of those set.
        let _env = LeaseTestEnv::new();
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
        // Two failures prove the same thing, and which one surfaces depends
        // on the machine rather than on this crate. With Docker present the
        // primitive gets as far as the role lookup and names the role. With
        // Docker absent — every `macos-latest` runner — the primitive's own
        // preflight bails first and names Docker. Either way the error came
        // from `crate::dispatch::dispatch`, which is the wiring this test
        // exists to pin; asserting only the role name made it pass locally
        // and fail in CI for a reason that had nothing to do with the fix.
        let named_the_role = msg.contains("this-role-does-not-exist-2628");
        let named_the_runtime_precondition = msg.contains("requires Docker");
        assert!(
            named_the_role || named_the_runtime_precondition,
            "the failure must come from the raw dispatch primitive (role lookup \
             or its Docker preflight), not from the reconcile layer: {msg}"
        );
    }

    /// Mirrors `residency_lease::residency_dir()`'s private path
    /// construction (`<DARKMUX_HOME>/residency/<pid>.lease`) so a test in
    /// THIS crate can assert on the lease file's existence without a new
    /// public accessor in `darkmux-types`. `DARKMUX_HOME` under
    /// `LeaseTestEnv` is always an absolute tempdir path, so no tilde
    /// expansion is needed here.
    fn lease_file_path(env: &LeaseTestEnv, pid: u32) -> std::path::PathBuf {
        env._tmp.path().join("residency").join(format!("{pid}.lease"))
    }

    /// **CONSIDER 4 (#2628): the lease guard's existence is asserted, not
    /// just its absence of a compile warning.** Deleting `let _lease_guard
    /// = residency_lease::LeaseGuard::acquire();` from
    /// `dispatch_reconciled_with`'s `LocalModel` arm leaves every other
    /// test in this module green — clippy's unused-import lint is the
    /// ONLY thing that would catch it today, and only if the import isn't
    /// used elsewhere. This test asserts the lease file for THIS process
    /// actually exists while the guard is conceptually held (via a
    /// `local_dispatch` that checks mid-call) and is gone once
    /// `dispatch_reconciled_with` returns normally.
    #[serial_test::serial]
    #[test]
    fn dispatch_reconciled_holds_and_releases_the_lease_on_normal_return() {
        let env = LeaseTestEnv::new();
        let pid = std::process::id();
        let path = lease_file_path(&env, pid);
        assert!(!path.exists(), "no lease file before the call: {path:?}");

        let host = Arc::new(Mutex::new(MockHost::new().cataloged("m", 1_000)));
        let factory = host_factory_over(host.clone());
        let path_for_closure = path.clone();

        let result = dispatch_reconciled_with(
            test_opts("coder"),
            SeatClaim::LocalModel(placement("m", 8_000)),
            move |opts| {
                // Mid-call: the lease guard is held and `LeaseGuard::write`
                // has already run as part of `ensure_wave_loaded` — the
                // file must exist right now, not just "eventually."
                assert!(path_for_closure.exists(), "lease file must exist while the guard is held: {path_for_closure:?}");
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

        assert!(!path.exists(), "lease file must be gone after a normal return: {path:?}");
    }

    /// **CONSIDER 4, panic-unwind half.** A `local_dispatch` that panics
    /// mid-call must still release the lease — `LeaseGuard::drop` runs on
    /// unwind, same as a normal return. Given three panic-path lease/lock
    /// leaks found in this repo this same week, this is asserted directly
    /// rather than assumed from `Drop`'s general contract.
    #[serial_test::serial]
    #[test]
    fn dispatch_reconciled_releases_the_lease_on_a_panicking_dispatch() {
        let env = LeaseTestEnv::new();
        let pid = std::process::id();
        let path = lease_file_path(&env, pid);
        assert!(!path.exists(), "no lease file before the call: {path:?}");

        let host = Arc::new(Mutex::new(MockHost::new().cataloged("m", 1_000)));
        let factory = host_factory_over(host.clone());

        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            dispatch_reconciled_with(
                test_opts("coder"),
                SeatClaim::LocalModel(placement("m", 8_000)),
                |_opts| panic!("W18A-CONSIDER-4: simulated dispatch panic"),
                factory.as_ref(),
            )
        }));
        assert!(outcome.is_err(), "the injected local_dispatch panic must propagate, not be swallowed");

        assert!(!path.exists(), "lease file must be released even when local_dispatch panics: {path:?}");
    }

    /// **#2663 coverage, named explicitly in this module's own doc**: this
    /// module's doc named `radio.rs`/`radio_answer.rs` as blocked on the
    /// same-process eviction hazard — two overlapping `session/prompt`
    /// tasks in a `darkmux acp` daemon, each reaching this function on its
    /// own thread. A live SAME-PROCESS sibling holder (its own
    /// `LeaseGuard`, still held, still resident) must never be evicted by
    /// a DIFFERENT `dispatch_reconciled_with` call's own Exclusive
    /// reconcile running in the same process.
    #[serial_test::serial]
    #[test]
    fn dispatch_reconciled_never_evicts_a_live_same_process_sibling_lease() {
        let _env = LeaseTestEnv::new();

        // The sibling: a second concurrent standalone dispatch in the
        // SAME process, holding its own guard for a different model,
        // still live (not dropped) for the whole test.
        let sibling_guard = residency_lease::LeaseGuard::acquire();
        sibling_guard.write(&["darkmux:sibling".to_string()]).expect("sibling writes its lease");

        let host = Arc::new(Mutex::new(
            MockHost::new().resident("darkmux:sibling", "sibling", 32_000, Some(1_000)).cataloged("m", 1_000),
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

        let ops = host.lock().unwrap().ops.clone();
        assert!(
            !ops.iter().any(|op| matches!(op, HostOp::Unload { identifier } if identifier == "darkmux:sibling")),
            "a live SAME-PROCESS sibling's model must never be evicted by a DIFFERENT \
             dispatch_reconciled call's own reconcile: {ops:?}"
        );
        assert!(
            ops.iter().any(|op| matches!(op, HostOp::Load { identifier, .. } if identifier == "darkmux:m")),
            "this dispatch's own wanted model still loads: {ops:?}"
        );

        drop(sibling_guard);
    }
}
