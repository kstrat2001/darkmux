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
//!   `kind IN ('role', 'capability', 'crew', 'mission', 'sprint')`.
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
//!   ticket text that didn't match any capability keyword).
//! - **FTS5 sync triggers** — `capability_keywords_ai` / `_ad` / `_au`
//!   propagate INSERT / DELETE / UPDATE on `capability_keywords` to the
//!   `capability_keywords_fts` mirror automatically.
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
//! - FTS5 keyword search produces ranked capabilities for a known fixture
//! - Index rebuild is idempotent (running twice produces the same state)
//! - `darkmux crew index status` flags drift (file mtime + content_hash)
//!
//! NOT covered here: CRUD CLI for each entity, audit-log / outcomes /
//! allocator population. Those land in follow-up PRs.

#![allow(dead_code)]

use crate::crew::loader;
use crate::crew::types::*;
use crate::lab::paths::{resolve, ResolveScope};
use anyhow::{Context, Result};
use rusqlite::{params, Connection, OptionalExtension};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

const SCHEMA_VERSION: i32 = 1;

const SCHEMA_SQL: &str = r#"
CREATE TABLE IF NOT EXISTS source_files (
    path          TEXT PRIMARY KEY,
    kind          TEXT NOT NULL CHECK (kind IN ('role','capability','crew','mission','sprint')),
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

CREATE TABLE IF NOT EXISTS capabilities (
    id          TEXT PRIMARY KEY,
    description TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS role_capabilities (
    role_id       TEXT NOT NULL REFERENCES roles(id) ON DELETE CASCADE,
    capability_id TEXT NOT NULL REFERENCES capabilities(id) ON DELETE CASCADE,
    PRIMARY KEY (role_id, capability_id)
);
CREATE INDEX IF NOT EXISTS idx_role_capabilities_cap ON role_capabilities(capability_id);

CREATE TABLE IF NOT EXISTS capability_keywords (
    capability_id TEXT NOT NULL REFERENCES capabilities(id) ON DELETE CASCADE,
    keyword       TEXT NOT NULL,
    weight        REAL NOT NULL,
    PRIMARY KEY (capability_id, keyword)
);

CREATE VIRTUAL TABLE IF NOT EXISTS capability_keywords_fts USING fts5(
    keyword,
    capability_id UNINDEXED,
    weight        UNINDEXED
);

CREATE TRIGGER IF NOT EXISTS capability_keywords_ai
AFTER INSERT ON capability_keywords
BEGIN
    INSERT INTO capability_keywords_fts(keyword, capability_id, weight)
    VALUES (NEW.keyword, NEW.capability_id, NEW.weight);
END;

CREATE TRIGGER IF NOT EXISTS capability_keywords_ad
AFTER DELETE ON capability_keywords
BEGIN
    DELETE FROM capability_keywords_fts
    WHERE keyword = OLD.keyword AND capability_id = OLD.capability_id;
END;

CREATE TRIGGER IF NOT EXISTS capability_keywords_au
AFTER UPDATE ON capability_keywords
BEGIN
    DELETE FROM capability_keywords_fts
    WHERE keyword = OLD.keyword AND capability_id = OLD.capability_id;
    INSERT INTO capability_keywords_fts(keyword, capability_id, weight)
    VALUES (NEW.keyword, NEW.capability_id, NEW.weight);
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
    created_ts  INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS sprints (
    id              TEXT NOT NULL,
    mission_id      TEXT NOT NULL REFERENCES missions(id) ON DELETE CASCADE,
    description     TEXT NOT NULL,
    status          TEXT NOT NULL CHECK (status IN ('planned','running','complete','abandoned')),
    depends_on_json TEXT NOT NULL DEFAULT '[]',
    created_ts      INTEGER NOT NULL,
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
"#;

/// Tables (and only tables) that `rebuild()` clears + repopulates from manifests.
/// Allocator tables (allocations / outcomes / unmatched_terms) and meta_kv are
/// preserved across rebuilds — those carry runtime state, not derived data.
const REBUILD_TABLES: &[&str] = &[
    "source_files",
    "role_escalation_targets",
    "role_capabilities",
    "capability_keywords",
    "crew_members",
    "sprints",
    "missions",
    "crews",
    "roles",
    "capabilities",
];

/// Default index path: `<paths.root>/index.db`. Resolved through the same
/// project-vs-user precedence as `lab::paths`. Stable across releases —
/// changing this silently invalidates every operator's existing index.
/// Tests use the `_at(&path)` variants (`rebuild_at`, `role_list_at`,
/// `crew_list_at`, etc.) rather than overriding this path.
pub(crate) fn default_index_path() -> PathBuf {
    resolve(ResolveScope::Auto).root.join("index.db")
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

pub(crate) fn open_index(path: &Path) -> Result<Connection> {
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
    conn.execute_batch(SCHEMA_SQL)
        .context("applying index schema")?;
    conn.execute_batch(&format!("PRAGMA user_version = {SCHEMA_VERSION};"))
        .context("setting user_version")?;
    Ok(())
}

/// Enumerate user-side manifest files for one entity kind. Returns
/// (path, role-or-cap-or-..-id, content) tuples. Only enumerates the
/// crew_root/<kind>s/ directory; builtins are intentionally excluded so
/// drift detection scopes to operator-owned state.
fn enumerate_user_files(crew_root: &Path, subdir: &str) -> Result<Vec<(PathBuf, String, Vec<u8>)>> {
    let dir = crew_root.join(subdir);
    if !dir.exists() {
        return Ok(Vec::new());
    }
    let mut out = Vec::new();
    for entry in fs::read_dir(&dir)
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
        let id = name.trim_end_matches(".json").to_string();
        let bytes = fs::read(&path)
            .with_context(|| format!("reading {}", path.display()))?;
        out.push((path, id, bytes));
    }
    Ok(out)
}

fn crew_root() -> PathBuf {
    std::env::var("DARKMUX_CREW_DIR")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| resolve(ResolveScope::Auto).crew)
}

/// Insert merged-effective entities into all derived tables, plus populate
/// `source_files` with whichever user-side files exist on disk.
fn populate(conn: &mut Connection) -> Result<()> {
    let roles = loader::load_roles()?;
    let capabilities = loader::load_capabilities()?;
    let crews = loader::load_crews()?;
    let missions = loader::load_missions()?;
    let sprints = loader::load_sprints()?;

    let tx = conn.transaction()?;
    tx.execute_batch("PRAGMA defer_foreign_keys = ON;")?;

    for tbl in REBUILD_TABLES {
        tx.execute(&format!("DELETE FROM {tbl};"), [])?;
    }
    // FTS5 mirror has triggers wired to capability_keywords, but the
    // DELETE-cascade only fires per-row — clearing capability_keywords
    // above already empties the FTS mirror via the AD trigger. Belt and
    // braces: ensure FTS is empty before re-INSERT.
    tx.execute("DELETE FROM capability_keywords_fts;", [])?;

    // Insert capabilities + keywords first (roles depend on capabilities via FK).
    for cap in &capabilities {
        tx.execute(
            "INSERT INTO capabilities (id, description) VALUES (?1, ?2)",
            params![cap.id, cap.description],
        )?;
        for kw in &cap.keywords {
            tx.execute(
                "INSERT INTO capability_keywords (capability_id, keyword, weight) VALUES (?1, ?2, ?3)",
                params![cap.id, kw.keyword, kw.weight as f64],
            )?;
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
        for cap_id in &role.capabilities {
            tx.execute(
                "INSERT INTO role_capabilities (role_id, capability_id) VALUES (?1, ?2)",
                params![role.id, cap_id],
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
            "INSERT INTO missions (id, description, status, created_ts) VALUES (?1, ?2, ?3, ?4)",
            params![
                mission.id,
                mission.description,
                mission_status_str(mission.status),
                mission.created_ts as i64,
            ],
        )?;
    }

    // Sprints.
    for sprint in &sprints {
        let depends_on_json = serde_json::to_string(&sprint.depends_on)?;
        tx.execute(
            "INSERT INTO sprints (id, mission_id, description, status, depends_on_json, created_ts)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                sprint.id,
                sprint.mission_id,
                sprint.description,
                sprint_status_str(sprint.status),
                depends_on_json,
                sprint.created_ts as i64,
            ],
        )?;
    }

    // source_files — scan user-side disk only (builtins are version-gated by
    // the stored darkmux_version in meta_kv, not by per-file mtime).
    let root = crew_root();
    let kinds: &[(&str, &str)] = &[
        ("role", "roles"),
        ("capability", "capabilities"),
        ("crew", "crews"),
        ("mission", "missions"),
        ("sprint", "sprints"),
    ];
    for (kind, subdir) in kinds {
        for (path, _id, bytes) in enumerate_user_files(&root, subdir)? {
            let mtime = file_mtime(&path)?;
            let hash = content_hash_hex(&bytes);
            tx.execute(
                "INSERT INTO source_files (path, kind, mtime, content_hash) VALUES (?1, ?2, ?3, ?4)",
                params![path.to_string_lossy(), *kind, mtime, hash],
            )?;
        }
    }

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

/// Internal entry point for tests + the public `rebuild()` wrapper.
pub(crate) fn rebuild_at(path: &Path) -> Result<()> {
    let mut conn = open_index(path)?;
    init_schema(&conn)?;
    populate(&mut conn)?;
    Ok(())
}

/// Rebuild the index from manifests on disk. DELETE+INSERT into all
/// derived tables in a single transaction; idempotent.
pub fn rebuild() -> Result<()> {
    let path = default_index_path();
    rebuild_at(&path)?;
    println!("crew index rebuilt at {}", path.display());
    Ok(())
}

#[derive(Debug, Default, PartialEq)]
struct StatusReport {
    last_rebuild_ts: Option<i64>,
    stored_darkmux_version: Option<String>,
    stored_schema_version: Option<i32>,
    per_kind_counts: Vec<(String, i64)>,
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

    // Drift detection: compare on-disk state to source_files.
    let root = crew_root();
    let kinds: &[(&str, &str)] = &[
        ("role", "roles"),
        ("capability", "capabilities"),
        ("crew", "crews"),
        ("mission", "missions"),
        ("sprint", "sprints"),
    ];

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
    for (kind, subdir) in kinds {
        for (path, _id, bytes) in enumerate_user_files(&root, subdir)? {
            let path_str = path.to_string_lossy().into_owned();
            seen_on_disk.insert(path_str.clone());
            match recorded_paths.get(&path_str) {
                None => {
                    report.added.push(((*kind).to_string(), path_str));
                }
                Some((_recorded_kind, recorded_mtime, recorded_hash)) => {
                    let cur_mtime = file_mtime(&path).unwrap_or(0);
                    if cur_mtime != *recorded_mtime {
                        let cur_hash = content_hash_hex(&bytes);
                        if &cur_hash != recorded_hash {
                            report.modified.push(((*kind).to_string(), path_str));
                        }
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
    struct CrewDirGuard {
        prev: Option<String>,
        tmp: TempDir,
    }

    impl CrewDirGuard {
        fn new() -> Self {
            let tmp = TempDir::new().unwrap();
            let prev = env::var("DARKMUX_CREW_DIR").ok();
            unsafe {
                env::set_var("DARKMUX_CREW_DIR", tmp.path());
            }
            Self { prev, tmp }
        }

        fn path(&self) -> &Path {
            self.tmp.path()
        }
    }

    impl Drop for CrewDirGuard {
        fn drop(&mut self) {
            unsafe {
                match &self.prev {
                    Some(v) => env::set_var("DARKMUX_CREW_DIR", v),
                    None => env::remove_var("DARKMUX_CREW_DIR"),
                }
            }
        }
    }

    fn write_role(
        crew_root: &Path,
        id: &str,
        description: &str,
        capabilities: &[&str],
        escalation: &str,
        handoff_to: Option<&str>,
    ) {
        let roles_dir = crew_root.join("roles");
        fs::create_dir_all(&roles_dir).unwrap();
        let caps_json: String = capabilities
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
              "capabilities": [{caps_json}],
              "tool_palette": {{"allow": ["read"], "deny": []}},
              "escalation_contract": {escalation_json}
            }}"#
        );
        fs::write(roles_dir.join(format!("{id}.json")), json).unwrap();
    }

    fn write_capability(
        crew_root: &Path,
        id: &str,
        description: &str,
        keywords: &[(&str, f32)],
    ) {
        let caps_dir = crew_root.join("capabilities");
        fs::create_dir_all(&caps_dir).unwrap();
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
        fs::write(caps_dir.join(format!("{id}.json")), json).unwrap();
    }

    fn write_mission(crew_root: &Path, id: &str, description: &str) {
        let dir = crew_root.join("missions");
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
        fs::write(dir.join(format!("{id}.json")), json).unwrap();
    }

    fn write_sprint(crew_root: &Path, id: &str, mission_id: &str, description: &str) {
        let dir = crew_root.join("sprints");
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
        fs::write(
            dir.join(format!("{mission_id}__{id}.json")),
            json,
        )
        .unwrap();
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
        write_capability(
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
                "SELECT capability_id FROM capability_keywords_fts WHERE keyword MATCH 'widget' LIMIT 1",
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
}
