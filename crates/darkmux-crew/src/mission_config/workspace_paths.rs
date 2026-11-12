//! (#3112 review) A user mission config whose step `message` names the
//! container's `/workspace`, written before `${WORKSPACE_ROOT}` existed.
//! Under a sandbox on the host the runtime renames it at a path boundary, so
//! it still works; `darkmux doctor` names it so the operator can move it to
//! the placeholder, which also names the tree before the runtime does.

use serde_json::Value;

const CONTAINER_WORKSPACE: &str = "/workspace";

/// Whether `text` names `/workspace` as a path: the runtime's rename rule
/// (`runtime/src/tools/workspace.rs`, `naming`), a boundary on both sides.
pub fn names_container_workspace(text: &str) -> bool {
    let name = |c: char| c.is_alphanumeric() || c == '_' || c == '-';
    let path = |c: char| name(c) || c == '/' || c == '.' || c == '~';
    text.match_indices(CONTAINER_WORKSPACE).any(|(at, _)| {
        let before_ok = text[..at].chars().next_back().is_none_or(|c| !path(c));
        let after_ok = text[at + CONTAINER_WORKSPACE.len()..].chars().next().is_none_or(|c| c == '/' || !name(c));
        before_ok && after_ok
    })
}

/// Where in `doc` (a mission config) a `message` string names the
/// container's `/workspace`, as dotted paths, sorted.
pub fn container_workspace_messages(doc: &Value) -> Vec<String> {
    let mut out = Vec::new();
    walk(doc, String::new(), &mut out);
    out.sort();
    out
}

fn walk(v: &Value, at: String, out: &mut Vec<String>) {
    match v {
        Value::Object(map) => {
            for (k, child) in map {
                let here = if at.is_empty() { k.clone() } else { format!("{at}.{k}") };
                match child {
                    Value::String(text) if k == "message" && names_container_workspace(text) => out.push(here),
                    _ => walk(child, here, out),
                }
            }
        }
        Value::Array(items) => items.iter().enumerate().for_each(|(i, child)| walk(child, format!("{at}[{i}]"), out)),
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// Every `message` string anywhere in the document that names
    /// `/workspace` as a path (never `/workspace2`, `/data/workspace` or the
    /// placeholder) is reported by where it sits.
    #[test]
    fn a_message_naming_the_container_workspace_is_found_by_its_place() {
        let doc = json!({
            "phases": [{"tasks": [{"steps": [
                {"id": "a", "config": {"message": "mounted at `/workspace/<source>`"}},
                {"id": "b", "config": {"message": "mounted at `${WORKSPACE_ROOT}/<source>`"}},
                {"id": "c", "config": {"message": "see /workspace2 and /data/workspace"}},
            ]}], "grow": {"config": {"message": "cd /workspace"}}}],
            "description": "/workspace in a description is not a message",
        });
        assert_eq!(
            container_workspace_messages(&doc),
            vec!["phases[0].grow.config.message".to_string(), "phases[0].tasks[0].steps[0].config.message".to_string()]
        );
        assert!(names_container_workspace("(/workspace)."));
        assert!(!names_container_workspace("/workspaces"));
    }
}
