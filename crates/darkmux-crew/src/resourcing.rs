//! (#1475, dissolved-probe-role refactor #1512, #1513 review finding) The
//! review resourcing resolver — the single planning step that staffs a
//! review's roles via the role→profile flip: each review role resolves
//! INDEPENDENTLY through the machine-local `role_profiles` map (with a
//! per-run launch override on top, and `default_profile` as the fresh-user
//! floor). It hands the review driver a [`ResolvedReviewRoles`] whose
//! `staffing` snapshot records what resolved and WHY (role → profile →
//! model → binding-source), so the run's envelope shows truth (operator
//! sovereignty #44).
//!
//! **There is no "probe role" concept, and no "crew" concept (#1512, #1513
//! review).** A probe is a TASK that carries one `role_id` and a probe-kind
//! step — "probe" is emergent from that composition, never a family this
//! module enumerates. [`resolve_review_roles`] is the ONE generic
//! resolution pass: it walks the "review" mission config's own declared
//! tasks, classifies each role-bearing task STRUCTURALLY by which Tier-3
//! step kind it carries (the now-deleted dedicated review launcher
//! classified its own judge/verify tasks that way; anything else with a
//! `role_id` ⇒ a probe task), and resolves every one of them through the
//! SAME per-task primitive, [`resolve_task_role`]. There is no Rust-side
//! enumeration of "the probe roles" (no array, no magic-string heuristic
//! reading `review-dedup-task.depends_on`), no `seats: BTreeMap<String,
//! Vec<_>>` family grouping, and no separate "crew" type — the probe
//! COUNT and every role's identity fall out of whatever `review.json`
//! declares, read directly off the document each time.
//!
//! A `ResolvedReviewRoles` is a DERIVED VIEW of a mission's resourcing,
//! never a declared entity: nobody keeps a registry of pre-formed crews
//! awaiting missions. There is a corps (the profile registry), there is
//! planning, and staffing is an OUTPUT. The roster-scoring resolver
//! (`select_model` per seat against one roster profile) it replaced was
//! deleted in #1475 packet 3 — recall diversity now falls out of distinct
//! probe role→profile bindings, not `k` draws of one scored model.

use darkmux_types::{BundleSelector, ProfileModel};


/// (#1426 ship-2 / #44) How a seat's model was chosen. The resolver stamps
/// this on every staffing so the envelope's staffing snapshot answers "where
/// did this decision come from" directly — the operator never has to wonder
/// whether a seat was scored or pinned (operator sovereignty #44: system
/// proposes, operator overrides, record shows truth AND why).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct StaffingProvenance {
    /// `"role-profile"` today (a seat staffed by the role→profile flip). A plain
    /// string, not an enum, so snapshot consumers stay lenient to future kinds.
    pub kind: String,
    /// Names the whole role→profile→model→binding-source chain.
    pub detail: String,
}

impl StaffingProvenance {
    /// (#1475) A seat staffed by the role→profile flip: the role was resolved by
    /// a per-run launch override (`source = "launch override"`), through the
    /// machine-local `role_profiles` map (`source = "role_profiles map"`), or
    /// fell through to `default_profile` (`source = "default_profile fallback"`,
    /// the fresh-user floor). Names the whole role→profile→model chain so the
    /// envelope answers "where did this seat's model come from" directly
    /// (operator sovereignty #44).
    pub fn role_profile(role_id: &str, profile_name: &str, source: &str) -> Self {
        StaffingProvenance {
            kind: "role-profile".to_string(),
            detail: format!(
                "role \"{role_id}\" → profile \"{profile_name}\" ({source})"
            ),
        }
    }
}

/// A seat staffing resolved to a concrete model — the resolver's per-seat
/// output. The review driver + envelope snapshot consume it unchanged.
///
/// (#1530 Packet 3a) `Default` derives cleanly (every field is itself
/// `Default` — `ProfileModel`, `BundleSelector`, and every `Option`/`Vec`/
/// primitive here already are) and is used ONLY by
/// `darkmux-lab::lab::review`'s context-free `ArtifactBus` factory default
/// for `ReviewStepContext::roles` — a value ALWAYS overwritten by
/// `run_review_graph`'s caller-seed before any step reads it (see
/// `Port::artifact`'s doc on why a factory can only build a context-free
/// default). Never constructed as a real staffing anywhere else.
///
/// (#2310 P1) `Serialize`/`Deserialize`/`PartialEq` added so this could
/// ride as DATA on `darkmux-lab`'s now-deleted `ReviewContext` step-output
/// body — the now-deleted dedicated review launcher's context step config
/// carried the resolved staffing as JSON and needed a round-trip (both
/// `ReviewContext` and that launcher are gone, #2310 P4d; this type's own
/// mutation test (drop a field, the typed read fails by name) still needs
/// `PartialEq` for the golden comparison).
/// Every field is itself `Serialize`/`Deserialize`/`PartialEq` already
/// (`ProfileModel` gained `PartialEq` in the same packet).
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct ResolvedSeatStaffing {
    /// The [`Profile`](darkmux_types::Profile) name this seat's role resolved to
    /// (via the role→profile flip) and dispatches through.
    pub name: String,
    /// (#1475) The review ROLE this seat was staffed for — whatever
    /// `role_id` the owning task declares. `None` only for hand-built test
    /// staffings. The envelope snapshot records it so a run names which
    /// role bound each seat.
    pub role_id: Option<String>,
    pub pm: ProfileModel,
    /// Historically the probe-seat draw BREADTH (a union over multiple
    /// dispatches of the same role). (#1512) `build_review_graph` no longer
    /// multiplies a probe role's task by `k` — one role is one task is one
    /// dispatch; recall breadth is now a review.json edit (declare another
    /// probe role), never a per-run draw multiplier. The field survives for
    /// back-compat (envelope staffing snapshots, `review-bench --k`
    /// reporting) and is always `1` for every seat this module resolves.
    /// Ignored by the judge/verify seats regardless.
    pub k: u32,
    /// Judge-seat consensus DEPTH (agreement across independent judgments —
    /// precision). Ignored by the probe/verify seats.
    pub passes: u32,
    pub max_tokens: Option<u32>,
    pub selector: Option<BundleSelector>,
    /// (#1475 / #44) The role→profile→model→binding-source chain, stamped by the
    /// resolver; `None` only for hand-built staffings (tests, synthetic paths).
    pub provenance: Option<StaffingProvenance>,
}

// ─── (#1475) role→profile staffing — THE FLIP ───────────────────────────────
