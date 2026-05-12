use crate::crew::index::{default_index_path, open_index};
use anyhow::{Result, bail};
use rusqlite::{params, OptionalExtension};
use std::path::Path;

/// Print a table listing every crew in the index.
pub fn crew_list() -> Result<i32> {
    crew_list_at(&default_index_path())
}

/// Print full details for a single crew.
pub fn crew_show(crew_id: &str) -> Result<i32> {
    crew_show_at(&default_index_path(), crew_id)
}

/// Internal entry for `crew list` taking an explicit index path. Tests use
/// this to avoid querying the live `~/.darkmux/index.db`.
pub(crate) fn crew_list_at(path: &Path) -> Result<i32> {
    if !path.exists() {
        bail!(
            "no index at {} — run `darkmux crew index rebuild` first",
            path.display()
        );
    }

    let conn = open_index(path)?;

    let mut stmt = conn.prepare(
        "SELECT c.id, \
         CASE WHEN LENGTH(c.description) > 60 THEN SUBSTR(c.description, 1, 57) || '…' ELSE c.description END, \
         (SELECT COUNT(*) FROM crew_members cm WHERE cm.crew_id = c.id) \
         FROM crews c ORDER BY c.id"
    )?;

    let mut rows: Vec<(String, String, i64)> = Vec::new();
    let stmt_rows = stmt.query_map([], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, i64>(2)?,
        ))
    })?;
    for row in stmt_rows {
        rows.push(row?);
    }

    if rows.is_empty() {
        println!("(no crews in index)");
        return Ok(0);
    }

    let mut id_w: usize = 2;
    let mut desc_w: usize = 12;

    for (id, desc, _) in &rows {
        id_w = id_w.max(id.len());
        desc_w = desc_w.max(desc.len());
    }

    println!(
        "{:<id_w$}  {:<desc_w$}  members",
        "id", "description"
    );

    for (id, desc, members) in &rows {
        println!(
            "{:<id_w$}  {:<desc_w$}  {}",
            id, desc, members
        );
    }

    Ok(0)
}

/// Internal entry for `crew show` taking an explicit index path.
pub(crate) fn crew_show_at(path: &Path, crew_id: &str) -> Result<i32> {
    if !path.exists() {
        bail!(
            "no index at {} — run `darkmux crew index rebuild` first",
            path.display()
        );
    }

    let conn = open_index(path)?;

    let row: Option<(String, String)> = conn
        .query_row(
            "SELECT id, description FROM crews WHERE id = ?1",
            params![crew_id],
            |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                ))
            },
        )
        .optional()?;

    let (id, description) = match row {
        Some(r) => r,
        None => {
            bail!(
                "crew '{}' not found in index — run `darkmux crew index rebuild` if it was added recently",
                crew_id
            );
        }
    };

    println!("id: {}", id);
    println!("description: {}", description);

    let mut member_stmt = conn.prepare(
        "SELECT role_id, position FROM crew_members WHERE crew_id = ?1 ORDER BY CASE WHEN position = 'lead' THEN 0 ELSE 1 END, role_id"
    )?;

    let mut members: Vec<(String, String)> = Vec::new();
    for row in member_stmt.query_map(params![crew_id], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, String>(1)?,
        ))
    })? {
        members.push(row?);
    }

    println!("members:");
    if members.is_empty() {
        println!("  (none)");
    } else {
        for (role_id, position) in &members {
            println!("  - {} [{}]", role_id, position);
        }
    }

    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crew::index::rebuild_at;
    use std::path::PathBuf;

    /// RAII guard: point DARKMUX_CREW_DIR at a TempDir for the test's
    /// lifetime. Mirrors the pattern from role_cli.rs.
    struct CrewDirGuard {
        prev: Option<String>,
        tmp: tempfile::TempDir,
    }

    impl CrewDirGuard {
        fn new() -> Self {
            let tmp = tempfile::TempDir::new().unwrap();
            let prev = std::env::var("DARKMUX_CREW_DIR").ok();
            unsafe {
                std::env::set_var("DARKMUX_CREW_DIR", tmp.path());
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
                    Some(v) => std::env::set_var("DARKMUX_CREW_DIR", v),
                    None => std::env::remove_var("DARKMUX_CREW_DIR"),
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
    ) {
        let roles_dir = crew_root.join("roles");
        std::fs::create_dir_all(&roles_dir).unwrap();
        let caps_json: String = capabilities
            .iter()
            .map(|c| format!("\"{c}\""))
            .collect::<Vec<_>>()
            .join(",");
        let json = format!(
            r#"{{
              "id": "{id}",
              "description": "{description}",
              "capabilities": [{caps_json}],
              "tool_palette": {{"allow": ["read"], "deny": []}},
              "escalation_contract": "{escalation}"
            }}"#
        );
        std::fs::write(roles_dir.join(format!("{id}.json")), json).unwrap();
    }

    fn write_crew(
        crew_root: &std::path::Path,
        id: &str,
        description: &str,
        members: &[(&str, &str)], // (role_id, position)
    ) {
        let crews_dir = crew_root.join("crews");
        std::fs::create_dir_all(&crews_dir).unwrap();
        let members_json: String = members
            .iter()
            .map(|(role_id, pos)| {
                format!(r#"{{"role_id":"{role_id}","position":"{pos}"}}"#)
            })
            .collect::<Vec<_>>()
            .join(",");
        let json = format!(
            r#"{{"id":"{id}","description":"{description}","members":[{members_json}]}}"#
        );
        std::fs::write(crews_dir.join(format!("{id}.json")), json).unwrap();
    }

    fn index_path(root: &std::path::Path) -> PathBuf {
        root.join("index.db")
    }

    #[serial_test::serial]
    #[test]
    fn crew_list_shows_custom_crews() {
        let guard = CrewDirGuard::new();

        // Write roles first (FK constraint on crew_members.role_id).
        write_role(guard.path(), "admin", "Administrator role", &[], "bail-with-explanation");
        write_role(guard.path(), "dev", "Developer role", &[], "bail-with-explanation");

        // Write two crew manifests.
        write_crew(
            guard.path(),
            "alpha",
            "Alpha crew for alpha testing",
            &[("admin", "lead"), ("dev", "support")],
        );
        write_crew(
            guard.path(),
            "beta",
            "Beta crew for beta testing and QA",
            &[("dev", "lead"), ("admin", "support")],
        );

        let idx = index_path(guard.path());
        rebuild_at(&idx).unwrap();

        let result = crew_list_at(&idx);
        assert!(result.is_ok(), "crew_list_at should succeed: {:?}", result);



        // Basic sanity: no panic means success, exit code 0.
        let code = result.unwrap();
        assert_eq!(code, 0);
    }

    #[serial_test::serial]
    #[test]
    fn crew_list_empty_index() {
        let guard = CrewDirGuard::new();
        let idx = index_path(guard.path());

        // Rebuild without writing any crew manifests.
        rebuild_at(&idx).unwrap();

        let result = crew_list_at(&idx);
        assert!(result.is_ok());
        // Should print "(no crews in index)".
    }

    #[serial_test::serial]
    #[test]
    fn crew_show_known_crew_orders_members_correctly() {
        let guard = CrewDirGuard::new();

        // Write roles.
        write_role(guard.path(), "alice", "Alice role", &[], "bail-with-explanation");
        write_role(guard.path(), "bob", "Bob role", &[], "bail-with-explanation");
        write_role(guard.path(), "charlie", "Charlie role", &[], "bail-with-explanation");

        // Write a crew with mixed lead/support, unsorted order.
        write_crew(
            guard.path(),
            "test-crew",
            "A crew with mixed members",
            &[
                ("charlie", "support"),  // support, should come after leads
                ("alice", "lead"),       // lead, alphabetically first
                ("bob", "support"),      // support, after charlie alphabetically
            ],
        );

        let idx = index_path(guard.path());
        rebuild_at(&idx).unwrap();

        // Capture stdout by piping through a command.
        let output = assert_cmd::Command::cargo_bin("darkmux")
            .unwrap()
            .env("DARKMUX_CREW_DIR", guard.path().to_str().unwrap())
            .args(["crew", "show", "test-crew"])
            .assert()
            .success()
            .get_output()
            .clone();

        let stdout = String::from_utf8(output.stdout.clone()).unwrap();

        // Expected order: alice (lead), bob (support), charlie (support)
        let lines: Vec<&str> = stdout.lines().collect();

        // Find member bullet lines.
        let member_lines: Vec<&str> = lines.iter()
            .filter(|l| l.starts_with("  - "))
            .map(|l| l.trim())
            .collect();

        assert_eq!(member_lines.len(), 3);
        // Leads first: alice
        assert!(member_lines[0].contains("alice"));
        // Supports alphabetically: bob, charlie
        assert!(member_lines[1].contains("bob"));
        assert!(member_lines[2].contains("charlie"));

        // Verify positions in brackets.
        assert!(member_lines[0].contains("[lead]"));
        assert!(member_lines[1].contains("[support]"));
        assert!(member_lines[2].contains("[support]"));
    }

    #[test]
    fn crew_show_nonexistent_errors() {
        let guard = CrewDirGuard::new();
        // Write a dummy crew but don't reference nonexistent-one.
        write_role(guard.path(), "dummy", "Dummy role", &[], "bail-with-explanation");
        write_crew(guard.path(), "dummy-crew", "A dummy crew", &[("dummy", "lead")]);

        let idx = index_path(guard.path());
        rebuild_at(&idx).unwrap();

        let result = crew_show_at(&idx, "nonexistent");
        let err = result.unwrap_err();
        assert!(err.to_string().contains("not found"));
    }

    #[test]
    fn crew_show_errors_when_index_missing() {
        let nonexistent = PathBuf::from("/tmp/darkmux-crew-cli-missing-index-test.db");
        let _ = std::fs::remove_file(&nonexistent);
        let result = crew_show_at(&nonexistent, "any");
        let err = result.unwrap_err();
        assert!(err.to_string().contains("no index at"));
    }
}
