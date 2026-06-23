//! (#994 engagement-context) The operator-AUTHORED lessons store —
//! conventions, constraints, and decisions for an engagement, plus the
//! reasoning behind them. "Include the why" is the documentation point: a good
//! lesson explains *why* the code is shaped the way it is, not just states the
//! rule, so a fresh-context local model can apply it with judgment.
//!
//! **Durable, concurrent-safe SQLite** (`lessons.db`), not a JSON file. The
//! store is detection-driven: the loop pathologies a churning run throws off
//! (raw detections in the append-only flow stream) are distilled into durable
//! lessons — so writes can land *while runs run*. A rewritten JSON file would
//! race (lost updates) and a single interrupted write would corrupt the whole
//! store; SQLite transactions are atomic (a crash rolls back), its locking
//! serializes writers, WAL lets reads proceed during a write, and
//! `PRAGMA user_version` gives a real migration path (JSON has none). Edited via
//! verbs (`darkmux lessons add`/`list`), not raw-file editing; an
//! `export`/`import` can restore the hand-edit/git roundtrip later.
//!
//! TWO TIERS (the gitconfig model), so lessons stay engagement-scoped and
//! don't bleed across the operator's many engagements:
//! - **per-repo** ([`repo_db_path`]) — `<repo>/.darkmux/lessons.db`, the
//!   engagement's own conventions. A coder dispatch in repo X sees only X's.
//! - **user-global** ([`global_db_path`]) — `~/.darkmux/lessons.db`,
//!   conventions that apply to ALL the operator's work (house style, language).
//!
//! The coder-brief inject reads BOTH; repo Y's never reaches a dispatch in X.

use anyhow::{Context, Result};
use rusqlite::{params, Connection, OptionalExtension, Row};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use darkmux_types::paths::{resolve, ResolveScope};

/// Plain semver (integer, via `PRAGMA user_version`) on the lessons-db
/// schema, independent of darkmux's version. Bump + add a migration block in
/// [`init_schema`] when the table shape changes.
pub const LESSONS_SCHEMA_VERSION: i32 = 1;

const LESSONS_SCHEMA_SQL: &str = r#"
CREATE TABLE IF NOT EXISTS lessons (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    title       TEXT NOT NULL,
    body        TEXT NOT NULL,
    file        TEXT,
    source      TEXT,
    created_ts  INTEGER NOT NULL,
    updated_ts  INTEGER NOT NULL
);
"#;

/// One authored lesson: a convention / constraint / decision and the reasoning
/// behind it (the "why" lives in `body`, it is not a separate field).
/// `Deserialize` is derived (not just `Serialize`) so [`import_json`] can read
/// an exported store back. The optional fields take `#[serde(default)]` so a
/// hand-authored entry can carry just `title` + `body`; the timestamps default
/// to `0`, which [`import_json`] reads as "stamp now".
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Lesson {
    /// DB rowid — `None` for a lesson being constructed for insert, `Some` when
    /// read back. [`edit`]/[`remove`] target it; [`import_json`] upserts on it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<i64>,
    /// Short statement of the rule / decision.
    pub title: String,
    /// The detail — explains the WHY, not just the rule.
    pub body: String,
    /// Optional area scope. `None` = engagement-level (applies everywhere);
    /// `Some(path)` = scoped to a file. File-precision retrieval is a later
    /// increment — today every lesson is injected (engagement-coarse).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file: Option<String>,
    /// Provenance — who authored it (`"operator"`, `"orchestrator"`). The
    /// authority signal the retrieve phase ranks on; defaults to `"operator"`
    /// when added via the CLI.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    #[serde(default)]
    pub created_ts: i64,
    #[serde(default)]
    pub updated_ts: i64,
}

/// Self-describing envelope for [`export_json`]/[`import_json`] — carries the
/// schema version alongside the rows so a hand-edited / git-committed dump is
/// unambiguous and a future migration can branch on it.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct LessonsExport {
    pub schema_version: i32,
    pub lessons: Vec<Lesson>,
}

/// What an [`import_json`] did — surfaced to the operator so the roundtrip is
/// auditable (insert vs in-place update).
#[derive(Debug, Default, Clone, Copy, Serialize, PartialEq)]
pub struct ImportStats {
    pub inserted: usize,
    pub updated: usize,
}

/// Per-repo (engagement-scoped) lessons db: `<repo>/.darkmux/lessons.db`.
/// The engagement boundary for a coder dispatch is the repo it edits, so each
/// engagement's lessons live in its own tree — a dispatch in repo X can
/// never see repo Y's. Resolved via the PROJECT scope (cwd-relative), NOT
/// `Auto` — `Auto` falls back to the user-global root when no project
/// `.darkmux/` exists, which is the cross-engagement bleed this design avoids.
pub fn repo_db_path() -> PathBuf {
    resolve(ResolveScope::ForceProject).root.join("lessons.db")
}

/// User-global lessons db: `~/.darkmux/lessons.db`. Conventions that apply
/// to ALL the operator's work regardless of engagement (house style, language,
/// universal constraints). Injected into every coder brief ALONGSIDE the repo's
/// own lessons — universal by opt-in (`lessons add --global`), never by
/// accident. (When `$DARKMUX_HOME` relocates the root, both tiers resolve to it
/// — a deliberate single-root install collapses the two tiers into one.)
pub fn global_db_path() -> PathBuf {
    resolve(ResolveScope::ForceUser).root.join("lessons.db")
}

fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Open (creating if absent) a lessons db at `path`, ensuring the schema +
/// version. WAL mode so a reader (an in-flight dispatch's inject) doesn't block
/// a writer (`lessons add`) and vice versa — the concurrency the store exists
/// to survive. Creates the parent dir as needed.
pub fn open_at(path: &Path) -> Result<Connection> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating lessons dir {}", parent.display()))?;
        }
    }
    let conn =
        Connection::open(path).with_context(|| format!("opening lessons db {}", path.display()))?;
    // (#994 QA) Wait for a concurrent writer rather than instantly returning
    // SQLITE_BUSY. `open_at` is shared by the write verb AND the best-effort
    // inject read, and the schema-init DDL/pragma below take a lock. Without
    // this, a dispatch reading lessons while a `lessons add` (or the future
    // distiller) is mid-write would degrade to "no lessons" for that run —
    // silently losing exactly the concurrency the SQLite store was chosen to
    // survive. With it, the read waits out the (sub-second) write and gets the
    // full set. Set BEFORE the WAL pragma so even that first lock waits.
    conn.busy_timeout(std::time::Duration::from_millis(2000))
        .context("setting lessons db busy_timeout")?;
    // WAL: concurrent reads during a write; persists on the file.
    conn.pragma_update(None, "journal_mode", "WAL")
        .context("setting WAL journal mode")?;
    init_schema(&conn)?;
    Ok(conn)
}

fn init_schema(conn: &Connection) -> Result<()> {
    let current: i32 = conn
        .query_row("PRAGMA user_version", [], |r| r.get(0))
        .unwrap_or(0);
    conn.execute_batch(LESSONS_SCHEMA_SQL)
        .context("applying lessons schema")?;
    // Future migrations: `if current < N { conn.execute_batch("ALTER TABLE …") }`
    // before the version stamp. `lessons` is a SOURCE table (never dropped),
    // so migrations are additive ALTERs, not the index's drop+recreate.
    if current != LESSONS_SCHEMA_VERSION {
        conn.execute_batch(&format!("PRAGMA user_version = {LESSONS_SCHEMA_VERSION};"))
            .context("stamping lessons schema version")?;
    }
    Ok(())
}

/// Append a new authored lesson (one atomic INSERT — a crash leaves the store
/// intact, never half-written). Stamps timestamps now and defaults `source` to
/// `"operator"` when unset. Returns the new rowid.
pub fn add(
    conn: &Connection,
    title: &str,
    body: &str,
    file: Option<&str>,
    source: Option<&str>,
) -> Result<i64> {
    let now = now_unix();
    conn.execute(
        "INSERT INTO lessons (title, body, file, source, created_ts, updated_ts)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        params![title, body, file, source.unwrap_or("operator"), now, now],
    )
    .context("inserting lesson")?;
    Ok(conn.last_insert_rowid())
}

/// Shared row → [`Lesson`] mapping for the read paths ([`list`]/[`get`]/
/// [`recall`]) — the column order is fixed by [`SELECT_COLS`].
const SELECT_COLS: &str = "id, title, body, file, source, created_ts, updated_ts";

fn row_to_lesson(r: &Row) -> rusqlite::Result<Lesson> {
    Ok(Lesson {
        id: Some(r.get(0)?),
        title: r.get(1)?,
        body: r.get(2)?,
        file: r.get(3)?,
        source: r.get(4)?,
        created_ts: r.get(5)?,
        updated_ts: r.get(6)?,
    })
}

/// All lessons, most-recently-updated first.
pub fn list(conn: &Connection) -> Result<Vec<Lesson>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {SELECT_COLS} FROM lessons ORDER BY updated_ts DESC, id DESC"
    ))?;
    let rows = stmt.query_map([], row_to_lesson)?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

/// One lesson by rowid, or `None` if no row matches.
pub fn get(conn: &Connection, id: i64) -> Result<Option<Lesson>> {
    let mut stmt = conn.prepare(&format!("SELECT {SELECT_COLS} FROM lessons WHERE id = ?1"))?;
    let mut rows = stmt.query_map(params![id], row_to_lesson)?;
    match rows.next() {
        Some(r) => Ok(Some(r?)),
        None => Ok(None),
    }
}

/// Edit a lesson in place (read-modify-write so `created_ts` is preserved and
/// only the supplied fields change). `file` is **tri-state**: `None` leaves the
/// scope unchanged, `Some(None)` clears it to engagement-level, `Some(Some(p))`
/// re-scopes to a file. Bumps `updated_ts`. Returns `false` if no row matches.
pub fn edit(
    conn: &Connection,
    id: i64,
    title: Option<&str>,
    body: Option<&str>,
    file: Option<Option<&str>>,
    source: Option<&str>,
) -> Result<bool> {
    let Some(mut l) = get(conn, id)? else {
        return Ok(false);
    };
    if let Some(t) = title {
        l.title = t.to_owned();
    }
    if let Some(b) = body {
        l.body = b.to_owned();
    }
    if let Some(f) = file {
        l.file = f.map(str::to_owned);
    }
    if let Some(s) = source {
        l.source = Some(s.to_owned());
    }
    conn.execute(
        "UPDATE lessons SET title=?1, body=?2, file=?3, source=?4, updated_ts=?5 WHERE id=?6",
        params![l.title, l.body, l.file, l.source, now_unix(), id],
    )
    .context("updating lesson")?;
    Ok(true)
}

/// Delete a lesson by rowid. Returns `false` if no row matched.
pub fn remove(conn: &Connection, id: i64) -> Result<bool> {
    let n = conn
        .execute("DELETE FROM lessons WHERE id = ?1", params![id])
        .context("deleting lesson")?;
    Ok(n > 0)
}

/// Read-only recall for operator inspection: filter the store by a
/// case-insensitive substring `term` (matched against title OR body) and/or an
/// exact `file` scope. A `None` filter matches everything; both filters AND
/// together. Ordering matches [`list`] (most-recently-updated first). Filtering
/// happens in Rust — the store is operator-small, so this avoids SQL `LIKE`
/// escaping and keeps the match rule one place for the file-precision work.
pub fn recall(conn: &Connection, term: Option<&str>, file: Option<&str>) -> Result<Vec<Lesson>> {
    let term_lc = term.map(str::to_lowercase);
    Ok(list(conn)?
        .into_iter()
        .filter(|l| {
            let term_ok = term_lc.as_ref().map_or(true, |t| {
                l.title.to_lowercase().contains(t) || l.body.to_lowercase().contains(t)
            });
            let file_ok = file.map_or(true, |f| l.file.as_deref() == Some(f));
            term_ok && file_ok
        })
        .collect())
}

/// Serialize the whole store to a stable, self-describing JSON envelope for a
/// hand-edit / git-commit / restore roundtrip (the inverse of [`import_json`]).
pub fn export_json(conn: &Connection) -> Result<String> {
    let env = LessonsExport {
        schema_version: LESSONS_SCHEMA_VERSION,
        lessons: list(conn)?,
    };
    serde_json::to_string_pretty(&env).context("serializing lessons export")
}

/// Restore an exported (or hand-authored) envelope into the store, in ONE
/// transaction (all-or-nothing — a malformed entry rolls the whole import back,
/// never half-applying). **Upsert by id**: an entry whose `id` already exists is
/// updated in place (so export → edit → import is idempotent); an entry with no
/// (or an unknown) `id` is inserted with a fresh rowid (so appending new entries
/// to the file Just Works). Import never deletes — use [`remove`] for that.
/// Timestamps from the file are preserved when positive; `0`/absent stamps now,
/// and a missing `source` defaults to `"operator"` (matching [`add`]).
pub fn import_json(conn: &mut Connection, data: &str) -> Result<ImportStats> {
    let env: LessonsExport = serde_json::from_str(data).context("parsing lessons export")?;
    let now = now_unix();
    let mut stats = ImportStats::default();
    let tx = conn.transaction().context("opening import transaction")?;
    // Process explicit-id entries (upsert) BEFORE no-id entries (autoincrement
    // insert). AUTOINCREMENT picks above the current max id, so doing the
    // id-bearing rows first guarantees a no-id insert can't land on an id a
    // later entry preserves — closing the only id-collision a hand-authored
    // file (mixing authored + explicit-id entries) could otherwise trip.
    let (with_id, without_id): (Vec<_>, Vec<_>) =
        env.lessons.into_iter().partition(|l| l.id.is_some());
    for l in with_id.into_iter().chain(without_id) {
        let created = if l.created_ts > 0 { l.created_ts } else { now };
        let updated = if l.updated_ts > 0 { l.updated_ts } else { now };
        // Probe by the file's id (when present) so the dump's ids stay
        // authoritative — INSERT preserves the id rather than letting
        // autoincrement reassign it. That keeps export → import a stable,
        // order-independent roundtrip: re-importing updates the same rows
        // instead of duplicating, and an entry can't collide with another
        // entry's freshly-assigned id mid-import.
        let existing_id = match l.id {
            Some(id) => tx
                .query_row("SELECT 1 FROM lessons WHERE id = ?1", params![id], |_| Ok(()))
                .optional()
                .context("probing lesson id")?
                .map(|_| id),
            None => None,
        };
        let source = l.source.unwrap_or_else(|| "operator".to_string());
        match existing_id {
            Some(id) => {
                tx.execute(
                    "UPDATE lessons SET title=?1, body=?2, file=?3, source=?4, \
                     created_ts=?5, updated_ts=?6 WHERE id=?7",
                    params![l.title, l.body, l.file, source, created, updated, id],
                )
                .context("import-updating lesson")?;
                stats.updated += 1;
            }
            None => {
                // Preserve the file's id when given (a fresh-store restore);
                // a no-id (hand-authored) entry gets a fresh autoincrement id.
                // Because all id-bearing entries are processed first (see the
                // partition above), AUTOINCREMENT here is above every preserved
                // id — no collision.
                match l.id {
                    Some(id) => tx.execute(
                        "INSERT INTO lessons (id, title, body, file, source, created_ts, updated_ts) \
                         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                        params![id, l.title, l.body, l.file, source, created, updated],
                    ),
                    None => tx.execute(
                        "INSERT INTO lessons (title, body, file, source, created_ts, updated_ts) \
                         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                        params![l.title, l.body, l.file, source, created, updated],
                    ),
                }
                .context("import-inserting lesson")?;
                stats.inserted += 1;
            }
        }
    }
    tx.commit().context("committing import")?;
    Ok(stats)
}

/// Read lessons for the per-dispatch inject — best-effort: a MISSING db is an
/// empty list (and is NOT created, so a read never writes), and any open/query
/// error also degrades to empty rather than erroring the dispatch (mirrors the
/// #849 corrections + #994 cautions collectors). The `lessons add` write path
/// uses [`open_at`] directly (loud on error) — only this read path is silent.
pub fn load_entries_best_effort(path: &Path) -> Vec<Lesson> {
    if !path.exists() {
        return Vec::new();
    }
    open_at(path)
        .and_then(|conn| list(&conn))
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn open_creates_schema_and_stamps_version() {
        let tmp = TempDir::new().unwrap();
        let conn = open_at(&tmp.path().join("lessons.db")).unwrap();
        let v: i32 = conn.query_row("PRAGMA user_version", [], |r| r.get(0)).unwrap();
        assert_eq!(v, LESSONS_SCHEMA_VERSION);
        // Idempotent re-open is fine.
        let conn2 = open_at(&tmp.path().join("lessons.db")).unwrap();
        assert!(list(&conn2).unwrap().is_empty());
    }

    #[test]
    fn open_sets_busy_timeout_so_reads_wait_out_a_writer() {
        // (#994 QA) Without a busy_timeout, a read racing a `lessons add`
        // returns SQLITE_BUSY immediately and the best-effort inject degrades to
        // "no lessons". Assert the connection is configured to wait instead.
        let tmp = TempDir::new().unwrap();
        let conn = open_at(&tmp.path().join("lessons.db")).unwrap();
        let ms: i64 = conn.query_row("PRAGMA busy_timeout", [], |r| r.get(0)).unwrap();
        assert!(ms >= 2000, "busy_timeout must be configured (got {ms})");
    }

    #[test]
    fn add_then_list_round_trips_and_defaults_source() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("lessons.db");
        let conn = open_at(&path).unwrap();
        add(
            &conn,
            "Bound the retry loop",
            "We cap retries because the loop entrenches its first answer (self-verification dilemma).",
            Some("runtime/src/loop_runner.rs"),
            None,
        )
        .unwrap();
        add(&conn, "American English", "House style across all engagements.", None, Some("operator")).unwrap();

        let entries = list(&conn).unwrap();
        assert_eq!(entries.len(), 2);
        // Most-recent-updated first; both stamped this second so id DESC breaks the tie.
        let retry = entries.iter().find(|e| e.title == "Bound the retry loop").unwrap();
        assert!(retry.body.contains("entrenches its first answer"));
        assert_eq!(retry.file.as_deref(), Some("runtime/src/loop_runner.rs"));
        assert_eq!(retry.source.as_deref(), Some("operator"), "CLI add defaults source");
        assert!(retry.id.is_some() && retry.created_ts > 0 && retry.updated_ts > 0);
        let style = entries.iter().find(|e| e.title == "American English").unwrap();
        assert_eq!(style.file, None, "engagement-level lesson has no file scope");
    }

    #[test]
    fn load_best_effort_on_missing_is_empty_and_does_not_create() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("absent.db");
        assert!(load_entries_best_effort(&path).is_empty());
        assert!(!path.exists(), "a read must not create the db file");
    }

    #[test]
    fn reopen_persists_entries_durably() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("lessons.db");
        {
            let conn = open_at(&path).unwrap();
            add(&conn, "t", "b", None, None).unwrap();
        } // connection dropped — the INSERT is committed (durable)
        let entries = load_entries_best_effort(&path);
        assert_eq!(entries.len(), 1, "committed lesson survives a reopen");
        assert_eq!(entries[0].title, "t");
    }

    #[test]
    fn get_returns_one_or_none() {
        let tmp = TempDir::new().unwrap();
        let conn = open_at(&tmp.path().join("lessons.db")).unwrap();
        let id = add(&conn, "t", "b", Some("src/x.rs"), None).unwrap();
        let got = get(&conn, id).unwrap().unwrap();
        assert_eq!(got.id, Some(id));
        assert_eq!(got.title, "t");
        assert!(get(&conn, id + 999).unwrap().is_none(), "absent id is None");
    }

    #[test]
    fn edit_updates_only_supplied_fields_and_handles_file_tristate() {
        let tmp = TempDir::new().unwrap();
        let conn = open_at(&tmp.path().join("lessons.db")).unwrap();
        let id = add(&conn, "orig title", "orig body", Some("src/a.rs"), Some("operator")).unwrap();
        let created = get(&conn, id).unwrap().unwrap().created_ts;

        // Only body changes; title + file + source untouched (None = leave).
        assert!(edit(&conn, id, None, Some("new body"), None, None).unwrap());
        let after = get(&conn, id).unwrap().unwrap();
        assert_eq!(after.title, "orig title", "title left untouched");
        assert_eq!(after.body, "new body");
        assert_eq!(after.file.as_deref(), Some("src/a.rs"), "file left untouched");
        assert_eq!(after.created_ts, created, "created_ts preserved across edit");

        // Some(None) clears the file scope to engagement-level.
        assert!(edit(&conn, id, None, None, Some(None), None).unwrap());
        assert_eq!(get(&conn, id).unwrap().unwrap().file, None, "Some(None) clears file");

        // Some(Some(p)) re-scopes.
        assert!(edit(&conn, id, None, None, Some(Some("src/b.rs")), None).unwrap());
        assert_eq!(get(&conn, id).unwrap().unwrap().file.as_deref(), Some("src/b.rs"));

        // Missing row → false, no panic.
        assert!(!edit(&conn, id + 999, Some("x"), None, None, None).unwrap());
    }

    #[test]
    fn remove_deletes_and_reports_match() {
        let tmp = TempDir::new().unwrap();
        let conn = open_at(&tmp.path().join("lessons.db")).unwrap();
        let id = add(&conn, "t", "b", None, None).unwrap();
        assert!(remove(&conn, id).unwrap(), "existing row removed");
        assert!(get(&conn, id).unwrap().is_none());
        assert!(!remove(&conn, id).unwrap(), "second remove finds nothing");
    }

    #[test]
    fn recall_filters_by_term_and_file() {
        let tmp = TempDir::new().unwrap();
        let conn = open_at(&tmp.path().join("lessons.db")).unwrap();
        add(&conn, "Bound the retry loop", "self-verification dilemma", Some("runtime/src/loop_runner.rs"), None).unwrap();
        add(&conn, "American English", "house STYLE everywhere", None, None).unwrap();

        // Case-insensitive substring against title OR body.
        let hits = recall(&conn, Some("style"), None).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].title, "American English");

        // File-exact filter.
        let scoped = recall(&conn, None, Some("runtime/src/loop_runner.rs")).unwrap();
        assert_eq!(scoped.len(), 1);
        assert_eq!(scoped[0].title, "Bound the retry loop");

        // No filters → everything.
        assert_eq!(recall(&conn, None, None).unwrap().len(), 2);
        // Both filters AND together → no match.
        assert!(recall(&conn, Some("style"), Some("runtime/src/loop_runner.rs")).unwrap().is_empty());
    }

    #[test]
    fn export_import_roundtrips_idempotently() {
        let tmp = TempDir::new().unwrap();
        let src = open_at(&tmp.path().join("src.db")).unwrap();
        add(&src, "Rule A", "why A", Some("src/a.rs"), Some("operator")).unwrap();
        add(&src, "Rule B", "why B", None, Some("orchestrator")).unwrap();
        let dumped = export_json(&src).unwrap();
        assert!(dumped.contains("\"schema_version\""));

        // Import into a fresh store inserts everything.
        let mut dst = open_at(&tmp.path().join("dst.db")).unwrap();
        let s1 = import_json(&mut dst, &dumped).unwrap();
        assert_eq!((s1.inserted, s1.updated), (2, 0));
        assert_eq!(list(&dst).unwrap().len(), 2);

        // Re-importing the SAME dump upserts by id — no duplicates.
        let s2 = import_json(&mut dst, &dumped).unwrap();
        assert_eq!((s2.inserted, s2.updated), (0, 2), "second import updates in place");
        assert_eq!(list(&dst).unwrap().len(), 2, "no duplicate rows on re-import");
    }

    #[test]
    fn import_hand_authored_entry_without_id_or_timestamps() {
        let tmp = TempDir::new().unwrap();
        let mut conn = open_at(&tmp.path().join("lessons.db")).unwrap();
        // Minimal hand-authored envelope: no id, no timestamps, no source.
        let json = r#"{"schema_version":1,"lessons":[{"title":"Hand","body":"authored"}]}"#;
        let stats = import_json(&mut conn, json).unwrap();
        assert_eq!((stats.inserted, stats.updated), (1, 0));
        let l = &list(&conn).unwrap()[0];
        assert_eq!(l.title, "Hand");
        assert_eq!(l.source.as_deref(), Some("operator"), "missing source defaults to operator");
        assert!(l.created_ts > 0 && l.updated_ts > 0, "missing timestamps stamped now");
    }

    #[test]
    fn import_mixed_authored_and_explicit_ids_does_not_collide() {
        // A hand-authored file mixing a no-id entry with an explicit id=1, into
        // a fresh store. Processing the explicit-id row first means the no-id
        // row's autoincrement lands above it — no UNIQUE collision, both land.
        let tmp = TempDir::new().unwrap();
        let mut conn = open_at(&tmp.path().join("lessons.db")).unwrap();
        let json = r#"{"schema_version":1,"lessons":[
            {"title":"authored","body":"no id"},
            {"id":1,"title":"explicit","body":"id one"}
        ]}"#;
        let stats = import_json(&mut conn, json).unwrap();
        assert_eq!((stats.inserted, stats.updated), (2, 0));
        let all = list(&conn).unwrap();
        assert_eq!(all.len(), 2, "both rows persisted, no collision");
        assert!(all.iter().any(|l| l.title == "explicit" && l.id == Some(1)));
        assert!(all.iter().any(|l| l.title == "authored" && l.id != Some(1)));
    }

    #[test]
    fn import_is_atomic_on_malformed_input() {
        let tmp = TempDir::new().unwrap();
        let mut conn = open_at(&tmp.path().join("lessons.db")).unwrap();
        add(&conn, "pre-existing", "b", None, None).unwrap();
        // Malformed JSON → parse error before the transaction touches anything.
        assert!(import_json(&mut conn, "{not json").is_err());
        assert_eq!(list(&conn).unwrap().len(), 1, "store unchanged after a failed import");
    }
}
