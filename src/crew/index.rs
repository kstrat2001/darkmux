//! SQLite-backed derived index over crew manifests. **Phase B of issue #45.**
//!
//! # Status: SCAFFOLD ONLY
//!
//! This module is intentionally stub. The full implementation is the next
//! darkmux dev unit; the scaffold exists so:
//!
//! 1. The Cargo dependency (`rusqlite` with `bundled` feature) is in place
//! 2. The module wires into `crew::` cleanly
//! 3. The synthesis of Pair 3's three candidate schema designs (see below)
//!    is captured *here*, near the implementation, so the next iteration
//!    doesn't re-litigate it
//! 4. The CLI verbs (`darkmux crew index rebuild`, `darkmux crew index status`)
//!    are registered with stubs that error loud, so accidental invocation
//!    doesn't silently no-op
//!
//! # Synthesis: where the schema comes from
//!
//! Three candidate schema designs were produced during Pair 3 of the
//! dev-agent bake-off (2026-05-13). The implementation should integrate
//! the strongest elements of each rather than picking one wholesale:
//!
//! ## D's design — the baseline
//!
//! Adopt D's overall structure:
//! - **Composite PK `(id, mission_id)` on sprints** — sprint IDs are scoped
//!   per mission, not globally unique. The Rust `Sprint` struct's
//!   `mission_id: String` field aligns with this; the issue spec's "nested
//!   under missions or sibling — implementer choice" is resolved by
//!   composite.
//! - **`PRAGMA defer_foreign_keys = ON` during rebuild** — bulk inserts
//!   in arbitrary order would otherwise hit FK constraint violations
//!   mid-rebuild. Deferred enforcement runs at transaction commit.
//! - **`PRAGMA foreign_keys = ON`** as a connection-default — FKs are off
//!   by default in SQLite; explicit opt-in is required.
//! - **`source_files.kind` covers ALL entity types** (D flagged this as a
//!   spec concern — the original sketch said 'role','capability' but
//!   crews/missions/sprints are manifests too). Use:
//!   `kind IN ('role','capability','crew','mission','sprint')`.
//! - **`PRAGMA user_version` for schema versioning** — SQLite-native;
//!   no external migration tooling needed.
//!
//! ## B's design — additive insights
//!
//! Adopt these from B's design:
//! - **`role_escalation_targets` table** — the `EscalationContract::HandOffTo(String)`
//!   enum variant carries a `role_id` payload. Storing the enum variant
//!   as a plain string in `roles.escalation_contract` loses the FK
//!   relationship. Factor out:
//!   ```sql
//!   CREATE TABLE role_escalation_targets (
//!     role_id TEXT PRIMARY KEY REFERENCES roles(id) ON DELETE CASCADE,
//!     target_role_id TEXT NOT NULL REFERENCES roles(id)
//!   );
//!   ```
//!   The `roles.escalation_contract` column then stores only the variant
//!   tag (`'bail-with-explanation'` / `'retry-with-hint'` / `'hand-off-to'`),
//!   and `hand-off-to` requires a row in `role_escalation_targets`.
//! - **`unmatched_terms` table** — for allocator FTS fallback (terms in
//!   ticket text that didn't match any capability keyword). Per the #45
//!   schema sketch. A and D both missed this; it ships:
//!   ```sql
//!   CREATE TABLE unmatched_terms (
//!     term TEXT PRIMARY KEY,
//!     count INTEGER NOT NULL DEFAULT 0,
//!     last_seen INTEGER NOT NULL
//!   );
//!   ```
//!
//! ## A's design — operational detail
//!
//! Adopt from A's design:
//! - **FTS5 sync trigger** — automatic propagation of `capability_keywords`
//!   INSERTs to `capability_keywords_fts`. Without it the FTS index goes
//!   stale on each rebuild and has to be manually rebuilt. Pattern:
//!   ```sql
//!   CREATE TRIGGER capability_keywords_ai
//!     AFTER INSERT ON capability_keywords
//!     BEGIN
//!       INSERT INTO capability_keywords_fts(keyword, capability_id, weight)
//!       VALUES (NEW.keyword, NEW.capability_id, NEW.weight);
//!     END;
//!   ```
//!   Plus matching `_ad` (after delete) and `_au` (after update) triggers
//!   for full keep-in-sync coverage. Or use a `content='capability_keywords'`
//!   contentless FTS5 table and let SQLite handle sync — D suggested this
//!   path; either works, but the explicit triggers are easier to reason
//!   about during the first impl.
//!
//! ## Spec concerns to resolve during impl
//!
//! D's design surfaced four concerns; the impl should decide each:
//!
//! 1. **Sprint ID scoping** — resolved above: composite PK `(id, mission_id)`
//!    per D's call. A and B picked global uniqueness; D's read of the
//!    spec is closer to the Rust struct shape.
//! 2. **`Mission.sprint_ids: Vec<String>` redundant with `sprints` table FK**
//!    — pick one. Recommendation: drop the JSON-array column on missions
//!    and derive sprint membership from `sprints WHERE mission_id = ?`.
//!    Saves a denormalization that has to be kept in sync.
//! 3. **`outcomes.sprint_id` not a FK** — because sprint IDs are scoped
//!    per-mission and an outcome belongs to an allocation that already
//!    carries `mission_id`, the constraint is `(outcomes.sprint_id,
//!    outcomes_allocation.mission_id)` must match a row in `sprints`.
//!    SQLite can't enforce this directly; either: (a) denormalize
//!    `outcomes.mission_id` and add a composite FK to `sprints(id, mission_id)`,
//!    or (b) skip the FK and rely on application-level validation.
//!    Recommendation: (a) — denormalize for FK integrity.
//! 4. **`source_files.kind` enum** — see baseline above; expand to all 5
//!    entity types.
//!
//! # Public surface
//!
//! These are the verbs the CLI will offer once implemented:
//!
//! - `darkmux crew index rebuild` — DELETE+INSERT from all manifests on disk
//! - `darkmux crew index status` — drift detection (mtime + content_hash
//!   comparison against `source_files`), last-rebuild timestamp, per-kind
//!   source count
//!
//! Plus CRUD CLI for each entity per the issue spec (`darkmux role`,
//! `darkmux crew`, `darkmux mission`, `darkmux sprint`). Read-only by
//! default; mutations land via hand-editing the JSON manifests + rebuild.
//!
//! # Acceptance criteria (from #45)
//!
//! - All four type CRUD CLIs work and pass tests
//! - Index rebuild from a clean state produces a queryable DB
//! - FTS5 keyword search produces ranked capabilities for a known ticket-text fixture
//! - Audit-log writes are append-only; outcomes link to allocations correctly
//! - Index rebuild is idempotent (running twice produces the same state)
//! - `darkmux index status` flags drift (file mtime newer than index timestamp)
//!
//! # Pair 3 candidate-design files (audit trail)
//!
//! The three full designs that fed this synthesis are preserved at:
//! - `~/.openclaw/workspace-coder/pair3/output/D-turboquant/schema-design.md` (baseline)
//! - `~/.openclaw/workspace-coder/pair3/output/B-codernext/schema-design.md` (HandOffTo + unmatched_terms)
//! - `~/.openclaw/workspace-coder/pair3/output/A-a10b/schema-design.md` (FTS5 triggers + density)
//!
//! And the scored rubric is in `~/de-projects/de-lab/LAB_NOTEBOOK.md` under
//! the 2026-05-13 "Pair 3 result + bake-off hire decision" entry.

#![allow(dead_code)]

use anyhow::{Result, bail};

/// Rebuild the index from manifests on disk.
///
/// **Not yet implemented.** See module-level doc for the synthesized design
/// that this should implement.
pub fn rebuild() -> Result<()> {
    bail!(
        "darkmux crew index rebuild is not yet implemented — see issue #45 Phase B. \
         Synthesis decisions are documented in src/crew/index.rs's module doc; \
         the next implementation pass fills in the SQL."
    )
}

/// Report index status: last rebuild time, source counts, drift.
///
/// **Not yet implemented.** See module-level doc.
pub fn status() -> Result<()> {
    bail!(
        "darkmux crew index status is not yet implemented — see issue #45 Phase B."
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rebuild_returns_not_implemented() {
        let err = rebuild().unwrap_err();
        assert!(err.to_string().contains("not yet implemented"));
        assert!(err.to_string().contains("#45"));
    }

    #[test]
    fn status_returns_not_implemented() {
        let err = status().unwrap_err();
        assert!(err.to_string().contains("not yet implemented"));
    }
}
