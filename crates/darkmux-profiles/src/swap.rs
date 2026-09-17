//! darkmux's LMStudio namespace helpers — the ownership contract for loaded
//! models (`darkmux:<model-id>` identifiers).
//!
//! (#1426 phase 3) This module used to hold the `darkmux swap` stack-swap
//! orchestration. That verb retired (gestalt is the one residency writer —
//! a dispatch loads what its staffing needs), and the whole swap executor
//! (`swap()`, `SwapOpts`/`SwapResult`, the desired-loads resolver, and the
//! `RegistryHooks` pre/post-swap runner — the hooks retired WITH the verb,
//! since swap was their only trigger) was deleted with it. What remains is
//! the namespace vocabulary every production consumer still uses:
//! [`DARKMUX_LMS_NAMESPACE`], [`namespaced_identifier`], and
//! [`is_darkmux_owned`].

use darkmux_types::ProfileModel;

/// Prefix attached to identifiers darkmux uses for its own LMStudio loads.
/// Anything visible via `lms ps` starting with this prefix is owned by darkmux
/// and safe for darkmux to unload; anything else is user state and off-limits.
///
/// (#1230 Packet 1 cutover) Re-exported from `darkmux_gestalt::ownership`,
/// which is now the canonical home for this constant — see that module's
/// doc comment ("Packet 3 re-points swap.rs at this module"). Kept as a
/// `pub const` alias here (not a bare re-export of a differently-named
/// item) so every existing `swap::DARKMUX_LMS_NAMESPACE` call site keeps
/// compiling unchanged.
///
/// See [issue #52](https://github.com/kstrat2001/darkmux/issues/52) for the
/// design rationale (operator-sovereignty applied at model-state level —
/// darkmux never touches state it didn't bring up).
pub const DARKMUX_LMS_NAMESPACE: &str = darkmux_gestalt::DARKMUX_NAMESPACE;

/// Compute the darkmux-namespaced LMStudio identifier for a profile model.
///
/// (#1230 Packet 1 cutover) Thin delegating wrapper over
/// `darkmux_gestalt::namespaced_identifier` — the `&ProfileModel` form this
/// crate's callers use, feeding gestalt's two-explicit-parameter form (a
/// bare `pub use` can't bridge the signature). ONE definition backs both
/// this wrapper and the review's `LmsCycler`.
///
/// If the profile sets an explicit `identifier`, it passes through as-is
/// (the documented namespace opt-out). Otherwise the model id is wrapped
/// under the `darkmux:` namespace so unload-filtering can distinguish
/// darkmux's loads from user-managed ones.
pub fn namespaced_identifier(m: &ProfileModel) -> String {
    darkmux_gestalt::namespaced_identifier(&m.id, m.identifier.as_deref())
}

/// `true` if this identifier was minted by darkmux (begins with our
/// namespace). Used to filter `lms ps` results into darkmux-managed vs
/// user state (`machine status`/`machine eject`, dispatch preflight).
///
/// (#1230 Packet 1 cutover) Delegates to `darkmux_gestalt::is_darkmux_owned`
/// — see `namespaced_identifier`'s doc above.
pub fn is_darkmux_owned(identifier: &str) -> bool {
    darkmux_gestalt::is_darkmux_owned(identifier)
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
    let loaded = crate::lms::list_loaded()?;
    let (managed, user_loaded_count) = partition_by_ownership(&loaded);
    let (ejected, failed) = eject_each(&managed, dry_run, &|id| crate::lms::unload(id));
    Ok(EjectSummary { ejected, user_loaded_count, failed })
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
            if let Err(e) = unload(&m.identifier) {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn namespaced_identifier_uses_prefix_when_no_override() {
        let m = ProfileModel {
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
        let m = ProfileModel {
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
}
