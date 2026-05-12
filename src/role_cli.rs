use crate::crew::index::{default_index_path, open_index};
use anyhow::{Context, Result, bail};
use rusqlite::{params, OptionalExtension};
use std::path::Path;

/// Print a table listing every role in the index.
pub fn role_list() -> Result<i32> {
    role_list_at(&default_index_path())
}

/// Print full details for a single role.
pub fn role_show(role_id: &str) -> Result<i32> {
    role_show_at(&default_index_path(), role_id)
}

/// Internal entry for `role list` taking an explicit index path. Tests use
/// this to avoid querying the live `~/.darkmux/index.db`.
pub(crate) fn role_list_at(path: &Path) -> Result<i32> {
    if !path.exists() {
        bail!(
            "no index at {} — run `darkmux crew index rebuild` first",
            path.display()
        );
    }

    let conn = open_index(path)?;

    let mut stmt = conn.prepare(
        "SELECT r.id, \
         CASE WHEN LENGTH(r.description) > 60 THEN SUBSTR(r.description, 1, 57) || '…' ELSE r.description END, \
         COALESCE(rc.cap_count, 0), \
         r.escalation_contract_tag \
         FROM roles r \
         LEFT JOIN (SELECT role_id, COUNT(*) AS cap_count FROM role_capabilities GROUP BY role_id) rc \
         ON r.id = rc.role_id \
         ORDER BY r.id"
    )?;

    let mut rows: Vec<(String, String, i32, String)> = Vec::new();
    let stmt_rows = stmt.query_map([], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, i32>(2)?,
            r.get::<_, String>(3)?,
        ))
    })?;
    for row in stmt_rows {
        rows.push(row?);
    }

    if rows.is_empty() {
        println!("(no roles in index)");
        return Ok(0);
    }

    let mut id_w: usize = 2;
    let mut desc_w: usize = 12;
    let mut cap_w: usize = 11;
    let mut esc_w: usize = 11;

    for (id, desc, caps, esc) in &rows {
        id_w = id_w.max(id.len());
        desc_w = desc_w.max(desc.len());
        cap_w = cap_w.max(caps.to_string().len());
        esc_w = esc_w.max(esc.len());
    }

    println!(
        "{:<id_w$}  {:<desc_w$}  {:<cap_w$}  escalation",
        "id", "description", "capabilities"
    );

    for (id, desc, caps, esc) in &rows {
        println!(
            "{:<id_w$}  {:<desc_w$}  {:<cap_w$}  {}",
            id, desc, caps, esc
        );
    }

    Ok(0)
}

/// Internal entry for `role show` taking an explicit index path.
pub(crate) fn role_show_at(path: &Path, role_id: &str) -> Result<i32> {
    if !path.exists() {
        bail!(
            "no index at {} — run `darkmux crew index rebuild` first",
            path.display()
        );
    }

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
                "role '{}' not found in index — run `darkmux crew index rebuild` if it was added recently",
                role_id
            );
        }
    };

    println!("id: {}", id);
    println!("description: {}", description);

    if let Some(p) = &prompt_path {
        println!("prompt_path: {}", p);
    }

    let mut cap_stmt = conn.prepare(
        "SELECT capability_id FROM role_capabilities WHERE role_id = ?1 ORDER BY capability_id"
    )?;
    let mut caps: Vec<String> = Vec::new();
    for row in cap_stmt.query_map(params![role_id], |r| r.get::<_, String>(0))? {
        caps.push(row?);
    }

    println!("capabilities:");
    if caps.is_empty() {
        println!("  (none)");
    } else {
        for cap in &caps {
            println!("  - {}", cap);
        }
    }

    let tool_palette_json: String = conn.query_row(
        "SELECT tool_palette_json FROM roles WHERE id = ?1",
        params![role_id],
        |r| r.get(0),
    )?;

    let palette: serde_json::Value = serde_json::from_str(&tool_palette_json)
        .context("parsing tool_palette_json")?;

    let allow_vals: Vec<&str> = palette.get("allow")
        .and_then(|a| a.as_array())
        .map(|arr| arr.iter().filter_map(|v| v.as_str()).collect())
        .unwrap_or_default();

    let deny_vals: Vec<&str> = palette.get("deny")
        .and_then(|a| a.as_array())
        .map(|arr| arr.iter().filter_map(|v| v.as_str()).collect())
        .unwrap_or_default();

    println!("tool_palette:");
    if allow_vals.is_empty() && deny_vals.is_empty() {
        println!("  (none)");
    } else {
        if !allow_vals.is_empty() {
            let allow_str: Vec<String> = allow_vals.iter().map(|s| format!("\"{}\"", s)).collect();
            println!("  allow: [{}]", allow_str.join(", "));
        }
        if !deny_vals.is_empty() {
            let deny_str: Vec<String> = deny_vals.iter().map(|s| format!("\"{}\"", s)).collect();
            println!("  deny: [{}]", deny_str.join(", "));
        }
    }

    println!("escalation: {}", escalation_tag);
    if escalation_tag == "hand-off-to" {
        let target: String = conn.query_row(
            "SELECT target_role_id FROM role_escalation_targets WHERE role_id = ?1",
            params![role_id],
            |r| r.get(0),
        )?;
        println!("  target: {}", target);
    }

    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crew::index::rebuild_at;
    use std::env;
    use std::path::PathBuf;
    use tempfile::TempDir;

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

        fn path(&self) -> &std::path::Path {
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
        crew_root: &std::path::Path,
        id: &str,
        description: &str,
        capabilities: &[&str],
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
            "capabilities": serde_json::Value::Array(
                capabilities.iter().map(|s| serde_json::Value::String(s.to_string())).collect()
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

        let result = role_list_at(&idx);
        assert!(result.is_ok(), "role_list_at should succeed: {:?}", result);
    }

    #[serial_test::serial]
    #[test]
    fn role_show_known_role() {
        let guard = CrewDirGuard::new();
        write_role(guard.path(), "alpha", "an alpha role", &["coding"], "retry-with-hint", None);
        let idx = index_path(guard.path());
        rebuild_at(&idx).unwrap();

        let result = role_show_at(&idx, "alpha");
        assert!(result.is_ok(), "role_show_at should succeed for alpha: {:?}", result);
    }

    #[serial_test::serial]
    #[test]
    fn role_show_nonexistent_role_errors() {
        let guard = CrewDirGuard::new();
        let idx = index_path(guard.path());
        rebuild_at(&idx).unwrap();

        let result = role_show_at(&idx, "nonexistent");
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

        let result = role_show_at(&idx, "supervisor");
        assert!(result.is_ok(), "expected ok, got: {:?}", result);

        let conn = open_index(&idx).unwrap();
        let stored: Option<String> = conn.query_row(
            "SELECT target_role_id FROM role_escalation_targets WHERE role_id = 'supervisor'",
            [],
            |r| r.get(0),
        ).optional().unwrap();
        assert_eq!(stored, Some("coder".to_string()));
    }

    #[test]
    fn role_list_errors_when_index_missing() {
        let nonexistent = PathBuf::from("/tmp/darkmux-role-cli-missing-index-test.db");
        let _ = std::fs::remove_file(&nonexistent);
        let result = role_list_at(&nonexistent);
        let err = result.unwrap_err();
        assert!(err.to_string().contains("no index at"));
    }

    #[test]
    fn role_show_errors_when_index_missing() {
        let nonexistent = PathBuf::from("/tmp/darkmux-role-cli-missing-index-test2.db");
        let _ = std::fs::remove_file(&nonexistent);
        let result = role_show_at(&nonexistent, "any");
        let err = result.unwrap_err();
        assert!(err.to_string().contains("no index at"));
    }
}
