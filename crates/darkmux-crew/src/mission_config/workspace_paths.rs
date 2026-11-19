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
    text.match_indices(CONTAINER_WORKSPACE).any(|(at, _)| is_the_path_at(text, at))
}

/// (contract 6) `text` with `/workspace` named as `root` wherever it is the
/// path itself, by the runtime's rule. The host names the system prompt so
/// before it records its hash and passes it as `--system`, which the runtime
/// sends byte for byte: the recorded hash is then the hash of what is sent.
/// Unchanged when `root` is `/workspace`.
pub fn named_as(text: &str, root: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut from = 0;
    for (at, _) in text.match_indices(CONTAINER_WORKSPACE).filter(|(at, _)| is_the_path_at(text, *at)) {
        out.push_str(&text[from..at]);
        out.push_str(root);
        from = at + CONTAINER_WORKSPACE.len();
    }
    out.push_str(&text[from..]);
    out
}

/// Whether the `/workspace` at byte `at` of `text` is the path itself:
/// preceded by the start or a character that cannot be part of a path, and
/// followed by the end, `/`, or a character that cannot continue a name. The
/// boundary is read from the whole text: in `/workspace/workspace` the
/// second follows an `e`, so it is a directory inside.
fn is_the_path_at(text: &str, at: usize) -> bool {
    let name = |c: char| c.is_alphanumeric() || c == '_' || c == '-';
    let path = |c: char| name(c) || c == '/' || c == '.' || c == '~';
    let before_ok = text[..at].chars().next_back().is_none_or(|c| !path(c));
    let after_ok = text[at + CONTAINER_WORKSPACE.len()..].chars().next().is_none_or(|c| c == '/' || !name(c));
    before_ok && after_ok
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

    /// (contract 6) The runtime's rename rule (`runtime/src/tools/workspace.rs`,
    /// `workspace_is_renamed_only_at_a_path_boundary`, the same cases), so
    /// the system prompt the host hashes is the one a host sandbox's model
    /// is sent. Unchanged under `/workspace`.
    #[test]
    fn workspace_is_named_as_the_root_only_at_a_path_boundary() {
        let renamed = |t: &str| named_as(t, "/Users/op/ws");
        assert_eq!(renamed("/workspace"), "/Users/op/ws");
        assert_eq!(renamed("at `/workspace/<source>`."), "at `/Users/op/ws/<source>`.");
        assert_eq!(renamed("cd /workspace && ls /workspace/src"), "cd /Users/op/ws && ls /Users/op/ws/src");
        assert_eq!(renamed("(/workspace), /workspace."), "(/Users/op/ws), /Users/op/ws.");
        for kept in ["/workspace2/x", "/workspaces", "/workspace_x", "/workspace-y", "/data/workspace/x", "a/workspace"] {
            assert_eq!(renamed(kept), kept, "{kept}");
        }
        assert_eq!(renamed("/workspace/workspace/x"), "/Users/op/ws/workspace/x");
        assert_eq!(renamed("/workspace2/workspace"), "/workspace2/workspace");
        assert_eq!(named_as("cd /workspace", "/workspace"), "cd /workspace");
    }
}
