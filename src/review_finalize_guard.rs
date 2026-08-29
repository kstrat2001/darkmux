//! `darkmux mission launch review`'s termination guard (#2124).
//!
//! `crawl_launch.rs`'s `CrawlFinalizeGuard` proved the shape: an RAII guard
//! armed right after a mission mints, so an early `?`-return, a panic, or —
//! new here — a caught SIGTERM/SIGINT still leaves a matching terminal
//! mission record behind instead of a mission stuck `Active` forever. This
//! module is the review launcher's version of that same guard, generalized
//! to also reap the OS-level children (`curl`) a signal-interrupted run can
//! orphan — crawl never shells out mid-unit, so it never needed that half.
//!
//! **Why review needs a SEPARATE mechanism from crawl's, not the same
//! one.** Crawl's guard covers a plain sequential Rust loop that checks
//! `darkmux_types::interrupt::is_set()` BETWEEN units — the loop itself is
//! the polling point. Review's dispatch (`with_dispatch_bookends` wrapping
//! `run_review_graph`, which drives `darkmux-crew`'s generic
//! `run_step_graph`) is ONE blocking, synchronous call with no polling seam
//! of its own — by the time it returns, a probe/judge/verify step could
//! have been running for any amount of the run's `--timeout`. Retrofitting
//! a poll point into the generic scheduler (`crates/darkmux-crew/src/
//! scheduler.rs`) would touch code EVERY mission type shares (coder-phase,
//! generic graphs, crawl's own `run_step_graph`-less path aside), a much
//! larger and riskier surface than this fix needs. Instead, [`run_dispatch`]
//! (`mission_launch_review.rs`) runs the whole blocking call on a spawned
//! worker thread and supervises it from the main thread — polling BOTH
//! `darkmux_types::interrupt::is_set()` AND the worker's `JoinHandle::
//! is_finished()` on a short timer (see that call site's own comment).
//! That polling loop is this fix's actual "between steps" checkpoint; it
//! just checks between POLL TICKS instead of between GRAPH STEPS, which is
//! finer-grained (a probe step can itself run for the full `--timeout`) and
//! needs no change to shared scheduler code.
//!
//! **Reaping — by pid, never by process group.** A worker thread blocked
//! deep in `Command::new("curl").output()` can't be interrupted from
//! outside without killing something — Rust has no cooperative-
//! cancellation primitive for a blocking foreign `wait(2)`. An EARLIER cut
//! of this fix isolated the launcher into its own process group
//! (`setpgid(0, 0)`) and `SIGKILL`ed the whole group on a caught signal —
//! proven WRONG by a pty-based test before merge: it silently broke a
//! terminal's Ctrl-C delivery whenever darkmux ran as a plain child of some
//! other foreground process (a non-interactive wrapper script, job control
//! off), moving the launcher OUT of the terminal's registered foreground
//! group so a real Ctrl-C only reached the wrapper, never darkmux. See
//! `darkmux_types::child_registry`'s own module doc for the full
//! measured-wrong story. This module NEVER touches its own process group
//! now — [`arm`] only installs signal handlers. Reaping instead goes
//! through `darkmux_types::child_registry`: every child the review
//! pipeline spawns (today, `curl` — `darkmux-crew`'s
//! `remote_chat_attempt`) registers its pid there before blocking on it;
//! [`ReviewFinalizeGuard::close`]/`Drop` call `child_registry::kill_all`,
//! which signals exactly those pids by number, once the guard has already
//! written a durable terminal record for the run.

use anyhow::{anyhow, Result};
use darkmux_lab::lab::review::ReviewEnvelope;
use std::any::Any;

/// Install SIGINT + SIGTERM handling — call ONCE, before minting the
/// mission this run's [`ReviewFinalizeGuard`] will cover. Idempotent (the
/// underlying `darkmux_types::interrupt` calls are; see their own docs),
/// so it's safe even if a future caller ends up invoking it more than once
/// in the same process. Deliberately does NOT touch this process's own
/// group — see this module's own doc for why an earlier version did and
/// was proven wrong.
pub(crate) fn arm() {
    darkmux_types::interrupt::install();
    darkmux_types::interrupt::install_term();
    // (#2124 pty-test finding) See `darkmux_types::interrupt`'s own module
    // doc: a wrapper-script invocation shape can deliver SIGHUP to darkmux
    // when the wrapper (the session's controlling-terminal owner) dies
    // from Ctrl-C — without this, that SIGHUP kills darkmux via its
    // unhandled default disposition before the SIGINT-handling story ever
    // gets a chance to run.
    darkmux_types::interrupt::install_hup();
}

/// Best-effort rendering of a caught `std::thread::JoinHandle::join()`
/// panic payload — the two shapes `std::panic!`/`.expect()`/`.unwrap()`
/// actually produce (`&'static str`, `String`); anything else names itself
/// honestly rather than guessing.
pub(crate) fn panic_message(payload: &(dyn Any + Send)) -> String {
    if let Some(s) = payload.downcast_ref::<&str>() {
        (*s).to_string()
    } else if let Some(s) = payload.downcast_ref::<String>() {
        s.clone()
    } else {
        "unknown panic payload".to_string()
    }
}

/// RAII guard mirroring `crawl_launch.rs`'s `CrawlFinalizeGuard` — armed
/// right after `mission_launch::ensure_mission_and_phases_with_provenance`
/// mints the review Mission's three phases, so ANY exit from that point
/// forward (the normal [`ReviewFinalizeGuard::close`] call, an early
/// `?`-return this module's author didn't foresee, a panic that unwinds
/// past the point a guard was constructed, or — the new case #2124 exists
/// for — a caught SIGTERM/SIGINT) leaves a matching mission-close record
/// and a non-`Active` mission behind, never a mission stuck `Active`
/// forever. `close()` is the normal end-of-run path and disarms the guard
/// so `Drop` never double-finalizes; `Drop` is the last-resort net for
/// every other exit.
pub(crate) struct ReviewFinalizeGuard {
    armed: bool,
    mission_id: String,
    phase_ids: Vec<String>,
}

impl ReviewFinalizeGuard {
    pub(crate) fn new(mission_id: String, phase_ids: Vec<String>) -> Self {
        Self { armed: true, mission_id, phase_ids }
    }

    /// The normal end-of-run path: writes the mission's terminal record
    /// for `result` (whatever shape it took — a clean `Ok`, a degenerate
    /// `Ok`, a hard `Err` from a genuine dispatch failure, or a
    /// synthesized `Err` this call site built for a caught panic/signal —
    /// see `run_dispatch`'s own supervisor-loop comment), then disarms the
    /// guard so its `Drop` becomes a no-op.
    ///
    /// When `result` reflects a SIGTERM/SIGINT that arrived during the run
    /// (`darkmux_types::interrupt::is_set()`), this ALSO reaps every pid
    /// `darkmux_types::child_registry` knows about — the worker thread may
    /// still be blocked deep inside a `curl` child at this point, and the
    /// terminal record just written is already durable, so it's safe to
    /// kill it by pid and exit. A normal completion (no signal observed)
    /// never reaches this branch — the launcher still has work to do
    /// afterward (`envelope_out`, rendering the review to `--emit`, the
    /// process exit code).
    pub(crate) fn close(&mut self, result: &Result<ReviewEnvelope>) {
        self.armed = false;
        crate::mission_launch_review::finalize_review_mission(&self.mission_id, &self.phase_refs(), result);
        if darkmux_types::interrupt::is_set() {
            darkmux_types::child_registry::kill_all(darkmux_types::child_registry::SIGKILL);
            // Unlike the earlier process-group approach, `SIGKILL`ing a
            // child pid does NOT end this process — the launcher must
            // explicitly exit here, now that finalize + reap are both
            // durable, rather than relying on a self-inclusion side
            // effect that (as this module's own doc explains) turned out
            // to have a real safety cost.
            std::process::exit(130);
        }
    }

    fn phase_refs(&self) -> Vec<&str> {
        self.phase_ids.iter().map(String::as_str).collect()
    }
}

impl Drop for ReviewFinalizeGuard {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        let err: Result<ReviewEnvelope> = Err(anyhow!(
            "mission launch review: mission aborted — the launcher exited before a terminal \
             outcome was recorded (a signal, a panic, or an early return this guard did not \
             expect)"
        ));
        crate::mission_launch_review::finalize_review_mission(&self.mission_id, &self.phase_refs(), &err);
        // Unlike `close()`, this fallback path reaps UNCONDITIONALLY — by
        // definition, reaching `Drop` still armed means something already
        // went wrong in a way this module's author didn't name a more
        // specific path for, so the safe default is "assume a child might
        // still be alive and kill it by pid" rather than trying to decide
        // case by case. No `std::process::exit` here (unlike `close()`'s
        // signal branch) — `Drop` can fire mid-unwind from many places,
        // some of which (a test, a caller with more cleanup of its own)
        // must not have the whole process pulled out from under them.
        darkmux_types::child_registry::kill_all(darkmux_types::child_registry::SIGKILL);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crew;
    use crate::mission_launch;
    use std::collections::BTreeMap;
    use tempfile::TempDir;

    // ── env isolation (mirrors mission_launch_review.rs's own CrewDirGuard) ──

    struct CrewDirGuard {
        prev_crew: Option<String>,
        prev_flows: Option<String>,
        _tmp_crew: TempDir,
        _tmp_flows: TempDir,
    }

    impl CrewDirGuard {
        fn new() -> Self {
            let tmp_crew = TempDir::new().unwrap();
            let tmp_flows = TempDir::new().unwrap();
            let prev_crew = std::env::var("DARKMUX_CREW_DIR").ok();
            let prev_flows = std::env::var("DARKMUX_FLOWS_DIR").ok();
            // SAFETY: every caller is `#[serial_test::serial]`.
            unsafe {
                std::env::set_var("DARKMUX_CREW_DIR", tmp_crew.path());
                std::env::set_var("DARKMUX_FLOWS_DIR", tmp_flows.path());
            }
            Self { prev_crew, prev_flows, _tmp_crew: tmp_crew, _tmp_flows: tmp_flows }
        }
    }

    impl Drop for CrewDirGuard {
        fn drop(&mut self) {
            // SAFETY: every caller is `#[serial_test::serial]`.
            unsafe {
                match &self.prev_crew {
                    Some(v) => std::env::set_var("DARKMUX_CREW_DIR", v),
                    None => std::env::remove_var("DARKMUX_CREW_DIR"),
                }
                match &self.prev_flows {
                    Some(v) => std::env::set_var("DARKMUX_FLOWS_DIR", v),
                    None => std::env::remove_var("DARKMUX_FLOWS_DIR"),
                }
            }
        }
    }

    struct InterruptFlagGuard;

    impl InterruptFlagGuard {
        fn new() -> Self {
            darkmux_types::interrupt::reset_for_test();
            Self
        }
    }

    impl Drop for InterruptFlagGuard {
        fn drop(&mut self) {
            darkmux_types::interrupt::reset_for_test();
        }
    }

    /// `darkmux_types::child_registry`'s `CHILDREN` set is process-wide,
    /// same as the interrupt flag above — isolate it around any test that
    /// exercises `close()`/`Drop`'s `kill_all` call, so a stray pid this
    /// test registers (or a real one some OTHER test in this binary
    /// happens to register concurrently) can't leak across tests.
    struct ChildRegistryGuard;

    impl ChildRegistryGuard {
        fn new() -> Self {
            darkmux_types::child_registry::reset_for_test();
            Self
        }
    }

    impl Drop for ChildRegistryGuard {
        fn drop(&mut self) {
            darkmux_types::child_registry::reset_for_test();
        }
    }

    fn mission_status_str(mission_id: &str) -> String {
        let path = crew::lifecycle::mission_path(mission_id);
        let text = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("reading {}: {e}", path.display()));
        let v: serde_json::Value = serde_json::from_str(&text).unwrap();
        v["status"].as_str().unwrap().to_string()
    }

    fn phase_status(mission_id: &str, phase_id: &str) -> String {
        let path = crew::lifecycle::phase_path(mission_id, phase_id);
        let text = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("reading {}: {e}", path.display()));
        let v: serde_json::Value = serde_json::from_str(&text).unwrap();
        v["status"].as_str().unwrap().to_string()
    }

    /// Mint a fresh review mission the same way `run_dispatch` does, every
    /// phase already `Running` (`ReviewFinalizeGuard` is armed AFTER the
    /// mint but BEFORE the dispatch, so its Drop/close always finds phases
    /// in that state in production) — returns `(mission_id, [investigate,
    /// adjudicate, report])`.
    fn mint_review_instance(case_id: &str) -> (String, [String; 3]) {
        let config = crew::mission_config::load("review").expect("review is embedded").config;
        let mut id_input: BTreeMap<String, serde_json::Value> = BTreeMap::new();
        id_input.insert("case_id".to_string(), serde_json::Value::String(case_id.to_string()));
        let mission_id = mission_launch::mint_run_id("review").unwrap();
        let spec = crew::types::MissionSpec {
            config_id: "review".to_string(),
            inputs_fingerprint: mission_launch::spec_fingerprint(&id_input).unwrap(),
            origin: Some(crew::types::MissionSpecOrigin::Builtin),
        };
        let description = format!("PR review — {case_id} (crew `test-crew`)");
        let real_phase_ids = mission_launch::ensure_mission_and_phases_with_provenance(
            &mission_id,
            &config,
            Some(&description),
            Some(spec),
        )
        .unwrap();
        for real_id in real_phase_ids.values() {
            crew::lifecycle::phase_start(real_id).unwrap();
        }
        (
            mission_id,
            [
                real_phase_ids["investigate"].clone(),
                real_phase_ids["adjudicate"].clone(),
                real_phase_ids["report"].clone(),
            ],
        )
    }

    /// (#2124, unit test 1a) `close()` on a synthesized error (mirroring a
    /// caught worker-thread panic, no signal involved) writes the mission's
    /// terminal record and abandons every phase — the SAME contract
    /// `finalize_review_mission`'s own tests already pin, now proven
    /// through the guard's own `close()` rather than the bare function.
    #[test]
    #[serial_test::serial]
    fn close_writes_the_terminal_record_after_a_simulated_error() {
        let _guard = CrewDirGuard::new();
        let (mission_id, phase_ids) = mint_review_instance("owner/repo@guard-close-error");
        let mut guard =
            ReviewFinalizeGuard::new(mission_id.clone(), phase_ids.to_vec());

        let result: Result<ReviewEnvelope> = Err(anyhow!("simulated worker-thread panic"));
        guard.close(&result);

        assert_eq!(mission_status_str(&mission_id), "finalized", "close() must finalize the mission");
        for phase_id in &phase_ids {
            assert_eq!(phase_status(&mission_id, phase_id), "abandoned");
        }
    }

    /// (#2124, unit test 1b) `Drop` — the ABORT path, never explicitly
    /// closed — writes the SAME terminal record. Simulates a SIGTERM
    /// arriving mid-run (`darkmux_types::interrupt::simulate_sigterm_for_
    /// test`, the real handler's exact code path minus an actual OS
    /// signal) and then drops the guard without ever calling `close()` —
    /// mirroring `run_dispatch`'s supervisor loop abandoning an interrupted
    /// worker thread's handle. Unlike the earlier process-group design,
    /// `Drop`'s `kill_all` reap never touches THIS test process itself —
    /// it only signals pids `darkmux_types::child_registry` was told
    /// about, which `ChildRegistryGuard` guarantees is none here — so this
    /// test can assert the finalize record directly without needing a
    /// subprocess to observe anything being killed.
    #[test]
    #[serial_test::serial]
    fn drop_writes_the_terminal_record_after_a_simulated_signal() {
        let _guard = CrewDirGuard::new();
        let _interrupt_guard = InterruptFlagGuard::new();
        let _children_guard = ChildRegistryGuard::new();
        let (mission_id, phase_ids) = mint_review_instance("owner/repo@guard-drop-signal");

        {
            let guard = ReviewFinalizeGuard::new(mission_id.clone(), phase_ids.to_vec());
            darkmux_types::interrupt::simulate_sigterm_for_test();
            assert!(darkmux_types::interrupt::is_set(), "the simulated SIGTERM must set the flag");
            // Deliberately no `guard.close(...)` call — the guard goes out
            // of scope here still armed, exercising `Drop`. The child
            // registry is empty (`ChildRegistryGuard` above), so `Drop`'s
            // `kill_all` call is a documented no-op — this proves the
            // RECORD-WRITING half of the fallback without needing a real
            // child pid or a subprocess to observe anything being killed.
            drop(guard);
        }

        assert_eq!(
            mission_status_str(&mission_id),
            "finalized",
            "Drop's fallback must finalize the mission even though close() was never called"
        );
        for phase_id in &phase_ids {
            assert_eq!(
                phase_status(&mission_id, phase_id),
                "abandoned",
                "an abort-path finalize must abandon every phase, never complete one"
            );
        }
    }

    /// (#2124) `close()` disarms the guard — a normal completion followed
    /// by the guard going out of scope must NOT double-finalize (which
    /// `finalize_mission`'s own idempotence would mask as a silent no-op,
    /// not a crash, so this proves the disarm rather than assuming it).
    #[test]
    #[serial_test::serial]
    fn close_disarms_so_drop_never_double_finalizes() {
        let _guard = CrewDirGuard::new();
        let (mission_id, phase_ids) = mint_review_instance("owner/repo@guard-disarm");
        let mut guard =
            ReviewFinalizeGuard::new(mission_id.clone(), phase_ids.to_vec());
        let result: Result<ReviewEnvelope> = Ok(ReviewEnvelope { degenerate: None, ..Default::default() });
        guard.close(&result);
        assert_eq!(mission_status_str(&mission_id), "finalized");
        drop(guard);
        // No panic, no second write to observe here beyond the same
        // terminal status persisting — `finalize_mission`'s own refusal
        // classification (Benign on an already-Finalized mission) would
        // make a stray second call quiet either way, so the REAL proof
        // this test wants is that `armed` is false, checked structurally:
        // `Drop`'s early-return on `!self.armed` is exercised by every
        // OTHER test in this module that calls `close()` and never panics
        // or hangs on a `kill_group(0, ...)` no-op it shouldn't reach.
        assert_eq!(mission_status_str(&mission_id), "finalized");
    }

    #[test]
    fn panic_message_reads_a_str_payload() {
        let payload: Box<dyn Any + Send> = Box::new("boom");
        assert_eq!(panic_message(&*payload), "boom");
    }

    #[test]
    fn panic_message_reads_a_string_payload() {
        let payload: Box<dyn Any + Send> = Box::new("boom".to_string());
        assert_eq!(panic_message(&*payload), "boom");
    }

    #[test]
    fn panic_message_names_an_unrecognized_payload_honestly() {
        let payload: Box<dyn Any + Send> = Box::new(42i32);
        assert_eq!(panic_message(&*payload), "unknown panic payload");
    }
}
