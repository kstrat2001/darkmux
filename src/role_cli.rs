use crate::crew::index::{default_index_path, ensure_fresh_index, open_index};
use crate::cli_json;
use anyhow::{Context, Result, bail};
use darkmux_crew::types::{EscalationKind, ToolPalette};
use rusqlite::{params, OptionalExtension};
use serde::Serialize;
use std::path::Path;

/// One role in `role list --json`, with its full, untruncated description.
#[derive(Serialize, schemars::JsonSchema)]
pub struct RoleListRow {
    pub id: String,
    pub description: String,
    pub skill_count: u32,
    pub escalation: EscalationKind,
}

/// `role list --json`.
#[derive(Serialize, schemars::JsonSchema)]
pub struct RoleList {
    pub roles: Vec<RoleListRow>,
}

/// `role show --json`. `escalation_target` is `null` for a role that does not
/// hand off and for a hand-off with no recorded target (index drift).
#[derive(Serialize, schemars::JsonSchema)]
pub struct RoleShow {
    pub id: String,
    pub description: String,
    pub prompt_path: Option<String>,
    pub skills: Vec<String>,
    pub tool_palette: ToolPalette,
    pub escalation: EscalationKind,
    pub escalation_target: Option<String>,
}

/// The kind an index row's tag names. The index constrains the column to the
/// three tags, so an unknown one means the index is not darkmux's.
fn escalation_of(tag: &str) -> Result<EscalationKind> {
    EscalationKind::from_tag(tag)
        .with_context(|| format!("the role index holds an unknown escalation contract `{tag}`"))
}

/// Print a table listing every role in the index.
pub fn role_list(json: bool) -> Result<i32> {
    role_list_at(&default_index_path(), json)
}

/// Print full details for a single role.
pub fn role_show(role_id: &str, json: bool) -> Result<i32> {
    role_show_at(&default_index_path(), role_id, json)
}

/// Every role's list row, ordered by id. The description is the full text: the text table
/// truncates it, `--json` does not (#907).
fn load_role_rows(conn: &rusqlite::Connection) -> Result<Vec<RoleListRow>> {
    let mut stmt = conn.prepare(
        "SELECT r.id, r.description, \
         COALESCE(rc.skill_count, 0), \
         r.escalation_contract_tag \
         FROM roles r \
         LEFT JOIN (SELECT role_id, COUNT(*) AS skill_count FROM role_skills GROUP BY role_id) rc \
         ON r.id = rc.role_id \
         ORDER BY r.id"
    )?;

    let mut rows: Vec<RoleListRow> = Vec::new();
    let stmt_rows = stmt.query_map([], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, u32>(2)?,
            r.get::<_, String>(3)?,
        ))
    })?;
    for row in stmt_rows {
        let (id, description, skill_count, tag) = row?;
        rows.push(RoleListRow { id, description, skill_count, escalation: escalation_of(&tag)? });
    }
    Ok(rows)
}

/// The text table of `role list`. The description is truncated here only (mirrors the old SQL
/// `CASE WHEN LENGTH > 60 THEN SUBSTR(.,1,57) || '…'`).
fn print_role_table(rows: &[RoleListRow]) {
    let truncate = |d: &str| -> String {
        if d.chars().count() > 60 {
            format!("{}…", d.chars().take(57).collect::<String>())
        } else {
            d.to_string()
        }
    };

    let display: Vec<(String, String, u32, &str)> = rows
        .iter()
        .map(|r| (r.id.clone(), truncate(&r.description), r.skill_count, r.escalation.tag()))
        .collect();

    let mut id_w: usize = 2;
    let mut desc_w: usize = 12;
    let mut skill_w: usize = 11;

    for (id, desc, skills, _esc) in &display {
        id_w = id_w.max(id.len());
        desc_w = desc_w.max(desc.len());
        skill_w = skill_w.max(skills.to_string().len());
    }

    println!(
        "{:<id_w$}  {:<desc_w$}  {:<skill_w$}  escalation",
        "id", "description", "skills"
    );

    for (id, desc, skills, esc) in &display {
        println!(
            "{:<id_w$}  {:<desc_w$}  {:<skill_w$}  {}",
            id, desc, skills, esc
        );
    }
}

/// Internal entry for `role list` taking an explicit index path. Tests use
/// this to avoid querying the live `~/.darkmux/index.db`.
pub(crate) fn role_list_at(path: &Path, json: bool) -> Result<i32> {
    // Derived index: build it on demand if missing or stale (#914) so the
    // verb just works — no manual `darkmux crew index rebuild`.
    ensure_fresh_index(path)?;

    let conn = open_index(path)?;

    let rows = load_role_rows(&conn)?;

    if json {
        // (#907) Full, untruncated description for machine consumers.
        cli_json::emit(&RoleList { roles: rows })?;
        return Ok(0);
    }

    if rows.is_empty() {
        println!("(no roles in index)");
        return Ok(0);
    }

    print_role_table(&rows);
    Ok(0)
}

/// Internal entry for `role show` taking an explicit index path.
pub(crate) fn role_show_at(path: &Path, role_id: &str, json: bool) -> Result<i32> {
    // Build the derived index on demand if missing or stale (#914).
    ensure_fresh_index(path)?;

    let conn = open_index(path)?;

    let row: Option<(String, String, Option<String>, String)> = conn
        .query_row(
            "SELECT id, description, prompt_path, escalation_contract_tag FROM roles WHERE id = ?1",
            params![role_id],
            |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, Option<String>>(2)?,
                    r.get::<_, String>(3)?,
                ))
            },
        )
        .optional()?;

    let (id, description, prompt_path, escalation_tag) = match row {
        Some(r) => r,
        None => {
            bail!(
                "role '{}' not found — no manifest under the crew root defines it \
                 (the index was just rebuilt from the manifests on disk)",
                role_id
            );
        }
    };

    let mut skill_stmt = conn.prepare(
        "SELECT skill_id FROM role_skills WHERE role_id = ?1 ORDER BY skill_id"
    )?;
    let mut skills: Vec<String> = Vec::new();
    for row in skill_stmt.query_map(params![role_id], |r| r.get::<_, String>(0))? {
        skills.push(row?);
    }

    let tool_palette_json: String = conn.query_row(
        "SELECT tool_palette_json FROM roles WHERE id = ?1",
        params![role_id],
        |r| r.get(0),
    )?;

    let palette: ToolPalette = serde_json::from_str(&tool_palette_json)
        .context("parsing tool_palette_json")?;
    let escalation = escalation_of(&escalation_tag)?;

    // #894: a `hand-off-to` role should have a target row, but the index could
    // be inconsistent. `.optional()` so a missing row surfaces as a clear
    // note/null rather than a raw `QueryReturnedNoRows` error.
    let escalation_target: Option<String> = if escalation == EscalationKind::HandOffTo {
        conn.query_row(
            "SELECT target_role_id FROM role_escalation_targets WHERE role_id = ?1",
            params![role_id],
            |r| r.get(0),
        )
        .optional()?
    } else {
        None
    };

    let show = RoleShow { id, description, prompt_path, skills, tool_palette: palette, escalation, escalation_target };
    if json {
        // (#907) machine-readable parity.
        cli_json::emit(&show)?;
        return Ok(0);
    }
    print_role_show(&show);
    Ok(0)
}

/// The text rendering of `role show`.
fn print_role_show(show: &RoleShow) {
    println!("id: {}", show.id);
    println!("description: {}", show.description);

    if let Some(p) = &show.prompt_path {
        println!("prompt_path: {}", p);
    }

    println!("skills:");
    if show.skills.is_empty() {
        println!("  (none)");
    } else {
        for skill in &show.skills {
            println!("  - {}", skill);
        }
    }

    println!("tool_palette:");
    if show.tool_palette.allow.is_empty() && show.tool_palette.deny.is_empty() {
        println!("  (none)");
    } else {
        if !show.tool_palette.allow.is_empty() {
            let allow_str: Vec<String> = show.tool_palette.allow.iter().map(|s| format!("\"{}\"", s)).collect();
            println!("  allow: [{}]", allow_str.join(", "));
        }
        if !show.tool_palette.deny.is_empty() {
            let deny_str: Vec<String> = show.tool_palette.deny.iter().map(|s| format!("\"{}\"", s)).collect();
            println!("  deny: [{}]", deny_str.join(", "));
        }
    }

    println!("escalation: {}", show.escalation.tag());
    if show.escalation == EscalationKind::HandOffTo {
        match &show.escalation_target {
            Some(t) => println!("  target: {}", t),
            None => println!("  target: (unresolved: no target recorded for this role)"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crew::index::rebuild_at;
    use std::env;
    use tempfile::TempDir;

    struct CrewDirGuard {
        prev: Option<String>,
        tmp: TempDir,
    }

    impl CrewDirGuard {
        fn new() -> Self {
            let tmp = TempDir::new().unwrap();
            let prev = env::var("DARKMUX_HOME").ok();
            unsafe {
                env::set_var("DARKMUX_HOME", tmp.path());
            }
            Self { prev, tmp }
        }

        fn path(&self) -> &std::path::Path {
            self.tmp.path()
        }
    }

    impl Drop for CrewDirGuard {
        fn drop(&mut self) {
            unsafe {
                match &self.prev {
                    Some(v) => env::set_var("DARKMUX_HOME", v),
                    None => env::remove_var("DARKMUX_HOME"),
                }
            }
        }
    }

    fn write_role(
        crew_root: &std::path::Path,
        id: &str,
        description: &str,
        skills: &[&str],
        escalation: &str,
        handoff_to: Option<&str>,
    ) {
        let roles_dir = crew_root.join("roles");
        std::fs::create_dir_all(&roles_dir).unwrap();
        let esc = match (escalation, handoff_to) {
            (_, Some(target)) => serde_json::json!({"hand-off-to": target}),
            (tag, _) => serde_json::json!(tag),
        };
        let json = serde_json::json!({
            "id": id,
            "description": description,
            "skills": serde_json::Value::Array(
                skills.iter().map(|s| serde_json::Value::String(s.to_string())).collect()
            ),
            "tool_palette": {
                "allow": ["read"],
                "deny": []
            },
            "escalation_contract": esc
        }).to_string();
        std::fs::write(roles_dir.join(format!("{}.json", id)), json).unwrap();
    }

    fn index_path(root: &std::path::Path) -> std::path::PathBuf {
        root.join("index.db")
    }

    #[serial_test::serial]
    #[test]
    fn role_list_shows_custom_role() {
        let guard = CrewDirGuard::new();
        write_role(guard.path(), "custom", "a custom role", &["coding"], "bail-with-explanation", None);
        let idx = index_path(guard.path());
        rebuild_at(&idx).unwrap();

        let result = role_list_at(&idx, false);
        assert!(result.is_ok(), "role_list_at should succeed: {:?}", result);
    }

    #[serial_test::serial]
    #[test]
    fn role_show_known_role() {
        let guard = CrewDirGuard::new();
        write_role(guard.path(), "alpha", "an alpha role", &["coding"], "retry-with-hint", None);
        let idx = index_path(guard.path());
        rebuild_at(&idx).unwrap();

        let result = role_show_at(&idx, "alpha", false);
        assert!(result.is_ok(), "role_show_at should succeed for alpha: {:?}", result);
    }

    #[serial_test::serial]
    #[test]
    fn role_show_nonexistent_role_errors() {
        let guard = CrewDirGuard::new();
        let idx = index_path(guard.path());
        rebuild_at(&idx).unwrap();

        let result = role_show_at(&idx, "nonexistent", false);
        let err = result.unwrap_err();
        assert!(err.to_string().contains("not found"));
    }

    #[serial_test::serial]
    #[test]
    fn role_show_handoff_shows_target() {
        let guard = CrewDirGuard::new();
        // Target must exist in the roles table (FK constraint on
        // role_escalation_targets.target_role_id). Use the `coder` builtin
        // which falls through from the bundled templates when the user dir
        // has no override.
        write_role(
            guard.path(),
            "supervisor",
            "routes work",
            &[],
            "hand-off-to",
            Some("coder"),
        );
        let idx = index_path(guard.path());
        rebuild_at(&idx).unwrap();

        let result = role_show_at(&idx, "supervisor", false);
        assert!(result.is_ok(), "expected ok, got: {:?}", result);

        let conn = open_index(&idx).unwrap();
        let stored: Option<String> = conn.query_row(
            "SELECT target_role_id FROM role_escalation_targets WHERE role_id = 'supervisor'",
            [],
            |r| r.get(0),
        ).optional().unwrap();
        assert_eq!(stored, Some("coder".to_string()));
    }

    #[serial_test::serial]
    #[test]
    fn role_list_lazy_rebuilds_when_index_missing() {
        // #914: the read-verb must build the derived index on demand instead
        // of bailing with "run `crew index rebuild` first".
        let guard = CrewDirGuard::new();
        write_role(guard.path(), "lazy", "a lazily-indexed role", &["coding"], "bail-with-explanation", None);
        let idx = index_path(guard.path());
        assert!(!idx.exists(), "precondition: no index built yet");

        let result = role_list_at(&idx, false);
        assert!(result.is_ok(), "role_list_at should lazily rebuild: {:?}", result);
        assert!(idx.exists(), "index must have been built on demand");
    }

    #[serial_test::serial]
    #[test]
    fn role_show_lazy_rebuilds_when_index_missing() {
        let guard = CrewDirGuard::new();
        write_role(guard.path(), "shown", "shown role", &["coding"], "retry-with-hint", None);
        let idx = index_path(guard.path());
        assert!(!idx.exists());

        let result = role_show_at(&idx, "shown", false);
        assert!(result.is_ok(), "role_show_at should lazily rebuild + find the role: {:?}", result);
        assert!(idx.exists());
    }

    #[serial_test::serial]
    #[test]
    fn role_skills_are_populated_on_rebuild() {
        // #914 regression: a partial/aborted rebuild left `role_skills` empty
        // so every role rendered `skills: 0`. The prior tests only asserted
        // `is_ok()`, never the skill *content* — this closes that gap.
        let guard = CrewDirGuard::new();
        write_role(guard.path(), "skilled", "has a declared skill", &["coding"], "bail-with-explanation", None);
        let idx = index_path(guard.path());
        rebuild_at(&idx).unwrap();

        let conn = open_index(&idx).unwrap();
        let count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM role_skills WHERE role_id = 'skilled'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(count, 1, "the role's declared skill must land in role_skills");
    }

    #[serial_test::serial]
    #[test]
    fn role_show_handoff_without_target_row_does_not_crash() {
        // #894: a hand-off-to role whose target row is absent (an inconsistent
        // index) must not crash role_show with a raw QueryReturnedNoRows.
        let guard = CrewDirGuard::new();
        write_role(
            guard.path(),
            "supervisor",
            "routes work",
            &[],
            "hand-off-to",
            Some("coder"),
        );
        let idx = index_path(guard.path());
        rebuild_at(&idx).unwrap();
        // Simulate an inconsistent index: drop the recorded target row.
        {
            let conn = open_index(&idx).unwrap();
            conn.execute(
                "DELETE FROM role_escalation_targets WHERE role_id = 'supervisor'",
                [],
            )
            .unwrap();
        }

        let result = role_show_at(&idx, "supervisor", false);
        assert!(
            result.is_ok(),
            "role_show must not crash when the target row is missing: {:?}",
            result
        );
    }

    #[serial_test::serial]
    #[test]
    fn role_list_json_mode_succeeds() {
        // (#907) the --json branch must build + serialize without panicking.
        let guard = CrewDirGuard::new();
        write_role(guard.path(), "jsonrole", "a role", &["coding"], "bail-with-explanation", None);
        let idx = index_path(guard.path());
        rebuild_at(&idx).unwrap();

        let result = role_list_at(&idx, true);
        assert!(result.is_ok(), "role_list_at json mode should succeed: {:?}", result);
    }

    #[serial_test::serial]
    #[test]
    fn role_show_json_mode_succeeds_with_handoff() {
        // (#907) json branch over the richest shape: skills + palette + handoff.
        let guard = CrewDirGuard::new();
        write_role(
            guard.path(),
            "supervisor",
            "routes work",
            &["coding"],
            "hand-off-to",
            Some("coder"),
        );
        let idx = index_path(guard.path());
        rebuild_at(&idx).unwrap();

        let result = role_show_at(&idx, "supervisor", true);
        assert!(result.is_ok(), "role_show_at json mode should succeed: {:?}", result);
    }
}
