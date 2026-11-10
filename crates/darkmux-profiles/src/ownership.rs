//! darkmux's LMStudio ownership contract (#52, #1274): which loaded
//! instances are darkmux's (`darkmux:<model-id>` identifiers), and the one
//! sweep that unloads them.
//!
//! The pure namespace vocabulary lives in `darkmux_gestalt::ownership`;
//! this module adds the `&ProfileModel` form of [`namespaced_identifier`]
//! and the `lms`-backed [`eject_all_managed`]. (Named `swap` until 4.0,
//! after the retired stack-swap verb whose executor it once held.)

use darkmux_types::ProfileModel;

pub use darkmux_gestalt::is_darkmux_owned;

/// Compute the darkmux-namespaced LMStudio identifier for a profile model:
/// the `&ProfileModel` form of `darkmux_gestalt::namespaced_identifier`.
///
/// If the profile sets an explicit `identifier`, it passes through as-is
/// (the documented namespace opt-out). Otherwise the model id is wrapped
/// under the `darkmux:` namespace so unload-filtering can distinguish
/// darkmux's loads from user-managed ones.
pub fn namespaced_identifier(m: &ProfileModel) -> String {
    darkmux_gestalt::namespaced_identifier(&m.id, m.identifier.as_deref())
}

/// (#2774 tier 5) One model this sweep ejected (or, under `dry_run`, would
/// have).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EjectedModel {
    pub identifier: String,
    pub context: u64,
}

/// (#2774 tier 5) Outcome of an [`eject_all_managed`] sweep — everything
/// `darkmux machine eject`'s own printing needs, and everything the
/// thermal breaker's tier-5 hard-stop needs to record on its own event,
/// without either caller re-deriving the managed/user-state split itself.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct EjectSummary {
    pub ejected: Vec<EjectedModel>,
    pub user_loaded_count: usize,
    /// (#2774 review C1) Per-model `lms unload` failures — the ones that
    /// did NOT come out. Empty on a clean eject and on every `dry_run`.
    /// See [`eject_all_managed`]'s own doc for why a failure no longer
    /// aborts the rest of the sweep.
    pub failed: Vec<EjectFailure>,
}

/// (#2774 review C1) One model that refused to unload, and why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EjectFailure {
    pub identifier: String,
    pub error: String,
}

/// (#2774 tier 5) Pure split of a `lms ps` listing into darkmux-managed vs
/// user-loaded, by the namespace contract ([`is_darkmux_owned`]) — pulled
/// out of [`eject_all_managed`] so the partition itself is testable without
/// a real `lms` process. `eject_all_managed` and `darkmux machine eject`'s
/// `cmd_model_eject` both build on this ONE split, per the namespace
/// contract's "state-mutating operations only touch the namespaced subset"
/// rule (CLAUDE.md, #1274) — there is exactly one place that decides what
/// counts as "ours."
fn partition_by_ownership(loaded: &[darkmux_types::LoadedModel]) -> (Vec<&darkmux_types::LoadedModel>, usize) {
    let managed: Vec<_> = loaded.iter().filter(|m| is_darkmux_owned(&m.identifier)).collect();
    let user_loaded_count = loaded.len().saturating_sub(managed.len());
    (managed, user_loaded_count)
}

/// (#2774 tier 5) Unload every `darkmux:`-namespaced resident on THIS host
/// — the SAME mechanism `darkmux machine eject`'s `cmd_model_eject` already
/// used (this function is that mechanism, factored out so the thermal
/// breaker's tier-5 hard-stop can call it too, rather than a second
/// unloader growing beside it). User-loaded models are filtered out by
/// [`partition_by_ownership`] before anything is touched — structurally
/// off-limits, per the namespace contract (#1274), not by convention at
/// each call site.
///
/// `dry_run: true` reports what WOULD be ejected without calling
/// `lms unload` at all — same semantics as `machine eject --dry-run`.
///
/// Best-effort per model, and it CONTINUES past a failure (#2774 review
/// C1). An earlier shape propagated the first `lms unload` error with `?`,
/// which dropped the partially-filled `ejected` list along with it — so on
/// the tier-5 safety path (the machine is at `critical`, and this call is
/// what actually releases the GPU) one stuck resident stopped darkmux
/// trying on any of the others, and the artifact recorded nothing about
/// what HAD come out. Every managed resident is now attempted, the
/// per-model errors are collected into [`EjectSummary::failed`], and the
/// caller reports both lists. Nothing is swallowed: a caller that needs
/// "did this fully succeed" asks `summary.failed.is_empty()`.
///
/// The one remaining `Err` is a failure to LIST — with no listing there is
/// no set to act on, and continuing would mean guessing.
pub fn eject_all_managed(dry_run: bool) -> anyhow::Result<EjectSummary> {
    let engine = crate::engine::lmstudio();
    let loaded = engine.loaded()?;
    eject_managed_inner(
        &loaded,
        dry_run,
        &|id| engine.unload(id),
        &|| engine.loaded(),
    )
}

pub(crate) fn eject_managed_inner(
    loaded: &[darkmux_types::LoadedModel],
    dry_run: bool,
    unload: &dyn Fn(&str) -> anyhow::Result<()>,
    recheck_loaded: &dyn Fn() -> anyhow::Result<Vec<darkmux_types::LoadedModel>>,
) -> anyhow::Result<EjectSummary> {
    let (managed, user_loaded_count) = partition_by_ownership(loaded);
    let (mut ejected, mut failed) = eject_each(&managed, dry_run, unload);
    if !dry_run && !ejected.is_empty() {
        if let Ok(recheck) = recheck_loaded() {
            let still_resident: std::collections::HashSet<String> =
                recheck.into_iter().map(|m| m.identifier).collect();
            ejected = reconcile_still_resident(ejected, &mut failed, &still_resident);
        }
    }
    Ok(EjectSummary { ejected, user_loaded_count, failed })
}

fn reconcile_still_resident(
    ejected: Vec<EjectedModel>,
    failed: &mut Vec<EjectFailure>,
    still_resident: &std::collections::HashSet<String>,
) -> Vec<EjectedModel> {
    let mut actually_ejected = Vec::with_capacity(ejected.len());
    for m in ejected {
        if still_resident.contains(&m.identifier) {
            failed.push(EjectFailure {
                identifier: m.identifier,
                error: "model still resident after unload".to_string(),
            });
        } else {
            actually_ejected.push(m);
        }
    }
    actually_ejected
}

/// (#2774 review C1) The unload LOOP, with the unloader injected — so the
/// continue-past-a-failure behavior above is testable without a real `lms`
/// process. Pure apart from whatever `unload` does.
fn eject_each(
    managed: &[&darkmux_types::LoadedModel],
    dry_run: bool,
    unload: &dyn Fn(&str) -> anyhow::Result<()>,
) -> (Vec<EjectedModel>, Vec<EjectFailure>) {
    let mut ejected = Vec::with_capacity(managed.len());
    let mut failed = Vec::new();
    for m in managed {
        if !dry_run {
            // (#3083) An instance already gone is the end state the eject wants.
            let outcome = unload(&m.identifier)
                .or_else(|e| if crate::engine::is_not_resident(&e) { Ok(()) } else { Err(e) });
            if let Err(e) = outcome {
                failed.push(EjectFailure {
                    identifier: m.identifier.clone(),
                    error: format!("{e:#}"),
                });
                continue;
            }
        }
        ejected.push(EjectedModel { identifier: m.identifier.clone(), context: m.context });
    }
    (ejected, failed)
}

/// (#3450) One server darkmux started that an eject stopped, would stop
/// (`dry_run`), or kept because a running dispatch holds it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EjectedServer {
    pub endpoint: String,
    pub model: String,
    pub pid: u32,
}

/// (#3450) Outcome of an [`eject_servers`] sweep.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ServerEjectSummary {
    pub stopped: Vec<EjectedServer>,
    /// Left running: a running dispatch holds it (its residency lease).
    pub held: Vec<EjectedServer>,
    /// Would not stop; `identifier` is the endpoint id.
    pub failed: Vec<EjectFailure>,
}

impl ServerEjectSummary {
    pub fn is_empty(&self) -> bool {
        self.stopped.is_empty() && self.held.is_empty() && self.failed.is_empty()
    }
}

/// (#3450) Stop every server darkmux started on this machine (Tensorfold,
/// rec's Flint), the engine-server half of `darkmux machine eject`. A server
/// is darkmux's only while its record matches the live process (contract
/// 4), so a server darkmux did not start is never listed here, let alone
/// stopped. One a running dispatch holds is kept: each stop asks the
/// residency leases under the server's endpoint lock, as a dispatch's
/// eviction does (`server::stop_unless_held`).
pub fn eject_servers(dry_run: bool) -> ServerEjectSummary {
    let probe = darkmux_types::residency_lease::LeaseGuard::acquire();
    let dirs = crate::engine::server::Dirs::under_home();
    let held = dispatch_holds(&probe);
    eject_servers_in(&dirs, dry_run, crate::engine::server::Bounds::default(), &held)
}

/// Whether a running dispatch holds darkmux's server: its served name is in
/// another live holder's residency lease. `probe` is a guard of this
/// process's own that leases nothing, so every dispatch's lease counts,
/// in this process or another.
fn dispatch_holds(probe: &darkmux_types::residency_lease::LeaseGuard) -> impl Fn(&darkmux_types::engine_record::EngineRecord) -> bool + '_ {
    move |rec| probe.all_live_leased_models().contains(&crate::engine::server::served_name(rec))
}

pub(crate) fn eject_servers_in(
    dirs: &crate::engine::server::Dirs,
    dry_run: bool,
    bounds: crate::engine::server::Bounds,
    held: crate::engine::server::Held<'_>,
) -> ServerEjectSummary {
    use crate::engine::server;
    let mut summary = ServerEjectSummary::default();
    // darkmux's live servers, and those whose leader exited leaving workers
    // on the port (still darkmux's, and still holding memory).
    let ours = darkmux_types::engine_record::list_in(&dirs.records)
        .0
        .into_iter()
        .filter(|r| server::alive(r) || server::left_behind(dirs, &r.endpoint).is_some());
    for rec in ours {
        let entry = EjectedServer { endpoint: rec.endpoint.clone(), model: rec.model.clone(), pid: rec.pid };
        let outcome = if dry_run { Ok(!held(&rec)) } else { server::stop_unless_held(dirs, &rec, bounds, held) };
        match outcome {
            Ok(true) => summary.stopped.push(entry),
            Ok(false) => summary.held.push(entry),
            Err(e) => summary.failed.push(EjectFailure { identifier: entry.endpoint, error: e.to_string() }),
        }
    }
    summary
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn namespaced_identifier_uses_prefix_when_no_override() {
        let m = ProfileModel { max_output_tokens: None, engine_args: Vec::new(),
            endpoint: None,
            extras: Default::default(),
            id: "qwen3.6-35b-a3b".into(),
            n_ctx: Some(100_000),
            capabilities: Default::default(),
            identifier: None,
        };
        assert_eq!(namespaced_identifier(&m), "darkmux:qwen3.6-35b-a3b");
    }

    #[test]
    fn namespaced_identifier_passes_through_explicit_id() {
        let m = ProfileModel { max_output_tokens: None, engine_args: Vec::new(),
            endpoint: None,
            extras: Default::default(),
            id: "qwen3.6-35b-a3b".into(),
            n_ctx: Some(100_000),
            capabilities: Default::default(),
            identifier: Some("my-custom-alias".into()),
        };
        // Explicit override wins — operator opted out of the auto-namespace.
        assert_eq!(namespaced_identifier(&m), "my-custom-alias");
    }

    #[test]
    fn is_darkmux_owned_detects_namespace() {
        assert!(is_darkmux_owned("darkmux:qwen3.6-35b-a3b"));
        assert!(is_darkmux_owned("darkmux:anything-after"));
        // Non-namespaced ids are user state — off-limits.
        assert!(!is_darkmux_owned("qwen3.6-35b-a3b"));
        assert!(!is_darkmux_owned("user-loaded-model"));
        assert!(!is_darkmux_owned("my-custom-alias"));
        // Partial match isn't enough.
        assert!(!is_darkmux_owned("dark:foo"));
        assert!(!is_darkmux_owned("predarkmux:foo"));
    }

    fn loaded(identifier: &str) -> darkmux_types::LoadedModel {
        darkmux_types::LoadedModel {
            identifier: identifier.to_string(),
            model: identifier.to_string(),
            status: "loaded".to_string(),
            size: "18GB".to_string(),
            context: 100_000,
            queued: None,
        }
    }

    #[test]
    fn partition_by_ownership_splits_on_the_namespace_only() {
        let rows = vec![loaded("darkmux:qwen3.6-35b-a3b"), loaded("user-loaded-model"), loaded("darkmux:coder")];
        let (managed, user_loaded_count) = partition_by_ownership(&rows);
        assert_eq!(managed.len(), 2, "only the two darkmux: entries are ours to touch");
        assert_eq!(user_loaded_count, 1, "the bare identifier is user state, counted but untouched");
        assert!(managed.iter().all(|m| m.identifier.starts_with("darkmux:")));
    }

    #[test]
    fn partition_by_ownership_on_all_user_state_ejects_nothing() {
        let rows = vec![loaded("user-a"), loaded("user-b")];
        let (managed, user_loaded_count) = partition_by_ownership(&rows);
        assert!(managed.is_empty(), "no darkmux: entries — nothing is ours");
        assert_eq!(user_loaded_count, 2);
    }

    /// (#2774 review C1) The eject sweep must not stop at the first stuck
    /// model. This is the tier-5 safety path: the machine is at
    /// `critical`, and this call is what actually releases the GPU — an
    /// abort there leaves every model AFTER the failure resident, and
    /// (before the fix) threw away the record of the ones that had already
    /// come out.
    #[test]
    fn one_stuck_model_does_not_stop_the_rest_of_the_eject() {
        let rows = vec![loaded("darkmux:a"), loaded("darkmux:stuck"), loaded("darkmux:c")];
        let (managed, _) = partition_by_ownership(&rows);
        let (ejected, failed) = eject_each(&managed, false, &|id| {
            if id == "darkmux:stuck" {
                anyhow::bail!("lms unload failed: device busy");
            }
            Ok(())
        });
        let ejected_ids: Vec<&str> = ejected.iter().map(|m| m.identifier.as_str()).collect();
        assert_eq!(
            ejected_ids,
            vec!["darkmux:a", "darkmux:c"],
            "the model AFTER the failure must still be attempted and released"
        );
        assert_eq!(failed.len(), 1);
        assert_eq!(failed[0].identifier, "darkmux:stuck");
        assert!(failed[0].error.contains("device busy"), "the cause must survive: {:?}", failed[0]);
    }

    /// (#3083 review) `lms unload` of an instance that is already gone
    /// ("Model Not Found", exit 0) leaves the end state the eject wanted. It
    /// is not a failure: `machine eject` must not list it as one (and exit 1).
    #[test]
    fn eject_of_an_already_gone_model_is_not_a_failure() {
        let rows = vec![loaded("darkmux:gone")];
        let summary = eject_managed_inner(
            &rows,
            false,
            &|id| crate::lms::verify_unload_outcome(true, "Model Not Found\nCannot find a model with the identifier darkmux:gone", id),
            &|| Ok(Vec::new()),
        )
        .unwrap();
        assert!(summary.failed.is_empty(), "got {:?}", summary.failed);
        assert_eq!(summary.ejected.len(), 1);
    }

    #[test]
    fn a_clean_eject_reports_no_failures_and_a_dry_run_attempts_nothing() {
        let rows = vec![loaded("darkmux:a"), loaded("darkmux:b")];
        let (managed, _) = partition_by_ownership(&rows);

        let (ejected, failed) = eject_each(&managed, false, &|_| Ok(()));
        assert_eq!(ejected.len(), 2);
        assert!(failed.is_empty());

        let attempts = std::cell::RefCell::new(0u32);
        let (would_eject, failed) = eject_each(&managed, true, &|_| {
            *attempts.borrow_mut() += 1;
            anyhow::bail!("a dry run must never reach the unloader");
        });
        assert_eq!(*attempts.borrow(), 0, "dry_run calls the unloader zero times");
        assert_eq!(would_eject.len(), 2, "a dry run still reports what WOULD come out");
        assert!(failed.is_empty());
    }

    #[test]
    fn eject_sweep_rechecks_residency_and_fails_still_resident_models() {
        let rows = vec![loaded("darkmux:ghost"), loaded("darkmux:gone")];
        // ghost is still reported by recheck_loaded
        let recheck = vec![loaded("darkmux:ghost")];
        let summary = eject_managed_inner(
            &rows,
            false,
            &|_| Ok(()),
            &|| Ok(recheck.clone()),
        ).expect("sweep completes");

        assert_eq!(summary.ejected.len(), 1);
        assert_eq!(summary.ejected[0].identifier, "darkmux:gone");
        assert_eq!(summary.failed.len(), 1);
        assert_eq!(summary.failed[0].identifier, "darkmux:ghost");
        assert!(summary.failed[0].error.contains("still resident"));
    }

    /// A dry run unloads nothing, so it has nothing to recheck: the residents
    /// it WOULD eject are still listed, and must not come back as failures.
    #[test]
    fn a_dry_run_reports_what_would_come_out_without_rechecking_residency() {
        let rows = vec![loaded("darkmux:a"), loaded("user-model")];
        let rechecks = std::cell::Cell::new(0u32);
        let summary = eject_managed_inner(&rows, true, &|_| anyhow::bail!("a dry run never unloads"), &|| {
            rechecks.set(rechecks.get() + 1);
            Ok(rows.clone())
        })
        .unwrap();
        assert_eq!(rechecks.get(), 0, "a dry run must not recheck residency");
        let ids: Vec<&str> = summary.ejected.iter().map(|m| m.identifier.as_str()).collect();
        assert_eq!(ids, vec!["darkmux:a"]);
        assert!(summary.failed.is_empty(), "nothing was attempted, nothing failed: {:?}", summary.failed);
        assert_eq!(summary.user_loaded_count, 1);
    }

    /// Writes a throwaway executable that impersonates `lms` (reached through
    /// `DARKMUX_LMS_BIN`, never the operator's real LMStudio). `ps --json`
    /// lists `darkmux:a` until an `unload darkmux:a` is logged, and always
    /// lists `user-model`; every `unload` argument is appended to the log.
    fn fake_lms_with_two_residents(dir: &std::path::Path) -> (std::path::PathBuf, std::path::PathBuf) {
        let log = dir.join("unloads.log");
        let bin = dir.join("fake-lms");
        let user = r#"{"identifier":"user-model","modelKey":"user-model","contextLength":4096}"#;
        let ours = r#"{"identifier":"darkmux:a","modelKey":"a","contextLength":8192}"#;
        std::fs::write(
            &bin,
            format!(
                "#!/bin/sh\nLOG='{log}'\ncase \"$1\" in\n  ps)\n    if grep -qx 'darkmux:a' \"$LOG\" 2>/dev/null; then echo '[{user}]'; else echo '[{ours},{user}]'; fi ;;\n  unload) echo \"$2\" >> \"$LOG\" ;;\nesac\nexit 0\n",
                log = log.display()
            ),
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        (bin, log)
    }

    /// (#1274) `eject_all_managed` end to end through a fake `lms`: it unloads
    /// the `darkmux:` resident and ONLY that one, reports it as ejected, and
    /// counts the user-loaded model without touching it. A dry run reports the
    /// same split and unloads nothing.
    #[test]
    #[serial_test::serial]
    fn eject_all_managed_unloads_only_the_darkmux_namespace() {
        let prev = std::env::var("DARKMUX_LMS_BIN").ok();
        let tmp = tempfile::tempdir().unwrap();
        let (bin, log) = fake_lms_with_two_residents(tmp.path());
        unsafe { std::env::set_var("DARKMUX_LMS_BIN", &bin) };

        let dry = eject_all_managed(true);
        let dry_unloads = std::fs::read_to_string(&log).unwrap_or_default();
        let real = eject_all_managed(false);
        let unloads = std::fs::read_to_string(&log).unwrap_or_default();

        unsafe {
            match prev {
                Some(v) => std::env::set_var("DARKMUX_LMS_BIN", v),
                None => std::env::remove_var("DARKMUX_LMS_BIN"),
            }
        }
        let expected = EjectSummary {
            ejected: vec![EjectedModel { identifier: "darkmux:a".into(), context: 8192 }],
            user_loaded_count: 1,
            failed: Vec::new(),
        };
        assert_eq!(dry.unwrap(), expected, "a dry run reports what would come out");
        assert_eq!(dry_unloads, "", "a dry run unloads nothing");
        assert_eq!(real.unwrap(), expected);
        assert_eq!(unloads, "darkmux:a\n", "only the darkmux: resident is unloaded; user state is never touched");
    }

    /// (#3450) A process standing in for a server darkmux started: it leads
    /// its own group, as `server::ensure` spawns one. Killed when dropped.
    struct Stand(std::process::Child);

    impl Drop for Stand {
        fn drop(&mut self) {
            let _ = darkmux_types::child_registry::kill_group(self.0.id(), darkmux_types::child_registry::SIGKILL);
            let _ = self.0.wait();
        }
    }

    fn stand_in() -> Stand {
        let mut cmd = std::process::Command::new("sleep");
        cmd.arg("30");
        std::os::unix::process::CommandExt::process_group(&mut cmd, 0);
        Stand(cmd.spawn().unwrap())
    }

    /// darkmux's record of `pid` on `endpoint`, with the process's real start
    /// stamp, or `stamp` in its place.
    fn record(dirs: &crate::engine::server::Dirs, endpoint: &str, pid: u32, stamp: Option<u64>) -> darkmux_types::engine_record::EngineRecord {
        use darkmux_types::engine_record::{self, EngineRecord, ENGINE_RECORD_SCHEMA};
        let real = (0..50).find_map(|_| {
            std::thread::sleep(std::time::Duration::from_millis(10));
            darkmux_platform::current().process_start_stamp(pid)
        });
        let rec = EngineRecord {
            schema: ENGINE_RECORD_SCHEMA,
            endpoint: endpoint.into(),
            engine: "tensorfold".into(),
            model: format!("m/{endpoint}"),
            n_ctx: 8192,
            port: 1,
            pid,
            start_stamp: stamp.or(real).expect("a start stamp"),
            owner_pid: std::process::id(),
            argv: vec!["tf".into(), "--name".into(), format!("darkmux:m/{endpoint}")],
            log: dirs.log_for(endpoint),
            started_at_ms: 0,
            engine_version: "fake 1.2.4".into(),
        };
        engine_record::write_in(&dirs.records, &rec).unwrap();
        rec
    }

    fn quick() -> crate::engine::server::Bounds {
        crate::engine::server::Bounds {
            start: std::time::Duration::from_secs(5),
            stop_grace: std::time::Duration::from_secs(2),
            poll: std::time::Duration::from_millis(50),
        }
    }

    fn dirs_in(tmp: &tempfile::TempDir) -> crate::engine::server::Dirs {
        crate::engine::server::Dirs { records: tmp.path().join("run"), logs: tmp.path().join("logs") }
    }

    /// (#3450) `machine eject` stops the servers darkmux started, keeps one a
    /// running dispatch holds, and never signals a process its record no
    /// longer matches (a reused pid is someone else's).
    #[test]
    fn eject_stops_darkmuxs_idle_servers_and_nothing_else() {
        let tmp = tempfile::tempdir().unwrap();
        let dirs = dirs_in(&tmp);
        let idle = stand_in();
        let busy = stand_in();
        let stranger = stand_in();
        record(&dirs, "idle", idle.0.id(), None);
        record(&dirs, "busy", busy.0.id(), None);
        record(&dirs, "reused", stranger.0.id(), Some(7));
        let held = |r: &darkmux_types::engine_record::EngineRecord| r.endpoint == "busy";

        let dry = eject_servers_in(&dirs, true, quick(), &held);
        assert_eq!(dry.stopped.iter().map(|s| s.endpoint.as_str()).collect::<Vec<_>>(), vec!["idle"]);
        assert_eq!(dry.held.iter().map(|s| s.endpoint.as_str()).collect::<Vec<_>>(), vec!["busy"]);
        let alive = |s: &Stand| darkmux_platform::current().process_start_stamp(s.0.id()).is_some() && !std::process::Command::new("ps").args(["-o", "stat=", "-p", &s.0.id().to_string()]).output().unwrap().stdout.starts_with(b"Z");
        assert!(alive(&idle), "a dry run stops nothing");

        let real = eject_servers_in(&dirs, false, quick(), &held);
        assert_eq!(real.stopped.iter().map(|s| (s.endpoint.as_str(), s.model.as_str())).collect::<Vec<_>>(), vec![("idle", "m/idle")]);
        assert_eq!(real.held.len(), 1);
        assert!(real.failed.is_empty(), "{:?}", real.failed);
        assert!(!alive(&idle), "darkmux's idle server is stopped");
        assert!(alive(&busy), "a server a running dispatch holds is kept");
        assert!(alive(&stranger), "a process its record no longer matches is never signaled");
        assert!(darkmux_types::engine_record::read_in(&dirs.records, "idle").unwrap().is_none(), "its record is gone");
    }

    /// (#3450) What `machine eject` asks before it stops a server is the
    /// residency leases, as a dispatch's eviction asks them: a server whose
    /// served name another dispatch leases is held.
    #[test]
    fn a_server_another_dispatch_leases_is_held() {
        let tmp = tempfile::tempdir().unwrap();
        let rec = record(&dirs_in(&tmp), "tf", std::process::id(), None);
        let probe = darkmux_types::residency_lease::LeaseGuard::acquire();
        assert!(!dispatch_holds(&probe)(&rec), "nothing leases it yet");
        let dispatch = darkmux_types::residency_lease::LeaseGuard::acquire();
        dispatch.write(&["darkmux:m/tf".to_string()]).unwrap();
        assert!(dispatch_holds(&probe)(&rec), "a running dispatch's lease holds it");
        drop(dispatch);
        assert!(!dispatch_holds(&probe)(&rec), "released with the dispatch");
    }
}
