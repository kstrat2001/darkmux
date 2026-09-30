//! The utility hold reaches the planner only if every production launcher
//! builds its `Facts` through `standing_facts`. A launcher that writes
//! `Facts::default()` (or a literal with no binding) forfeits the protection
//! and no planner test notices, because the planner tests hand-build `Facts`.
//! Two launchers (`mission launch`, the ACP panel) drive the real LMStudio
//! host and cannot be exercised through their seam, so this pins the source:
//! each launcher calls the one builder, and no other non-test code builds a
//! `Facts` literal.

use std::path::{Path, PathBuf};

/// The non-test portion of a source file (everything before its test module).
fn production_source(path: &str) -> String {
    let text = std::fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join(path))
        .unwrap_or_else(|e| panic!("read {path}: {e}"));
    production_part(&text).to_string()
}

/// Everything before the file's `mod tests` block.
fn production_part(text: &str) -> &str {
    let cut = text.find("\nmod tests {").or_else(|| text.find("\n#[cfg(test)]\nmod ")).unwrap_or(text.len());
    &text[..cut]
}

/// True when `line` names the gestalt `Facts` type being built (a literal or
/// `::default()`), not a longer identifier that merely ends in `Facts`.
fn builds_facts(line: &str) -> bool {
    let mut from = 0;
    while let Some(i) = line[from..].find("Facts") {
        let at = from + i;
        let before_ok = line[..at].chars().next_back().is_none_or(|c| !(c.is_alphanumeric() || c == '_'));
        let rest = &line[at + 5..];
        if before_ok && (rest.starts_with(" {") || rest.starts_with("::default()")) && !line.contains("struct ") && !line.contains("impl ") {
            return true;
        }
        from = at + 5;
    }
    false
}

fn rust_files(dir: &Path, out: &mut Vec<PathBuf>) {
    for entry in std::fs::read_dir(dir).unwrap().flatten() {
        let p = entry.path();
        if p.is_dir() {
            rust_files(&p, out);
        } else if p.extension().is_some_and(|e| e == "rs") {
            out.push(p);
        }
    }
}

/// Every launcher that hands a scheduler its `Facts`, by the builder call it
/// must contain.
const LAUNCHERS: [(&str, &str); 3] = [
    ("src/mission_launch.rs", "standing_facts(None)"),
    ("src/acp_panel.rs", "standing_facts(None)"),
    ("crates/darkmux-crew/src/dispatch_as_crew_of_one.rs", "standing_facts(opts.config_path.as_deref())"),
];

#[test]
fn every_launcher_builds_its_facts_through_standing_facts() {
    for (path, call) in LAUNCHERS {
        assert!(
            production_source(path).contains(call),
            "{path} must build its Facts with `{call}` so the standing utility model is held resident"
        );
    }
}

#[test]
fn no_production_code_builds_a_facts_literal_outside_the_builder_and_the_planner_seam() {
    // `concurrent_dispatch.rs` builds the per-reconcile snapshot from the
    // binding it is handed; `standing_facts` lives there too. `mock.rs` is the
    // test host. Nothing else may write `Facts {` or `Facts::default()`.
    let allowed = ["concurrent_dispatch.rs", "mock.rs", "facts.rs"];
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut files = Vec::new();
    rust_files(&root.join("src"), &mut files);
    rust_files(&root.join("crates"), &mut files);
    let mut offenders = Vec::new();
    for f in files {
        let name = f.file_name().unwrap().to_string_lossy().to_string();
        if allowed.contains(&name.as_str()) || name.contains("test") || f.to_string_lossy().contains("/tests/") {
            continue;
        }
        let text = std::fs::read_to_string(&f).unwrap();
        for (i, line) in production_part(&text).lines().enumerate() {
            let t = line.trim_start();
            if !t.starts_with("//") && builds_facts(t) {
                offenders.push(format!("{}:{}: {}", f.display(), i + 1, t));
            }
        }
    }
    assert!(offenders.is_empty(), "a Facts literal built outside `standing_facts`:\n{}", offenders.join("\n"));
}
