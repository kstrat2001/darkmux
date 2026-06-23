//! SQLite-backed derived index over crew manifests. **Phase B of issue #45.**
//!
//! # Schema rationale
//!
//! The non-obvious choices and why:
//!
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
//! - **`source_files.kind` covers ALL entity types** —
//!   `kind IN ('role', 'skill', 'crew', 'mission', 'sprint')`.
//! - **`PRAGMA user_version` for schema versioning** — SQLite-native; no
//!   external migration tooling needed.
//! - **`role_escalation_targets` table** — the
//!   `EscalationContract::HandOffTo(String)` enum variant carries a
//!   `role_id` payload. Storing the enum variant as a plain string in
//!   `roles.escalation_contract_tag` loses the FK relationship. So:
//!   `roles.escalation_contract_tag` stores only the tag
//!   (`'bail-with-explanation'` / `'retry-with-hint'` / `'hand-off-to'`)
//!   and `'hand-off-to'` requires a row in `role_escalation_targets`.
//! - **`unmatched_terms` table** — for allocator FTS fallback (terms in
//!   ticket text that did not match any skill keyword).
//! - **FTS5 sync triggers** — `skill_keywords_ai` / `_ad` / `_au`
//!   propagate INSERT / DELETE / UPDATE on `skill_keywords` to the
//!   `skill_keywords_fts` mirror automatically.
//! - **`Mission.sprint_ids` is JSON-only**, NOT a denormalized DB column —
//!   sprint membership is derived from `sprints WHERE mission_id = ?`.
//!   The JSON-side field stays in the manifest for operator hand-editing.
//! - **`outcomes.sprint_id` + `outcomes.mission_id`** — both columns
//!   present. A composite FK to `sprints(id, mission_id)` isn't
//!   expressible in SQLite without extra triggers; the redundancy is a
//!   deliberate denormalization for query ergonomics + application-side
//!   validation.
//!
//! # Public surface
//!
//! - `rebuild()` — DELETE+INSERT all derivable tables from manifests on
//!   disk. Idempotent.
//! - `status()` — last-rebuild timestamp, per-kind source count, drift
//!   summary (added / modified / deleted user-side files).
//!
//! # Acceptance criteria (from #45) covered here
//!
//! - Index rebuild from a clean state produces a queryable DB
//! - FTS5 keyword search produces ranked skills for a known fixture
//! - Index rebuild is idempotent (running twice produces the same state)
//! - `darkmux crew index status` flags drift (file mtime + content_hash)
//!
//! NOT covered here: CRUD CLI for each entity, audit-log / outcomes /
//! allocator population. Those land in follow-up PRs.

#![allow(dead_code)]

use crate::loader;
use crate::types::*;
use darkmux_types::paths::{resolve, ResolveScope};
use anyhow::{Context, Result};
use rusqlite::{params, Connection, OptionalExtension};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

/// Bumped to 2 in refactor 0 (`capability` → `skill` rename, #448); to 3
/// for the #95 mission/sprint transition-timestamp columns (#914); to 4 for
/// the #994 engagement-context `cautions` table (derived from the flow stream);
/// to 5 (#999) when the scaffolded `knowledge` table was dropped — authored
/// lessons live in their own durable `lessons.db` store now (`darkmux lessons`),
/// not in this derived crew index. Two independent mechanisms in [`init_schema`]
/// keep an on-disk DB current: (1) version-gated migration blocks drop legacy
/// artifacts (the `< 2` capability-rename tables; the `< 5` vestigial
/// `knowledge` table); (2) every rebuild drops + recreates the derived
/// `REBUILD_TABLES` so a column added to the DDL (e.g. the #95 timestamps)
/// lands even on a pre-existing DB — a plain `CREATE TABLE IF NOT EXISTS`
/// cannot evolve an existing table's columns. Bumping this constant also
/// gives the read path a staleness signal (see [`ensure_fresh_index`]).
/// Allocator tables + `meta_kv` are NOT derived and are preserved across
/// rebuilds. No data is lost — every dropped table is rebuilt from the on-disk
/// manifests + the flow stream.
const SCHEMA_VERSION: i32 = 5;

const SCHEMA_SQL: &str = r#"
CREATE TABLE IF NOT EXISTS source_files (
    path          TEXT PRIMARY KEY,
    kind          TEXT NOT NULL CHECK (kind IN ('role','skill','crew','mission','sprint')),
    mtime         INTEGER NOT NULL,
    content_hash  TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS roles (
    id                      TEXT PRIMARY KEY,
    description             TEXT NOT NULL,
    prompt_path             TEXT,
    tool_palette_json       TEXT NOT NULL,
    escalation_contract_tag TEXT NOT NULL CHECK (
        escalation_contract_tag IN ('bail-with-explanation','retry-with-hint','hand-off-to')
    )
);

CREATE TABLE IF NOT EXISTS role_escalation_targets (
    role_id        TEXT PRIMARY KEY REFERENCES roles(id) ON DELETE CASCADE,
    target_role_id TEXT NOT NULL REFERENCES roles(id)
);

CREATE TABLE IF NOT EXISTS skills (
    id          TEXT PRIMARY KEY,
    description TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS role_skills (
    role_id       TEXT NOT NULL REFERENCES roles(id) ON DELETE CASCADE,
    skill_id TEXT NOT NULL REFERENCES skills(id) ON DELETE CASCADE,
    PRIMARY KEY (role_id, skill_id)
);
CREATE INDEX IF NOT EXISTS idx_role_skills_skill_id ON role_skills(skill_id);

CREATE TABLE IF NOT EXISTS skill_keywords (
    skill_id TEXT NOT NULL REFERENCES skills(id) ON DELETE CASCADE,
    keyword       TEXT NOT NULL,
    weight        REAL NOT NULL,
    PRIMARY KEY (skill_id, keyword)
);

CREATE VIRTUAL TABLE IF NOT EXISTS skill_keywords_fts USING fts5(
    keyword,
    skill_id UNINDEXED,
    weight        UNINDEXED
);

CREATE TRIGGER IF NOT EXISTS skill_keywords_ai
AFTER INSERT ON skill_keywords
BEGIN
    INSERT INTO skill_keywords_fts(keyword, skill_id, weight)
    VALUES (NEW.keyword, NEW.skill_id, NEW.weight);
END;

CREATE TRIGGER IF NOT EXISTS skill_keywords_ad
AFTER DELETE ON skill_keywords
BEGIN
    DELETE FROM skill_keywords_fts
    WHERE keyword = OLD.keyword AND skill_id = OLD.skill_id;
END;

CREATE TRIGGER IF NOT EXISTS skill_keywords_au
AFTER UPDATE ON skill_keywords
BEGIN
    DELETE FROM skill_keywords_fts
    WHERE keyword = OLD.keyword AND skill_id = OLD.skill_id;
    INSERT INTO skill_keywords_fts(keyword, skill_id, weight)
    VALUES (NEW.keyword, NEW.skill_id, NEW.weight);
END;

CREATE TABLE IF NOT EXISTS crews (
    id          TEXT PRIMARY KEY,
    description TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS crew_members (
    crew_id  TEXT NOT NULL REFERENCES crews(id) ON DELETE CASCADE,
    role_id  TEXT NOT NULL REFERENCES roles(id),
    position TEXT NOT NULL CHECK (position IN ('lead','support')),
    PRIMARY KEY (crew_id, role_id)
);

CREATE TABLE IF NOT EXISTS missions (
    id          TEXT PRIMARY KEY,
    description TEXT NOT NULL,
    status      TEXT NOT NULL CHECK (status IN ('active','closed','paused')),
    created_ts  INTEGER NOT NULL,
    started_ts  INTEGER,  -- Active transition; #95
    closed_ts   INTEGER,  -- Closed transition (terminal); #95
    paused_ts   INTEGER   -- most-recent Paused transition; #95
);

CREATE TABLE IF NOT EXISTS sprints (
    id              TEXT NOT NULL,
    mission_id      TEXT NOT NULL REFERENCES missions(id) ON DELETE CASCADE,
    description     TEXT NOT NULL,
    status          TEXT NOT NULL CHECK (status IN ('planned','running','complete','abandoned')),
    depends_on_json TEXT NOT NULL DEFAULT '[]',
    created_ts      INTEGER NOT NULL,
    started_ts      INTEGER,  -- Running transition; #95
    completed_ts    INTEGER,  -- Complete transition (terminal); #95
    abandoned_ts    INTEGER,  -- Abandoned transition (cleared on restart); #95
    PRIMARY KEY (id, mission_id)
);

-- Allocator territory (#46). Empty in Phase B; defined here for FK compatibility.
CREATE TABLE IF NOT EXISTS allocations (
    id                    INTEGER PRIMARY KEY AUTOINCREMENT,
    mission_id            TEXT,
    sprint_id             TEXT,
    ticket_text           TEXT,
    ticket_hash           TEXT,
    suggested_crew_json   TEXT,
    confidence            REAL,
    operator_override_json TEXT,
    final_crew_json       TEXT,
    ts                    INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS outcomes (
    allocation_id     INTEGER PRIMARY KEY REFERENCES allocations(id) ON DELETE CASCADE,
    sprint_id         TEXT,
    mission_id        TEXT,
    wall_seconds      INTEGER,
    success           INTEGER NOT NULL CHECK (success IN (0,1)),
    postmortem_notes  TEXT,
    ts                INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS unmatched_terms (
    term      TEXT PRIMARY KEY,
    count     INTEGER NOT NULL DEFAULT 0,
    last_seen INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS meta_kv (
    key   TEXT PRIMARY KEY,
    value TEXT NOT NULL
);

-- (#994) cautions — DERIVED from the flow stream (a row per detector firing).
-- The mistakes the local crew has already made, keyed (when known) to the file
-- they happened in, so the retrieve slice can feed the relevant ones into the
-- next dispatch's brief. In REBUILD_TABLES → dropped + re-derived every rebuild
-- (the flow JSONL is the source of truth; this is a queryable index of it).
-- `file` is NULL for engagement-level firings (turn-level detectors, or a
-- cycle on a non-file tool). `code_hash` is the firing-time file hash for
-- staleness ranking — NULL until the runtime-side capture slice 2 emits it.
CREATE TABLE IF NOT EXISTS cautions (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    file        TEXT,
    kind        TEXT NOT NULL,
    severity    TEXT NOT NULL,
    detail      TEXT NOT NULL,
    code_hash   TEXT,
    mission_id  TEXT,
    sprint_id   TEXT,
    session_id  TEXT,
    role        TEXT,
    model       TEXT,
    ts          TEXT NOT NULL   -- RFC3339 (lexicographic == chronological)
);
CREATE INDEX IF NOT EXISTS idx_cautions_file ON cautions(file);

-- (#999) The operator-authored engagement context (conventions, constraints,
-- decisions + the "why") lives in its own durable `lessons.db` store now
-- (`darkmux lessons`), NOT in this derived crew index. The scaffolded
-- `knowledge` table that briefly lived here (#998) is dropped by the `< 5`
-- migration in `init_schema`.
"#;

/// Tables (and only tables) that `rebuild()` clears + repopulates. Most are
/// derived from manifests; `cautions` is derived from the flow stream (#994).
/// Allocator tables (allocations / outcomes / unmatched_terms) and `meta_kv`
/// are preserved across rebuilds — those carry runtime state, not derived data.
/// (Operator-authored lessons live in a separate `lessons.db` store, not in
/// this index at all — #999.)
const REBUILD_TABLES: &[&str] = &[
    "source_files",
    "role_escalation_targets",
    "role_skills",
    "skill_keywords",
    "crew_members",
    "sprints",
    "missions",
    "crews",
    "roles",
    "skills",
    "cautions",
];

/// Default index path: `<paths.root>/index.db`. Resolved through the same
/// Stable across releases — changing this silently invalidates every operator's
/// existing index. Tests use the `_at(&path)` variants (`rebuild_at`,
/// `role_list_at`, `crew_list_at`, etc.) rather than overriding this path.
/// (#1012) ForceUser, NOT Auto: the index is DERIVED from the user-scope crew /
/// missions / sprints (now resolved via `user_state_root` = ForceUser), so it
/// must be user-scoped to match its content — a project-scoped index of
/// user-scoped data is incoherent, and a bare `<cwd>/.darkmux/` must not relocate
/// it. In the common no-project-`.darkmux` case `Auto` already resolved to user,
/// so the path is unchanged; only a repo with a stray `.darkmux/` is corrected
/// (one rebuild). DARKMUX_HOME still wins.
pub fn default_index_path() -> PathBuf {
    resolve(ResolveScope::ForceUser).root.join("index.db")
}

fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// FNV-1a 64-bit content hash for drift detection. NOT cryptographic —
/// strictly to disambiguate "mtime changed but content didn't" from real
/// edits. 6-line inline implementation per the project's "small inline
/// beats a crate" convention (`src/providers/coding_task.rs` precedent).
fn fnv1a_64(bytes: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf29ce4_84222325;
    for &b in bytes {
        hash ^= b as u64;
        hash = hash.wrapping_mul(0x100000001b3);
    }
    hash
}

fn content_hash_hex(bytes: &[u8]) -> String {
    format!("{:016x}", fnv1a_64(bytes))
}

fn file_mtime(path: &Path) -> Result<i64> {
    let md = fs::metadata(path)
        .with_context(|| format!("stat {}", path.display()))?;
    let modified = md
        .modified()
        .with_context(|| format!("mtime {}", path.display()))?;
    Ok(modified
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0))
}

fn escalation_tag(contract: &EscalationContract) -> &'static str {
    match contract {
        EscalationContract::BailWithExplanation => "bail-with-explanation",
        EscalationContract::RetryWithHint => "retry-with-hint",
        EscalationContract::HandOffTo(_) => "hand-off-to",
    }
}

fn position_str(p: Position) -> &'static str {
    match p {
        Position::Lead => "lead",
        Position::Support => "support",
    }
}

fn mission_status_str(s: MissionStatus) -> &'static str {
    match s {
        MissionStatus::Active => "active",
        MissionStatus::Closed => "closed",
        MissionStatus::Paused => "paused",
    }
}

fn sprint_status_str(s: SprintStatus) -> &'static str {
    match s {
        SprintStatus::Planned => "planned",
        SprintStatus::Running => "running",
        SprintStatus::Complete => "complete",
        SprintStatus::Abandoned => "abandoned",
    }
}

pub fn open_index(path: &Path) -> Result<Connection> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            fs::create_dir_all(parent)
                .with_context(|| format!("creating index parent {}", parent.display()))?;
        }
    }
    let conn = Connection::open(path)
        .with_context(|| format!("opening index db at {}", path.display()))?;
    conn.execute_batch("PRAGMA foreign_keys = ON;")?;
    Ok(conn)
}

fn init_schema(conn: &Connection) -> Result<()> {
    // Migration: refactor 0 renamed `capability` → `skill` (#448). Drop
    // the legacy tables / triggers / index / virtual table BEFORE
    // applying the new schema so the IF-NOT-EXISTS in SCHEMA_SQL
    // creates fresh `skill`-named state and the old ones don't linger.
    // Idempotent + no-op on fresh DBs (DROP IF EXISTS).
    let current_version: i32 = conn
        .query_row("PRAGMA user_version", [], |r| r.get(0))
        .unwrap_or(0);
    if current_version < 2 {
        conn.execute_batch(
            "DROP TRIGGER IF EXISTS capability_keywords_ai;
             DROP TRIGGER IF EXISTS capability_keywords_ad;
             DROP TRIGGER IF EXISTS capability_keywords_au;
             DROP TABLE IF EXISTS capability_keywords_fts;
             DROP TABLE IF EXISTS capability_keywords;
             DROP INDEX IF EXISTS idx_role_capabilities_cap;
             DROP TABLE IF EXISTS role_capabilities;
             DROP TABLE IF EXISTS capabilities;",
        )
        .context("dropping pre-rename legacy tables (refactor 0, #448)")?;
    }

    // Migration (#999): the scaffolded `knowledge` table (#998) is superseded
    // by the durable `lessons.db` store — authored lessons live there now, not
    // in this derived crew index. Drop the vestigial table. Idempotent + a
    // no-op on DBs that never had it (DROP IF EXISTS).
    if current_version < 5 {
        conn.execute_batch("DROP TABLE IF EXISTS knowledge;")
            .context("dropping the vestigial index.db knowledge table (#999)")?;
    }

    // Self-heal derived-table schema drift (#914): drop + recreate every
    // derived table on each rebuild so a column added to the DDL (e.g. the
    // #95 mission/sprint timestamp columns) lands even on a pre-existing DB —
    // the `CREATE TABLE IF NOT EXISTS` in SCHEMA_SQL below cannot add columns
    // to a table that already exists. Dropping `skill_keywords` also drops
    // its three FTS-sync triggers, which SCHEMA_SQL then recreates; the FTS
    // mirror itself is cleared + refilled by `populate`. FKs are toggled off
    // for the drop so order is immaterial (FK-cascading re-INSERT happens in
    // `populate`). Allocator tables + `meta_kv` are NOT in REBUILD_TABLES and
    // carry non-derived runtime state, so they are deliberately preserved.
    let mut drop_sql = String::from("PRAGMA foreign_keys = OFF;\n");
    for tbl in REBUILD_TABLES {
        drop_sql.push_str("DROP TABLE IF EXISTS ");
        drop_sql.push_str(tbl);
        drop_sql.push_str(";\n");
    }
    drop_sql.push_str("PRAGMA foreign_keys = ON;\n");
    conn.execute_batch(&drop_sql)
        .context("dropping derived tables for a clean rebuild (#914)")?;

    conn.execute_batch(SCHEMA_SQL)
        .context("applying index schema")?;
    conn.execute_batch(&format!("PRAGMA user_version = {SCHEMA_VERSION};"))
        .context("setting user_version")?;
    Ok(())
}

/// Enumerate user-side manifest files for one entity kind. Returns
/// (path, role-or-cap-or-..-id, content) tuples. Only enumerates the
/// caller-resolved directory; builtins are intentionally excluded so
/// drift detection scopes to operator-owned state. Caller is
/// responsible for resolving the right dir (e.g., `loader::roles_dir()`)
/// so the post-Beat-33 dual-read fallback is honored.
fn enumerate_user_files(dir: &Path) -> Result<Vec<(PathBuf, String, Vec<u8>)>> {
    if !dir.exists() {
        return Ok(Vec::new());
    }
    let mut out = Vec::new();
    for entry in fs::read_dir(dir)
        .with_context(|| format!("reading {}", dir.display()))?
    {
        let entry = match entry {
            Ok(e) => e,
            Err(_) => continue,
        };
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let name = match path.file_name().and_then(|n| n.to_str()) {
            Some(n) => n,
            None => continue,
        };
        if !name.ends_with(".json") {
            continue;
        }
        // #892: strip exactly one ".json" suffix (trim_end_matches would strip
        // repeated trailing matches).
        let id = name.strip_suffix(".json").unwrap_or(name).to_string();
        let bytes = fs::read(&path)
            .with_context(|| format!("reading {}", path.display()))?;
        out.push((path, id, bytes));
    }
    Ok(out)
}

/// Resolve the per-kind directory through the loader's dual-read helpers
/// so the index respects the post-Beat-33 layout (canonical-first,
/// legacy fallback). Centralized here so populate() + status_at() never
/// drift apart.
fn kind_to_dir(kind: &str) -> PathBuf {
    match kind {
        "role" => loader::roles_dir(),
        "skill" => loader::skills_dir(),
        "crew" => loader::crews_dir(),
        "mission" => loader::missions_dir(),
        "sprint" => loader::sprints_dir(),
        _ => unreachable!("unknown index kind: {kind}"),
    }
}

/// Insert merged-effective entities into all derived tables, plus populate
/// `source_files` with whichever user-side files exist on disk.
fn populate(conn: &mut Connection) -> Result<()> {
    let roles = loader::load_roles()?;
    let skills = loader::load_skills()?;
    let crews = loader::load_crews()?;
    let missions = loader::load_missions()?;
    let sprints = loader::load_sprints()?;

    let tx = conn.transaction()?;
    tx.execute_batch("PRAGMA defer_foreign_keys = ON;")?;

    for tbl in REBUILD_TABLES {
        tx.execute(&format!("DELETE FROM {tbl};"), [])?;
    }
    // FTS5 mirror has triggers wired to skill_keywords, but the
    // DELETE-cascade only fires per-row — clearing skill_keywords
    // above already empties the FTS mirror via the AD trigger. Belt and
    // braces: ensure FTS is empty before re-INSERT.
    tx.execute("DELETE FROM skill_keywords_fts;", [])?;

    // Insert skills + keywords first (roles depend on skills via FK).
    for skill in &skills {
        tx.execute(
            "INSERT INTO skills (id, description) VALUES (?1, ?2)",
            params![skill.id, skill.description],
        )?;
        for kw in &skill.keywords {
            tx.execute(
                "INSERT INTO skill_keywords (skill_id, keyword, weight) VALUES (?1, ?2, ?3)",
                params![skill.id, kw.keyword, kw.weight as f64],
            )?;
        }
    }

    // (#906) Pre-validate escalation hand-off targets resolve to a known
    // role BEFORE inserting, so a dangling `HandOffTo` target fails with a
    // clear, role-named message instead of an opaque deferred-FK abort at
    // COMMIT that rolls back the entire rebuild. Validates against the full
    // role set so forward references (target defined later) are allowed.
    let known_role_ids: std::collections::HashSet<&str> =
        roles.iter().map(|r| r.id.as_str()).collect();
    for role in &roles {
        if let EscalationContract::HandOffTo(target) = &role.escalation_contract {
            if !known_role_ids.contains(target.as_str()) {
                anyhow::bail!(
                    "role `{}` declares an escalation hand-off to `{}`, but no role with \
                     that id exists — fix the `escalation_contract` hand-off target or add \
                     the missing role",
                    role.id,
                    target
                );
            }
        }
    }

    // Roles.
    for role in &roles {
        let tool_palette_json = serde_json::to_string(&role.tool_palette)?;
        let tag = escalation_tag(&role.escalation_contract);
        let prompt_path = role
            .prompt_path
            .as_ref()
            .map(|p| p.to_string_lossy().into_owned());
        tx.execute(
            "INSERT INTO roles (id, description, prompt_path, tool_palette_json, escalation_contract_tag)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![role.id, role.description, prompt_path, tool_palette_json, tag],
        )?;
        if let EscalationContract::HandOffTo(target) = &role.escalation_contract {
            tx.execute(
                "INSERT INTO role_escalation_targets (role_id, target_role_id) VALUES (?1, ?2)",
                params![role.id, target],
            )?;
        }
        for skill_id in &role.skills {
            tx.execute(
                "INSERT INTO role_skills (role_id, skill_id) VALUES (?1, ?2)",
                params![role.id, skill_id],
            )?;
        }
    }

    // Crews + members.
    for crew in &crews {
        tx.execute(
            "INSERT INTO crews (id, description) VALUES (?1, ?2)",
            params![crew.id, crew.description],
        )?;
        for m in &crew.members {
            tx.execute(
                "INSERT INTO crew_members (crew_id, role_id, position) VALUES (?1, ?2, ?3)",
                params![crew.id, m.role_id, position_str(m.position)],
            )?;
        }
    }

    // Missions.
    for mission in &missions {
        tx.execute(
            "INSERT INTO missions (id, description, status, created_ts, started_ts, closed_ts, paused_ts)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                mission.id,
                mission.description,
                mission_status_str(mission.status),
                mission.created_ts as i64,
                mission.started_ts.map(|t| t as i64),
                mission.closed_ts.map(|t| t as i64),
                mission.paused_ts.map(|t| t as i64),
            ],
        )?;
    }

    // Sprints.
    for sprint in &sprints {
        let depends_on_json = serde_json::to_string(&sprint.depends_on)?;
        tx.execute(
            "INSERT INTO sprints (id, mission_id, description, status, depends_on_json, created_ts, started_ts, completed_ts, abandoned_ts)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            params![
                sprint.id,
                sprint.mission_id,
                sprint.description,
                sprint_status_str(sprint.status),
                depends_on_json,
                sprint.created_ts as i64,
                sprint.started_ts.map(|t| t as i64),
                sprint.completed_ts.map(|t| t as i64),
                sprint.abandoned_ts.map(|t| t as i64),
            ],
        )?;
    }

    // source_files — scan user-side disk only (builtins are version-gated by
    // the stored darkmux_version in meta_kv, not by per-file mtime). Each
    // kind's directory is resolved through the loader's dual-read helpers
    // so the legacy <root>/crew/<subdir>/ layout still indexes correctly.
    let kinds: &[&str] = &["role", "skill", "crew", "mission", "sprint"];
    for kind in kinds {
        let dir = kind_to_dir(kind);
        for (path, _id, bytes) in enumerate_user_files(&dir)? {
            let mtime = file_mtime(&path)?;
            let hash = content_hash_hex(&bytes);
            tx.execute(
                "INSERT INTO source_files (path, kind, mtime, content_hash) VALUES (?1, ?2, ?3, ?4)",
                params![path.to_string_lossy(), *kind, mtime, hash],
            )?;
        }
    }

    // cautions — derive from the flow stream (#994). Source of truth is the
    // per-day JSONL; this is a queryable index of the detector firings in it.
    derive_cautions(&tx)?;

    // meta_kv — capture rebuild context for status() to surface later.
    let now = now_unix();
    let darkmux_version = env!("CARGO_PKG_VERSION");
    upsert_meta(&tx, "last_rebuild_ts", &now.to_string())?;
    upsert_meta(&tx, "darkmux_version", darkmux_version)?;
    upsert_meta(&tx, "schema_version", &SCHEMA_VERSION.to_string())?;

    tx.commit()?;
    Ok(())
}

fn upsert_meta(conn: &Connection, key: &str, value: &str) -> Result<()> {
    conn.execute(
        "INSERT INTO meta_kv (key, value) VALUES (?1, ?2)
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        params![key, value],
    )?;
    Ok(())
}

fn read_meta(conn: &Connection, key: &str) -> Result<Option<String>> {
    let v = conn
        .query_row("SELECT value FROM meta_kv WHERE key = ?1", params![key], |r| r.get::<_, String>(0))
        .optional()?;
    Ok(v)
}

/// (#994) Whether a flow record is a detector firing — the source of a
/// *caution*. Detector telemetry records are `category=Telemetry` with
/// `source="detector"` (the discriminator the observability viewer keys on);
/// the capture path emits one per detector trajectory event.
///
/// (#994 QA / #1002) This typed predicate has a Value-based twin in
/// `mission_run::mission_cautions` (the hot per-dispatch brief-inject path,
/// which scans the raw flow stream). They classify the same records from
/// different representations — keep them in sync: a change to `source`/
/// `category` semantics must update BOTH.
fn is_detector_caution(rec: &darkmux_flow::FlowRecord) -> bool {
    matches!(rec.category, darkmux_flow::Category::Telemetry)
        && rec.source.as_deref() == Some("detector")
}

/// (#994) Pull the caution columns out of a detector record's `payload`
/// (`{kind, severity, detail, area?}` — see `detector_telemetry_payload`).
/// Returns `(kind, severity, detail, file, code_hash)`; `file` and `code_hash`
/// are `None` when the firing carries no `area` (engagement-level) or no
/// firing-time hash (until runtime slice 2). Defaults are defensive — a
/// malformed payload still yields a storable row rather than dropping the
/// firing.
fn caution_fields(
    rec: &darkmux_flow::FlowRecord,
) -> (String, String, String, Option<String>, Option<String>) {
    let payload = rec.payload.as_ref();
    let str_at = |k: &str| {
        payload
            .and_then(|v| v.get(k))
            .and_then(|v| v.as_str())
            .map(String::from)
    };
    let kind = str_at("kind").unwrap_or_else(|| "unknown".to_string());
    let severity = str_at("severity").unwrap_or_else(|| "warn".to_string());
    let detail = str_at("detail").unwrap_or_default();

    let area = payload.and_then(|v| v.get("area"));
    let file = area
        .and_then(|a| a.get("files"))
        .and_then(|f| f.as_array())
        .and_then(|arr| arr.first())
        .and_then(|v| v.as_str())
        .map(String::from);
    let code_hash = area
        .and_then(|a| a.get("code_hash"))
        .and_then(|v| v.as_str())
        .map(String::from);

    (kind, severity, detail, file, code_hash)
}

/// (#994) Derive the `cautions` table from the flow stream: scan the per-day
/// JSONL files under `flows_dir()`, parse each line as a `FlowRecord`, and
/// insert one row per detector firing — keyed (when present) to the file the
/// firing touched. Lines that don't parse as a `FlowRecord` (the schema-header
/// line each file opens with, a partial tail write) are skipped, matching the
/// casual LocalFileSink reader contract (unknown/garbage lines ignored).
///
/// The scan is currently unbounded (every day file). Rebuilds are occasional
/// (schema bump / manual / first read after a stale index), so this is
/// acceptable for the MVP; bounding to recent day files is a follow-up if a
/// large flow corpus makes the rebuild slow.
fn derive_cautions(tx: &Connection) -> Result<()> {
    let dir = darkmux_flow::flows_dir();
    if !dir.exists() {
        return Ok(());
    }
    let mut insert = tx.prepare(
        "INSERT INTO cautions
            (file, kind, severity, detail, code_hash, mission_id, sprint_id, session_id, role, model, ts)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
    )?;
    for entry in
        fs::read_dir(&dir).with_context(|| format!("reading flows dir {}", dir.display()))?
    {
        let path = match entry {
            Ok(e) => e.path(),
            Err(_) => continue,
        };
        if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
            continue;
        }
        let content = match fs::read_to_string(&path) {
            Ok(c) => c,
            Err(_) => continue,
        };
        for line in content.lines() {
            let rec = match serde_json::from_str::<darkmux_flow::FlowRecord>(line) {
                Ok(r) => r,
                Err(_) => continue,
            };
            if !is_detector_caution(&rec) {
                continue;
            }
            let (kind, severity, detail, file, code_hash) = caution_fields(&rec);
            insert.execute(params![
                file,
                kind,
                severity,
                detail,
                code_hash,
                rec.mission_id,
                rec.sprint_id,
                rec.session_id,
                rec.handle,
                rec.model,
                rec.ts,
            ])?;
        }
    }
    Ok(())
}

/// Internal entry point for tests + the public `rebuild()` wrapper.
pub fn rebuild_at(path: &Path) -> Result<()> {
    let mut conn = open_index(path)?;
    init_schema(&conn)?;
    populate(&mut conn)?;
    Ok(())
}

/// Rebuild the index from manifests on disk. `init_schema` drops + recreates
/// the derived tables, then `populate` refills them — idempotent, and
/// self-healing across schema drift (#914).
pub fn rebuild() -> Result<()> {
    let path = default_index_path();
    rebuild_at(&path)?;
    println!("crew index rebuilt at {}", path.display());
    Ok(())
}

/// Ensure the derived index at `path` exists and matches the current
/// `SCHEMA_VERSION`, rebuilding from manifests if it is missing, stale, or
/// unreadable. The index is derived state (the JSON manifests under the crew
/// root are the source of truth), so an on-demand rebuild is always safe and
/// recoverable — this is what lets the `role` / `crew` read-verbs work
/// without a manual `darkmux crew index rebuild`. (#914)
pub fn ensure_fresh_index(path: &Path) -> Result<()> {
    let fresh = path.exists() && populated_schema_version(path) == Some(SCHEMA_VERSION);
    if !fresh {
        rebuild_at(path)?;
    }
    Ok(())
}

/// Read the schema version recorded by the last *successful* `populate` — the
/// `meta_kv.schema_version` row, which is written inside populate's
/// transaction — or `None` if it's absent/unreadable. This is deliberately
/// NOT `PRAGMA user_version`: `init_schema` bumps `user_version` *before*
/// `populate` runs, so a rebuild whose populate failed (rolling back to empty
/// derived tables) would still report the current `user_version` and the lazy
/// read path would trust it as fresh — re-arming the silent-stale failure
/// #914 exists to eliminate. `meta_kv.schema_version` only advances when the
/// refill actually commits, so a failed populate correctly reads as stale and
/// is rebuilt on the next access. `None` (absent table/row, unreadable DB) is
/// treated as stale by callers. (#914)
fn populated_schema_version(path: &Path) -> Option<i32> {
    let conn = Connection::open(path).ok()?;
    let raw: String = conn
        .query_row(
            "SELECT value FROM meta_kv WHERE key = 'schema_version'",
            [],
            |r| r.get(0),
        )
        .ok()?;
    raw.parse::<i32>().ok()
}

#[derive(Debug, Default, PartialEq)]
struct StatusReport {
    last_rebuild_ts: Option<i64>,
    stored_darkmux_version: Option<String>,
    stored_schema_version: Option<i32>,
    per_kind_counts: Vec<(String, i64)>,
    caution_count: i64, // (#994) derived detector firings in the index
    added: Vec<(String, String)>,    // (kind, path)
    modified: Vec<(String, String)>, // (kind, path)
    deleted: Vec<(String, String)>,  // (kind, path)
}

fn status_at(path: &Path) -> Result<StatusReport> {
    if !path.exists() {
        anyhow::bail!(
            "no index at {} — run `darkmux crew index rebuild` first",
            path.display()
        );
    }
    let conn = open_index(path)?;

    let last_rebuild_ts = read_meta(&conn, "last_rebuild_ts")?
        .and_then(|s| s.parse::<i64>().ok());
    let stored_darkmux_version = read_meta(&conn, "darkmux_version")?;
    let stored_schema_version = read_meta(&conn, "schema_version")?
        .and_then(|s| s.parse::<i32>().ok());

    let mut report = StatusReport {
        last_rebuild_ts,
        stored_darkmux_version,
        stored_schema_version,
        ..Default::default()
    };

    // Per-kind source counts.
    let mut counts_stmt = conn.prepare(
        "SELECT kind, COUNT(*) FROM source_files GROUP BY kind ORDER BY kind",
    )?;
    let count_rows = counts_stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))?;
    for row in count_rows {
        report.per_kind_counts.push(row?);
    }

    // (#994) Derived caution count. (Authored *lessons* live in their own
    // durable `lessons.db` store — surfaced by `darkmux lessons list`, not the
    // crew index; the scaffolded index.db `knowledge` table was dropped in #999.)
    report.caution_count = conn
        .query_row("SELECT COUNT(*) FROM cautions", [], |r| r.get(0))
        .unwrap_or(0);

    // Drift detection: compare on-disk state to source_files. Each kind's
    // directory is resolved through the loader's dual-read helpers so
    // a legacy <root>/crew/<subdir>/ layout still drift-detects correctly.
    let kinds: &[&str] = &["role", "skill", "crew", "mission", "sprint"];

    // Build set of all paths currently recorded.
    let mut recorded_paths: std::collections::BTreeMap<String, (String, i64, String)> =
        std::collections::BTreeMap::new();
    {
        let mut stmt = conn.prepare("SELECT path, kind, mtime, content_hash FROM source_files")?;
        let rows = stmt.query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, i64>(2)?,
                r.get::<_, String>(3)?,
            ))
        })?;
        for row in rows {
            let (path, kind, mtime, hash) = row?;
            recorded_paths.insert(path, (kind, mtime, hash));
        }
    }

    let mut seen_on_disk: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    for kind in kinds {
        let dir = kind_to_dir(kind);
        for (path, _id, bytes) in enumerate_user_files(&dir)? {
            let path_str = path.to_string_lossy().into_owned();
            seen_on_disk.insert(path_str.clone());
            match recorded_paths.get(&path_str) {
                None => {
                    report.added.push(((*kind).to_string(), path_str));
                }
                Some((_recorded_kind, _recorded_mtime, recorded_hash)) => {
                    // #891: compare the content hash UNCONDITIONALLY. An edit
                    // that doesn't advance mtime (a same-second write, or an
                    // mtime-preserving copy/restore) still changed the file,
                    // and the hash — not mtime — is the source of truth for
                    // "modified". Gating the hash check behind a mtime change
                    // silently missed those edits.
                    let cur_hash = content_hash_hex(&bytes);
                    if &cur_hash != recorded_hash {
                        report.modified.push(((*kind).to_string(), path_str));
                    }
                }
            }
        }
    }
    for (path, (kind, _, _)) in &recorded_paths {
        if !seen_on_disk.contains(path) {
            report.deleted.push((kind.clone(), path.clone()));
        }
    }
    report.added.sort();
    report.modified.sort();
    report.deleted.sort();

    Ok(report)
}

/// Report index status: last-rebuild timestamp, per-kind source count,
/// drift summary (user-side files added / modified / deleted since
/// last rebuild).
pub fn status() -> Result<()> {
    let path = default_index_path();
    let report = status_at(&path)?;

    println!("index: {}", path.display());
    match report.last_rebuild_ts {
        Some(ts) => println!("last_rebuild_ts: {ts}"),
        None => println!("last_rebuild_ts: (none — index has no rebuild record)"),
    }
    if let Some(v) = &report.stored_darkmux_version {
        let cur = env!("CARGO_PKG_VERSION");
        if v != cur {
            println!("darkmux_version: {v} (current binary is {cur} — re-run rebuild to refresh builtins)");
        } else {
            println!("darkmux_version: {v}");
        }
    }
    if let Some(sv) = report.stored_schema_version {
        if sv != SCHEMA_VERSION {
            println!("schema_version: {sv} (current binary expects {SCHEMA_VERSION} — re-run rebuild)");
        } else {
            println!("schema_version: {sv}");
        }
    }
    println!();
    println!("source counts (user-side files):");
    if report.per_kind_counts.is_empty() {
        println!("  (none — no user-side manifests; builtins are version-gated by darkmux_version above)");
    } else {
        for (kind, n) in &report.per_kind_counts {
            println!("  {kind:12} {n}");
        }
    }
    println!();
    // (#994) Derived cautions in the index. Authored lessons are a separate
    // durable store (`darkmux lessons list`), not part of the crew index.
    println!("engagement context:");
    println!("  {:12} {} (derived from the flow stream)", "cautions", report.caution_count);
    println!();
    let drift = report.added.len() + report.modified.len() + report.deleted.len();
    if drift == 0 {
        println!("drift: none");
    } else {
        println!("drift: {drift} user-side change(s) since last rebuild");
        for (kind, p) in &report.added {
            println!("  + [{kind}] {p}");
        }
        for (kind, p) in &report.modified {
            println!("  ~ [{kind}] {p}");
        }
        for (kind, p) in &report.deleted {
            println!("  - [{kind}] {p}");
        }
        println!();
        println!("re-run `darkmux crew index rebuild` to apply.");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::env;
    use tempfile::TempDir;

    /// RAII guard: point DARKMUX_CREW_DIR at a TempDir for the test's
    /// lifetime. Mirrors the loader's pattern; serialized via #[serial].
    ///
    /// (#994) Also isolates DARKMUX_FLOWS_DIR to an (initially absent) subdir of
    /// the same TempDir: the `cautions` derive scans `flows_dir()` on every
    /// rebuild, so without this a crew rebuild test would read the operator's
    /// real `~/.darkmux/flows`. An un-seeded test thus gets an empty stream by
    /// construction (the derive no-ops); a test that wants cautions seeds the
    /// stream via `write_flow_day`.
    struct CrewDirGuard {
        prev_crew: Option<String>,
        prev_flows: Option<String>,
        tmp: TempDir,
    }

    impl CrewDirGuard {
        fn new() -> Self {
            let tmp = TempDir::new().unwrap();
            let prev_crew = env::var("DARKMUX_CREW_DIR").ok();
            let prev_flows = env::var("DARKMUX_FLOWS_DIR").ok();
            unsafe {
                env::set_var("DARKMUX_CREW_DIR", tmp.path());
                env::set_var("DARKMUX_FLOWS_DIR", tmp.path().join("flows"));
            }
            Self { prev_crew, prev_flows, tmp }
        }

        fn path(&self) -> &Path {
            self.tmp.path()
        }

        /// Seed the isolated flow stream with one per-day JSONL file (#994).
        fn write_flow_day(&self, name: &str, lines: &[String]) {
            let dir = self.tmp.path().join("flows");
            fs::create_dir_all(&dir).unwrap();
            fs::write(dir.join(name), lines.join("\n")).unwrap();
        }
    }

    impl Drop for CrewDirGuard {
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
            }
        }
    }

    /// Build one flow-stream line via the SAME constructor the runtime capture
    /// path uses (`build_telemetry_record`), so the test fixture format can't
    /// drift from what `derive_cautions` parses.
    fn detector_line(source: &str, payload: serde_json::Value) -> String {
        let rec = crate::dispatch::build_telemetry_record(
            darkmux_flow::Level::Info,
            "telemetry.detector",
            source,
            "coder",
            "sess-1",
            Some("test-model"),
            Some("m1"),
            Some("s1"),
            payload,
        );
        serde_json::to_string(&rec).unwrap()
    }

    #[serial_test::serial]
    #[test]
    fn rebuild_derives_cautions_from_flow_stream() {
        let crew = CrewDirGuard::new();
        crew.write_flow_day(
            "2026-06-22.jsonl",
            &[
                // The real schema-header line each flow file opens with
                // (`_type:"schema"`, per `integrity::schema_header_line`) — it
                // must be skipped, not error the rebuild (it doesn't
                // deserialize as a FlowRecord).
                r#"{"_type":"schema","version":"1.13.0","darkmux_version":"1.7.0"}"#.to_string(),
                // A file-keyed cycle firing → caution with file=src/x.rs.
                detector_line(
                    "detector",
                    serde_json::json!({
                        "kind": "cycle", "severity": "warn", "detail": "`edit` called 3×",
                        "area": { "files": ["src/x.rs"] }
                    }),
                ),
                // An engagement-level firing (no area) → caution with NULL file.
                detector_line(
                    "detector",
                    serde_json::json!({
                        "kind": "reasoning-loop", "severity": "warn", "detail": "same reasoning 3×"
                    }),
                ),
                // A non-detector telemetry record (source=runtime) → ignored.
                detector_line(
                    "runtime",
                    serde_json::json!({ "kind": "context", "detail": "context fill 40%" }),
                ),
            ],
        );

        let idx = index_path(crew.path());
        rebuild_at(&idx).unwrap();

        let conn = open_index(&idx).unwrap();
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM cautions", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 2, "only the two detector firings become cautions");

        let (file, code_hash, mission, role): (
            Option<String>,
            Option<String>,
            Option<String>,
            Option<String>,
        ) = conn
            .query_row(
                "SELECT file, code_hash, mission_id, role FROM cautions WHERE kind='cycle'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .unwrap();
        assert_eq!(file.as_deref(), Some("src/x.rs"));
        assert_eq!(code_hash, None, "code_hash is NULL until runtime slice 2");
        assert_eq!(mission.as_deref(), Some("m1"), "mission threaded through");
        assert_eq!(role.as_deref(), Some("coder"), "handle stored as role");

        let engagement_level: Option<String> = conn
            .query_row(
                "SELECT file FROM cautions WHERE kind='reasoning-loop'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(engagement_level, None, "engagement-level firing → NULL file");
    }

    // (#999) The former `knowledge_preserved_but_cautions_rederived_across_rebuild`
    // test was dropped with the vestigial index.db `knowledge` table: its
    // preserved-across-rebuild property is covered by
    // `rebuild_preserves_non_derived_runtime_state`, and its caution-rederive
    // property by `rebuild_derives_cautions_from_flow_stream`.

    #[test]
    fn caution_fields_extracts_area_and_defaults() {
        let with_area = crate::dispatch::build_telemetry_record(
            darkmux_flow::Level::Info,
            "telemetry.detector",
            "detector",
            "coder",
            "s",
            None,
            None,
            None,
            serde_json::json!({
                "kind": "cycle", "severity": "warn", "detail": "d",
                "area": { "files": ["f.rs"], "code_hash": "abc" }
            }),
        );
        let (kind, sev, detail, file, code_hash) = caution_fields(&with_area);
        assert_eq!((kind.as_str(), sev.as_str(), detail.as_str()), ("cycle", "warn", "d"));
        assert_eq!(file.as_deref(), Some("f.rs"));
        assert_eq!(code_hash.as_deref(), Some("abc"));

        let no_area = crate::dispatch::build_telemetry_record(
            darkmux_flow::Level::Info,
            "telemetry.detector",
            "detector",
            "coder",
            "s",
            None,
            None,
            None,
            serde_json::json!({ "kind": "reasoning-loop", "severity": "warn", "detail": "d2" }),
        );
        let (_, _, _, file2, hash2) = caution_fields(&no_area);
        assert_eq!(file2, None);
        assert_eq!(hash2, None);

        // Malformed payload (no kind/severity/detail) → defaults, never panics.
        let malformed = crate::dispatch::build_telemetry_record(
            darkmux_flow::Level::Info,
            "telemetry.detector",
            "detector",
            "coder",
            "s",
            None,
            None,
            None,
            serde_json::json!({ "unexpected": true }),
        );
        let (k3, s3, d3, _, _) = caution_fields(&malformed);
        assert_eq!((k3.as_str(), s3.as_str(), d3.as_str()), ("unknown", "warn", ""));
    }

    #[test]
    fn is_detector_caution_keys_on_category_and_source() {
        let detector = crate::dispatch::build_telemetry_record(
            darkmux_flow::Level::Info,
            "telemetry.detector",
            "detector",
            "coder",
            "s",
            None,
            None,
            None,
            serde_json::json!({}),
        );
        assert!(is_detector_caution(&detector));

        let runtime = crate::dispatch::build_telemetry_record(
            darkmux_flow::Level::Info,
            "telemetry.runtime",
            "runtime",
            "coder",
            "s",
            None,
            None,
            None,
            serde_json::json!({}),
        );
        assert!(!is_detector_caution(&runtime), "non-detector telemetry is not a caution");

        // A non-telemetry record, even with source=detector, is not a caution
        // (the category gate). Deserialized from the minimal required fields.
        let work: darkmux_flow::FlowRecord = serde_json::from_str(
            r#"{"ts":"2026-06-22T00:00:00Z","level":"info","category":"work","tier":"local","stage":"dispatch","action":"x","handle":"coder","source":"detector"}"#,
        )
        .unwrap();
        assert!(!is_detector_caution(&work), "non-telemetry category is not a caution");
    }

    fn write_role(
        crew_root: &Path,
        id: &str,
        description: &str,
        skills: &[&str],
        escalation: &str,
        handoff_to: Option<&str>,
    ) {
        let roles_dir = crew_root.join("roles");
        fs::create_dir_all(&roles_dir).unwrap();
        let skills_json: String = skills
            .iter()
            .map(|c| format!("\"{c}\""))
            .collect::<Vec<_>>()
            .join(",");
        let escalation_json = match (escalation, handoff_to) {
            ("hand-off-to", Some(target)) => format!("{{\"hand-off-to\":\"{target}\"}}"),
            (tag, _) => format!("\"{tag}\""),
        };
        let json = format!(
            r#"{{
              "id": "{id}",
              "description": "{description}",
              "skills": [{skills_json}],
              "tool_palette": {{"allow": ["read"], "deny": []}},
              "escalation_contract": {escalation_json}
            }}"#
        );
        fs::write(roles_dir.join(format!("{id}.json")), json).unwrap();
    }

    fn write_skill(
        crew_root: &Path,
        id: &str,
        description: &str,
        keywords: &[(&str, f32)],
    ) {
        let skills_dir = crew_root.join("skills");
        fs::create_dir_all(&skills_dir).unwrap();
        let kws: String = keywords
            .iter()
            .map(|(k, w)| format!("{{\"keyword\":\"{k}\",\"weight\":{w}}}"))
            .collect::<Vec<_>>()
            .join(",");
        let json = format!(
            r#"{{
              "id": "{id}",
              "description": "{description}",
              "keywords": [{kws}]
            }}"#
        );
        fs::write(skills_dir.join(format!("{id}.json")), json).unwrap();
    }

    fn write_mission(crew_root: &Path, id: &str, description: &str) {
        // Per-mission layout (#148): <crew_root>/missions/<id>/mission.json
        let dir = crew_root.join("missions").join(id);
        fs::create_dir_all(&dir).unwrap();
        let json = format!(
            r#"{{
              "id": "{id}",
              "description": "{description}",
              "status": "active",
              "sprint_ids": [],
              "created_ts": 1700000000
            }}"#
        );
        fs::write(dir.join("mission.json"), json).unwrap();
    }

    fn write_sprint(crew_root: &Path, id: &str, mission_id: &str, description: &str) {
        // Per-mission layout (#148): <crew_root>/missions/<mission_id>/sprints/<id>.json
        let dir = crew_root.join("missions").join(mission_id).join("sprints");
        fs::create_dir_all(&dir).unwrap();
        let json = format!(
            r#"{{
              "id": "{id}",
              "mission_id": "{mission_id}",
              "description": "{description}",
              "status": "planned",
              "depends_on": [],
              "created_ts": 1700000000
            }}"#
        );
        fs::write(dir.join(format!("{id}.json")), json).unwrap();
    }

    fn index_path(root: &Path) -> PathBuf {
        root.join("index.db")
    }

    #[serial_test::serial]
    #[test]
    fn schema_applies_cleanly() {
        let guard = CrewDirGuard::new();
        let idx = index_path(guard.path());
        let conn = open_index(&idx).unwrap();
        init_schema(&conn).unwrap();
        // Apply twice — IF NOT EXISTS makes this safe.
        init_schema(&conn).unwrap();
        let v: i32 = conn
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(v, SCHEMA_VERSION);
    }

    #[serial_test::serial]
    #[test]
    fn rebuild_round_trips_custom_role_with_handoff() {
        let guard = CrewDirGuard::new();
        // The builtin "coder" role exists; we add a "supervisor" that
        // hands off to it.
        write_role(
            guard.path(),
            "supervisor",
            "Routes work to others.",
            &[],
            "hand-off-to",
            Some("coder"),
        );

        let idx = index_path(guard.path());
        rebuild_at(&idx).unwrap();

        let conn = open_index(&idx).unwrap();
        let (desc, tag): (String, String) = conn
            .query_row(
                "SELECT description, escalation_contract_tag FROM roles WHERE id = 'supervisor'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(desc, "Routes work to others.");
        assert_eq!(tag, "hand-off-to");

        let target: String = conn
            .query_row(
                "SELECT target_role_id FROM role_escalation_targets WHERE role_id = 'supervisor'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(target, "coder");
    }

    #[serial_test::serial]
    #[test]
    fn rebuild_is_idempotent() {
        let _guard = CrewDirGuard::new();
        let idx = index_path(_guard.path());

        rebuild_at(&idx).unwrap();
        let counts_first: Vec<(String, i64)> = {
            let conn = open_index(&idx).unwrap();
            let mut s = conn
                .prepare("SELECT name, (SELECT COUNT(*) FROM roles) FROM sqlite_master WHERE type='table' AND name='roles'")
                .unwrap();
            s.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))
                .unwrap()
                .collect::<rusqlite::Result<_>>()
                .unwrap()
        };

        rebuild_at(&idx).unwrap();
        let counts_second: Vec<(String, i64)> = {
            let conn = open_index(&idx).unwrap();
            let mut s = conn
                .prepare("SELECT name, (SELECT COUNT(*) FROM roles) FROM sqlite_master WHERE type='table' AND name='roles'")
                .unwrap();
            s.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))
                .unwrap()
                .collect::<rusqlite::Result<_>>()
                .unwrap()
        };

        assert_eq!(
            counts_first, counts_second,
            "rebuilding twice should produce identical row counts"
        );
    }

    #[serial_test::serial]
    #[test]
    fn fts5_ranks_known_keyword() {
        let guard = CrewDirGuard::new();
        write_skill(
            guard.path(),
            "widget-engineering",
            "Designs widgets",
            &[("widget", 0.9), ("gadget", 0.4)],
        );
        let idx = index_path(guard.path());
        rebuild_at(&idx).unwrap();

        let conn = open_index(&idx).unwrap();
        let hit: String = conn
            .query_row(
                "SELECT skill_id FROM skill_keywords_fts WHERE keyword MATCH 'widget' LIMIT 1",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(hit, "widget-engineering");
    }

    #[serial_test::serial]
    #[test]
    fn composite_sprint_pk_allows_same_id_across_missions() {
        let guard = CrewDirGuard::new();
        write_mission(guard.path(), "alpha", "First mission");
        write_mission(guard.path(), "beta", "Second mission");
        write_sprint(guard.path(), "kickoff", "alpha", "Kickoff for alpha");
        write_sprint(guard.path(), "kickoff", "beta", "Kickoff for beta");

        let idx = index_path(guard.path());
        rebuild_at(&idx).unwrap();

        let conn = open_index(&idx).unwrap();
        let n: i64 = conn
            .query_row("SELECT COUNT(*) FROM sprints WHERE id = 'kickoff'", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 2, "same sprint id should coexist under different missions");
    }

    #[serial_test::serial]
    #[test]
    fn status_errors_when_index_missing() {
        let _guard = CrewDirGuard::new();
        let idx = index_path(_guard.path());
        let err = status_at(&idx).unwrap_err();
        assert!(err.to_string().contains("no index at"));
    }

    #[serial_test::serial]
    #[test]
    fn status_reports_clean_after_rebuild() {
        let guard = CrewDirGuard::new();
        write_role(guard.path(), "alpha", "a role", &[], "bail-with-explanation", None);
        let idx = index_path(guard.path());
        rebuild_at(&idx).unwrap();

        let report = status_at(&idx).unwrap();
        assert!(report.last_rebuild_ts.is_some());
        assert_eq!(report.stored_schema_version, Some(SCHEMA_VERSION));
        assert!(report.added.is_empty(), "no additions expected, got {:?}", report.added);
        assert!(report.modified.is_empty(), "no modifications expected, got {:?}", report.modified);
        assert!(report.deleted.is_empty(), "no deletions expected, got {:?}", report.deleted);
    }

    #[serial_test::serial]
    #[test]
    fn index_picks_up_legacy_layout_roles() {
        // Regression for the Beat-33 dual-read miss: index.rs previously
        // had its own private crew_root() that bypassed the loader's
        // dual-read helpers. An operator on the legacy <root>/crew/roles/
        // layout would `darkmux crew index rebuild` and silently record
        // zero source_files — then status would report every role as
        // `deleted` against the empty snapshot.
        let guard = CrewDirGuard::new();
        // Seed a role at the LEGACY path (<root>/crew/roles/) instead of
        // the canonical (<root>/roles/) — emulates an operator who hasn't
        // run PR-3b's mv script yet.
        let legacy_roles = guard.path().join("crew").join("roles");
        std::fs::create_dir_all(&legacy_roles).unwrap();
        std::fs::write(
            legacy_roles.join("alpha.json"),
            r#"{"id":"alpha","description":"legacy-layout role","skills":[],"tool_palette":{"allow":["read"],"deny":[]},"escalation_contract":"bail-with-explanation"}"#,
        )
        .unwrap();

        let idx = index_path(guard.path());
        rebuild_at(&idx).unwrap();

        // Drift detection should see the legacy-layout file. If the dual-
        // read miss recurs, the file is invisible to the index and the
        // status report will be "clean" against an empty snapshot — a
        // silent data-integrity failure for legacy-layout operators.
        let conn = open_index(&idx).unwrap();
        let mut stmt = conn
            .prepare("SELECT path FROM source_files WHERE kind = 'role'")
            .unwrap();
        let rows: Vec<String> = stmt
            .query_map([], |r| r.get::<_, String>(0))
            .unwrap()
            .filter_map(|r| r.ok())
            .collect();
        assert_eq!(rows.len(), 1, "legacy-layout role should index — got rows={:?}", rows);
        assert!(
            rows[0].contains("/crew/roles/alpha.json"),
            "indexed path should be the legacy location, got: {}",
            rows[0]
        );
    }

    #[serial_test::serial]
    #[test]
    fn status_detects_added_file() {
        let guard = CrewDirGuard::new();
        let idx = index_path(guard.path());
        rebuild_at(&idx).unwrap();

        // Add a new user-side role AFTER rebuild.
        write_role(guard.path(), "newcomer", "new role", &[], "bail-with-explanation", None);

        let report = status_at(&idx).unwrap();
        assert_eq!(report.added.len(), 1, "expected one addition, got {:?}", report.added);
        assert_eq!(report.added[0].0, "role");
    }

    #[serial_test::serial]
    #[test]
    fn status_detects_deleted_file() {
        let guard = CrewDirGuard::new();
        write_role(guard.path(), "doomed", "soon-deleted", &[], "bail-with-explanation", None);
        let idx = index_path(guard.path());
        rebuild_at(&idx).unwrap();

        fs::remove_file(guard.path().join("roles").join("doomed.json")).unwrap();

        let report = status_at(&idx).unwrap();
        assert_eq!(report.deleted.len(), 1, "expected one deletion, got {:?}", report.deleted);
        assert_eq!(report.deleted[0].0, "role");
    }

    #[serial_test::serial]
    #[test]
    fn status_detects_modified_file() {
        let guard = CrewDirGuard::new();
        write_role(guard.path(), "mutable", "v1", &[], "bail-with-explanation", None);
        let idx = index_path(guard.path());
        rebuild_at(&idx).unwrap();

        // Rewrite with different content + force a newer mtime.
        let path = guard.path().join("roles").join("mutable.json");
        // sleep briefly so mtime is guaranteed distinct on coarse FS clocks
        std::thread::sleep(std::time::Duration::from_millis(1100));
        write_role(guard.path(), "mutable", "v2-different-content", &[], "bail-with-explanation", None);

        // Sanity: the file's mtime is actually newer than the recorded one.
        let new_mt = file_mtime(&path).unwrap();
        let recorded_mt: i64 = {
            let conn = open_index(&idx).unwrap();
            conn.query_row(
                "SELECT mtime FROM source_files WHERE path = ?1",
                params![path.to_string_lossy()],
                |r| r.get(0),
            )
            .unwrap()
        };
        assert!(new_mt > recorded_mt, "test setup didn't advance mtime: new={new_mt} recorded={recorded_mt}");

        let report = status_at(&idx).unwrap();
        assert_eq!(report.modified.len(), 1, "expected one modification, got {:?}", report.modified);
        assert_eq!(report.modified[0].0, "role");
    }

    /// (#448 refactor 0) Verifies the v1 → v2 schema migration: a DB
    /// seeded with the legacy `capabilities` / `role_capabilities` /
    /// `capability_keywords` / `capability_keywords_fts` tables +
    /// triggers gets cleanly migrated to v2 when `init_schema` runs.
    /// Asserts: legacy artifacts gone, new `skills`-named artifacts
    /// present, `PRAGMA user_version = 2`. Then re-opens to confirm
    /// idempotency (second init_schema on a v2 DB is a no-op).
    #[serial_test::serial]
    #[test]
    fn migration_v1_to_v2_drops_legacy_capability_artifacts() {
        let tmp = TempDir::new().unwrap();
        let idx_path = tmp.path().join("v1.db");

        // Seed a v1-shaped DB with the legacy artifacts populated.
        {
            let conn = Connection::open(&idx_path).unwrap();
            conn.execute_batch(
                "
                PRAGMA user_version = 1;
                CREATE TABLE capabilities (id TEXT PRIMARY KEY, description TEXT NOT NULL);
                CREATE TABLE roles (id TEXT PRIMARY KEY, description TEXT NOT NULL);
                CREATE TABLE role_capabilities (
                    role_id TEXT NOT NULL REFERENCES roles(id) ON DELETE CASCADE,
                    capability_id TEXT NOT NULL REFERENCES capabilities(id) ON DELETE CASCADE,
                    PRIMARY KEY (role_id, capability_id)
                );
                CREATE INDEX idx_role_capabilities_cap ON role_capabilities(capability_id);
                CREATE TABLE capability_keywords (
                    capability_id TEXT NOT NULL REFERENCES capabilities(id) ON DELETE CASCADE,
                    keyword TEXT NOT NULL,
                    weight REAL NOT NULL,
                    PRIMARY KEY (capability_id, keyword)
                );
                CREATE VIRTUAL TABLE capability_keywords_fts USING fts5(
                    keyword, capability_id UNINDEXED, weight UNINDEXED
                );
                CREATE TRIGGER capability_keywords_ai
                AFTER INSERT ON capability_keywords
                BEGIN
                    INSERT INTO capability_keywords_fts(keyword, capability_id, weight)
                    VALUES (NEW.keyword, NEW.capability_id, NEW.weight);
                END;
                INSERT INTO capabilities (id, description) VALUES ('coding','seed');
                INSERT INTO roles (id, description) VALUES ('coder','seed');
                INSERT INTO role_capabilities (role_id, capability_id) VALUES ('coder','coding');
                INSERT INTO capability_keywords (capability_id, keyword, weight) VALUES ('coding','seed-kw',0.5);
                ",
            )
            .unwrap();
        }

        // First open: triggers the v1 → v2 migration.
        let conn = Connection::open(&idx_path).unwrap();
        conn.execute_batch("PRAGMA foreign_keys = ON;").unwrap();
        init_schema(&conn).unwrap();

        let table_exists = |name: &str| -> bool {
            conn.query_row(
                "SELECT name FROM sqlite_master WHERE type IN ('table','view') AND name = ?1",
                params![name],
                |_| Ok(()),
            )
            .optional()
            .unwrap()
            .is_some()
        };

        // Legacy artifacts dropped.
        assert!(!table_exists("capabilities"), "legacy `capabilities` table must be dropped");
        assert!(!table_exists("role_capabilities"), "legacy `role_capabilities` must be dropped");
        assert!(!table_exists("capability_keywords"), "legacy `capability_keywords` must be dropped");
        assert!(!table_exists("capability_keywords_fts"), "legacy FTS virtual must be dropped");

        // New schema applied.
        assert!(table_exists("skills"), "new `skills` table must exist");
        assert!(table_exists("role_skills"), "new `role_skills` table must exist");
        assert!(table_exists("skill_keywords"), "new `skill_keywords` table must exist");
        assert!(table_exists("skill_keywords_fts"), "new `skill_keywords_fts` must exist");

        // Version bumped.
        let v: i32 = conn
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(v, SCHEMA_VERSION);

        drop(conn);

        // Second open: idempotency check. The migration block must not
        // re-run (DROP IF EXISTS is technically idempotent, but on a v2
        // DB `current_version < 2` is false, so the block is skipped
        // entirely). init_schema must complete cleanly.
        let conn2 = Connection::open(&idx_path).unwrap();
        conn2.execute_batch("PRAGMA foreign_keys = ON;").unwrap();
        init_schema(&conn2).unwrap();
        let v2: i32 = conn2
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(v2, SCHEMA_VERSION);
        let skills_exists: bool = conn2
            .query_row(
                "SELECT name FROM sqlite_master WHERE type IN ('table','view') AND name = 'skills'",
                [],
                |_| Ok(()),
            )
            .optional()
            .unwrap()
            .is_some();
        assert!(skills_exists);
    }

    /// (#914) A pre-#95 index whose `missions` table predates the
    /// `started_ts`/`closed_ts`/`paused_ts` columns. Pre-fix, the
    /// `CREATE TABLE IF NOT EXISTS` in SCHEMA_SQL skipped the existing table
    /// (so a `populate` INSERT with `started_ts` crashed, or rolled back to
    /// stale data). The self-healing drop+recreate in `init_schema` must
    /// rebuild the table with the current columns.
    #[serial_test::serial]
    #[test]
    fn rebuild_heals_stale_table_missing_a_column() {
        let tmp = TempDir::new().unwrap();
        let idx = tmp.path().join("stale.db");
        {
            let conn = Connection::open(&idx).unwrap();
            conn.execute_batch(
                "PRAGMA user_version = 2;
                 CREATE TABLE missions (
                     id          TEXT PRIMARY KEY,
                     description TEXT NOT NULL,
                     status      TEXT NOT NULL,
                     created_ts  INTEGER NOT NULL
                 );
                 INSERT INTO missions (id, description, status, created_ts)
                   VALUES ('legacy', 'seed', 'active', 0);",
            )
            .unwrap();
        }

        // Pre-fix this errored with `table missions has no column named
        // started_ts`; post-fix the table is dropped + recreated fresh.
        rebuild_at(&idx).unwrap();

        let conn = open_index(&idx).unwrap();
        let has_started_ts: Option<()> = conn
            .query_row(
                "SELECT 1 FROM pragma_table_info('missions') WHERE name = 'started_ts'",
                [],
                |_| Ok(()),
            )
            .optional()
            .unwrap();
        assert!(
            has_started_ts.is_some(),
            "rebuild must heal the stale missions table to include the #95 columns"
        );
    }

    /// (#999) A pre-#999 index (version 4) carrying the scaffolded `knowledge`
    /// table must, on rebuild, drop that table (authored lessons live in
    /// `lessons.db` now) and advance to version 5. The `< 5` migration block in
    /// `init_schema` does this; assert the table is gone afterward.
    #[serial_test::serial]
    #[test]
    fn rebuild_drops_vestigial_knowledge_table() {
        let tmp = TempDir::new().unwrap();
        let idx = tmp.path().join("vestigial.db");
        {
            let conn = Connection::open(&idx).unwrap();
            conn.execute_batch(
                "PRAGMA user_version = 4;
                 CREATE TABLE knowledge (
                     id          INTEGER PRIMARY KEY AUTOINCREMENT,
                     file        TEXT,
                     title       TEXT NOT NULL,
                     body        TEXT NOT NULL,
                     source      TEXT,
                     created_ts  INTEGER NOT NULL,
                     updated_ts  INTEGER NOT NULL
                 );
                 INSERT INTO knowledge (title, body, created_ts, updated_ts)
                   VALUES ('legacy', 'seed', 0, 0);",
            )
            .unwrap();
        }

        rebuild_at(&idx).unwrap();

        let conn = open_index(&idx).unwrap();
        let knowledge_exists: Option<()> = conn
            .query_row(
                "SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'knowledge'",
                [],
                |_| Ok(()),
            )
            .optional()
            .unwrap();
        assert!(
            knowledge_exists.is_none(),
            "the vestigial index.db knowledge table must be dropped on migration (#999)"
        );
        let version: i32 = conn
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(version, SCHEMA_VERSION, "user_version advanced to current");
    }

    /// (#914, CONSIDER-1) A DB whose structural `user_version` is current but
    /// whose `populate` never committed (no `meta_kv.schema_version` row) —
    /// the state a rebuild leaves behind when `init_schema` succeeds but
    /// `populate` rolls back. `ensure_fresh_index` must treat it as stale and
    /// rebuild, NOT trust the header version and serve empty derived tables.
    #[serial_test::serial]
    #[test]
    fn ensure_fresh_rebuilds_when_populate_signal_absent() {
        let tmp = TempDir::new().unwrap();
        let idx = tmp.path().join("half.db");
        {
            // Header stamped current; no committed populate → no meta_kv row.
            let conn = Connection::open(&idx).unwrap();
            conn.execute_batch(&format!("PRAGMA user_version = {SCHEMA_VERSION};"))
                .unwrap();
        }

        ensure_fresh_index(&idx).unwrap();

        // Post-condition: populate committed → meta_kv records the version and
        // the derived tables are filled from the builtin manifests.
        let conn = open_index(&idx).unwrap();
        let recorded: String = conn
            .query_row(
                "SELECT value FROM meta_kv WHERE key = 'schema_version'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(recorded, SCHEMA_VERSION.to_string());
        let role_count: i64 = conn
            .query_row("SELECT COUNT(*) FROM roles", [], |r| r.get(0))
            .unwrap();
        assert!(
            role_count > 0,
            "ensure_fresh_index must rebuild from builtin manifests, not trust the stale header"
        );
    }

    fn write_mission_with_started_ts(crew_root: &std::path::Path, id: &str) {
        let dir = crew_root.join("missions").join(id);
        std::fs::create_dir_all(&dir).unwrap();
        let mission = serde_json::json!({
            "id": id,
            "description": "mission carrying a started_ts",
            "sprint_ids": [],
            "created_ts": 1_700_000_000u64,
            "started_ts": 1_700_000_100u64,
        });
        std::fs::write(
            dir.join("mission.json"),
            serde_json::to_string_pretty(&mission).unwrap(),
        )
        .unwrap();
    }

    fn column_names(conn: &Connection, table: &str) -> Vec<String> {
        let mut stmt = conn
            .prepare(&format!(
                "SELECT name FROM pragma_table_info('{table}') ORDER BY name"
            ))
            .unwrap();
        stmt.query_map([], |r| r.get::<_, String>(0))
            .unwrap()
            .collect::<rusqlite::Result<Vec<String>>>()
            .unwrap()
    }

    /// (#914, CONSIDER-2 — recurrence guard) Pins the column set of every
    /// rebuild-managed table to the schema version. The #914 bug was a column
    /// added to the DDL (#95 timestamps) without a `SCHEMA_VERSION` bump,
    /// invisible until a rebuild crashed. If you change a derived table's
    /// columns, update this snapshot AND bump `SCHEMA_VERSION` (and weigh the
    /// heal/migration path). A pure "remember to bump" comment is the same
    /// discipline-as-willpower that already failed once here.
    #[serial_test::serial]
    #[test]
    fn derived_table_columns_match_versioned_snapshot() {
        let tmp = TempDir::new().unwrap();
        let idx = tmp.path().join("snap.db");
        rebuild_at(&idx).unwrap();
        let conn = open_index(&idx).unwrap();

        let expected: &[(&str, &[&str])] = &[
            ("source_files", &["content_hash", "kind", "mtime", "path"]),
            (
                "roles",
                &[
                    "description",
                    "escalation_contract_tag",
                    "id",
                    "prompt_path",
                    "tool_palette_json",
                ],
            ),
            ("role_escalation_targets", &["role_id", "target_role_id"]),
            ("skills", &["description", "id"]),
            ("role_skills", &["role_id", "skill_id"]),
            ("skill_keywords", &["keyword", "skill_id", "weight"]),
            ("crews", &["description", "id"]),
            ("crew_members", &["crew_id", "position", "role_id"]),
            (
                "missions",
                &[
                    "closed_ts",
                    "created_ts",
                    "description",
                    "id",
                    "paused_ts",
                    "started_ts",
                    "status",
                ],
            ),
            (
                "sprints",
                &[
                    "abandoned_ts",
                    "completed_ts",
                    "created_ts",
                    "depends_on_json",
                    "description",
                    "id",
                    "mission_id",
                    "started_ts",
                    "status",
                ],
            ),
            (
                "cautions",
                &[
                    "code_hash",
                    "detail",
                    "file",
                    "id",
                    "kind",
                    "mission_id",
                    "model",
                    "role",
                    "session_id",
                    "severity",
                    "sprint_id",
                    "ts",
                ],
            ),
        ];

        for (table, cols) in expected {
            let actual = column_names(&conn, table);
            let want: Vec<String> = cols.iter().map(|s| s.to_string()).collect();
            assert_eq!(
                actual, want,
                "column-set drift in `{table}` — update this snapshot AND bump SCHEMA_VERSION (#914 CONSIDER-2)"
            );
        }

        // (#994) Guard the guard: the snapshot must cover EXACTLY the derived
        // tables (`REBUILD_TABLES`), so adding a rebuild table without a
        // snapshot row fails HERE rather than silently passing. That per-table-
        // only gap is how `cautions` slipped through this guard once — the very
        // drift the #914 CONSIDER-2 guard exists to catch.
        let snapshot_tables: std::collections::BTreeSet<&str> =
            expected.iter().map(|(t, _)| *t).collect();
        let rebuild_tables: std::collections::BTreeSet<&str> =
            REBUILD_TABLES.iter().copied().collect();
        assert_eq!(
            snapshot_tables, rebuild_tables,
            "snapshot must list EXACTLY the REBUILD_TABLES — add the new table's \
             column row above AND bump SCHEMA_VERSION (#914 CONSIDER-2 / #994)"
        );
    }

    /// (#914) The literal crash path: a pre-#95 `missions` table plus a
    /// mission manifest carrying `started_ts`. Pre-fix, `populate`'s INSERT
    /// errored with `table missions has no column named started_ts`; post-fix
    /// the table is dropped + recreated, so the INSERT lands.
    #[serial_test::serial]
    #[test]
    fn rebuild_heals_stale_table_then_inserts_mission_timestamps() {
        let guard = CrewDirGuard::new();
        write_mission_with_started_ts(guard.path(), "m1");
        let idx = guard.path().join("index.db");
        {
            let conn = Connection::open(&idx).unwrap();
            conn.execute_batch(
                "PRAGMA user_version = 2;
                 CREATE TABLE missions (
                     id          TEXT PRIMARY KEY,
                     description TEXT NOT NULL,
                     status      TEXT NOT NULL,
                     created_ts  INTEGER NOT NULL
                 );",
            )
            .unwrap();
        }

        rebuild_at(&idx).unwrap();

        let conn = open_index(&idx).unwrap();
        let started: Option<i64> = conn
            .query_row(
                "SELECT started_ts FROM missions WHERE id = 'm1'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(started, Some(1_700_000_100));
    }

    /// (#914) Non-derived runtime tables (allocator + meta) are NOT in
    /// `REBUILD_TABLES` and must survive a rebuild that wipes + refills the
    /// derived tables. Guards against a future change folding them in.
    #[serial_test::serial]
    #[test]
    fn rebuild_preserves_non_derived_runtime_state() {
        let tmp = TempDir::new().unwrap();
        let idx = tmp.path().join("preserve.db");
        rebuild_at(&idx).unwrap();
        {
            let conn = open_index(&idx).unwrap();
            conn.execute(
                "INSERT INTO unmatched_terms (term, count, last_seen) VALUES ('keepme', 7, 123)",
                [],
            )
            .unwrap();
        }

        // A second rebuild drops + recreates the derived tables.
        rebuild_at(&idx).unwrap();

        let conn = open_index(&idx).unwrap();
        let surviving: i64 = conn
            .query_row(
                "SELECT count FROM unmatched_terms WHERE term = 'keepme'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(surviving, 7, "non-derived runtime state must survive a rebuild");
        let role_count: i64 = conn
            .query_row("SELECT COUNT(*) FROM roles", [], |r| r.get(0))
            .unwrap();
        assert!(role_count > 0, "derived tables should be refilled from manifests");
    }

    /// (#914) `skill_keywords` is dropped + recreated each rebuild but the FTS
    /// virtual mirror `skill_keywords_fts` is not — `populate` clears + refills
    /// it. After repeated rebuilds the mirror must hold exactly one row per
    /// `skill_keywords` row (no stale survivors, no duplicates).
    #[serial_test::serial]
    #[test]
    fn fts_mirror_stays_consistent_across_repeated_rebuilds() {
        let tmp = TempDir::new().unwrap();
        let idx = tmp.path().join("fts.db");
        rebuild_at(&idx).unwrap();
        rebuild_at(&idx).unwrap();
        let conn = open_index(&idx).unwrap();
        let kw: i64 = conn
            .query_row("SELECT COUNT(*) FROM skill_keywords", [], |r| r.get(0))
            .unwrap();
        let fts: i64 = conn
            .query_row("SELECT COUNT(*) FROM skill_keywords_fts", [], |r| r.get(0))
            .unwrap();
        assert_eq!(kw, fts, "FTS mirror must match skill_keywords 1:1 after repeated rebuilds");
        assert!(kw > 0, "builtin skills declare keywords — otherwise this guard is vacuous");
    }

    /// (#891) Drift detection must flag a content edit even when mtime did not
    /// advance. We simulate "content changed, mtime unchanged" by staling only
    /// the recorded `content_hash` (the recorded mtime still matches disk),
    /// then assert `status_at` reports the role as modified.
    #[serial_test::serial]
    #[test]
    fn drift_detects_content_edit_without_mtime_change() {
        let guard = CrewDirGuard::new();
        write_role(
            guard.path(),
            "drifter",
            "v1 description",
            &[],
            "bail-with-explanation",
            None,
        );
        let idx = guard.path().join("index.db");
        rebuild_at(&idx).unwrap();

        {
            let conn = open_index(&idx).unwrap();
            conn.execute(
                "UPDATE source_files SET content_hash = 'stale-hash' WHERE kind = 'role'",
                [],
            )
            .unwrap();
        }

        let report = status_at(&idx).unwrap();
        assert!(
            report.modified.iter().any(|(kind, _)| kind == "role"),
            "status must flag the role modified when the content hash differs, regardless of mtime"
        );
    }
}
