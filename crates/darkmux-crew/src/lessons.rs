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
use rusqlite::{params, Connection};
use serde::Serialize;
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
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Lesson {
    /// DB rowid — `None` for a lesson being constructed for insert, `Some` when
    /// read back (so a future `edit`/`remove` verb can target it).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<i64>,
    /// Short statement of the rule / decision.
    pub title: String,
    /// The detail — explains the WHY, not just the rule.
    pub body: String,
    /// Optional area scope. `None` = engagement-level (applies everywhere);
    /// `Some(path)` = scoped to a file. File-precision retrieval is a later
    /// increment — today every lesson is injected (engagement-coarse).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub file: Option<String>,
    /// Provenance — who authored it (`"operator"`, `"orchestrator"`). The
    /// authority signal the retrieve phase ranks on; defaults to `"operator"`
    /// when added via the CLI.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    pub created_ts: i64,
    pub updated_ts: i64,
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

/// All lessons, most-recently-updated first.
pub fn list(conn: &Connection) -> Result<Vec<Lesson>> {
    let mut stmt = conn.prepare(
        "SELECT id, title, body, file, source, created_ts, updated_ts
         FROM lessons ORDER BY updated_ts DESC, id DESC",
    )?;
    let rows = stmt.query_map([], |r| {
        Ok(Lesson {
            id: Some(r.get(0)?),
            title: r.get(1)?,
            body: r.get(2)?,
            file: r.get(3)?,
            source: r.get(4)?,
            created_ts: r.get(5)?,
            updated_ts: r.get(6)?,
        })
    })?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
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
}
