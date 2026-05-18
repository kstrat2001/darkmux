//! Agent tool implementations.
//!
//! Tools shipped in the spike palette:
//!
//! - `echo`  — Phase 2 placeholder, kept for sanity tests
//! - `bash`  — run a bash command with cwd=/workspace
//! - `read`  — read a file from inside /workspace
//! - `write` — write a file to inside /workspace
//! - `edit`  — targeted patch on an existing file (Phase 7-bis)
//!
//! `edit` was added after Phase 6d's diagnostic compared openclaw's
//! coder palette (5 tools, including `edit`) to the spike's (4 tools,
//! `write`-only). The spike was completing the same work openclaw did
//! but needed 2× the tool calls for any file modification (read +
//! full-write where openclaw uses one targeted edit). Adding `edit`
//! closes that granularity gap.
//!
//! The path-validation contract is enforced in `workspace.rs` and is
//! the security-critical piece. Every Read / Write / Bash invocation
//! routes through it.
//!
//! Each tool implements:
//! - a stable `name`
//! - an LLM-facing `description`
//! - a JSON-Schema for `parameters` the model emits
//! - an `execute(args) -> Result<String>` that returns the text put
//!   back into the conversation as a `role: tool` message

pub mod workspace;

use anyhow::{anyhow, Context, Result};
use serde::Deserialize;
use std::path::Path;
use std::process::Command;

use crate::lmstudio::{FunctionDef, ToolDef};
use workspace::{resolve_read, resolve_write, DEFAULT_WORKSPACE};

/// Hard cap on how much content a Read tool returns. Anything bigger
/// gets truncated with a marker so the model knows. Keeps a single
/// tool result from blowing the context window on its own.
const READ_MAX_BYTES: usize = 1024 * 1024; // 1 MB

/// Default cap on how long a Bash command can run before timing out.
/// Overridable per-invocation via the tool's `timeout_seconds` arg.
const BASH_DEFAULT_TIMEOUT_SECS: u64 = 30;

/// All tools the runtime can dispatch in this phase.
#[derive(Debug, Clone, Copy)]
pub enum Tool {
    Echo,
    Bash,
    Read,
    Write,
    Edit,
}

impl Tool {
    pub fn name(self) -> &'static str {
        match self {
            Tool::Echo => "echo",
            Tool::Bash => "bash",
            Tool::Read => "read",
            Tool::Write => "write",
            Tool::Edit => "edit",
        }
    }

    pub fn description(self) -> &'static str {
        match self {
            Tool::Echo => {
                "Echoes the provided text back to the caller. Use this \
                 to verify the tool-calling round-trip works. \
                 Arguments: { text: string }."
            }
            Tool::Bash => {
                "Runs a bash command inside the dispatch workspace (cwd = \
                 /workspace). Returns the exit code, stdout, and stderr. \
                 Useful for running tests, listing files, running grep, \
                 git operations, etc. The command cannot affect anything \
                 outside the workspace (the container is the boundary). \
                 Arguments: { command: string, timeout_seconds?: integer }."
            }
            Tool::Read => {
                "Reads a file from inside the workspace. Path may be \
                 absolute (/workspace/...) or relative (resolved against \
                 the workspace root). Paths that resolve outside the \
                 workspace are rejected. Returns the file's contents \
                 (truncated to 1 MB if larger). \
                 Arguments: { path: string }."
            }
            Tool::Write => {
                "Writes a file inside the workspace. Path may be \
                 absolute (/workspace/...) or relative. The parent \
                 directory must already exist (use `bash` with mkdir -p \
                 if it doesn't). Paths that resolve outside the workspace \
                 are rejected. PREFER `edit` over `write` when modifying \
                 an existing file — it's targeted, cheaper, and preserves \
                 lines you didn't touch. \
                 Arguments: { path: string, content: string }."
            }
            Tool::Edit => {
                "Applies a targeted patch to an existing file: replaces \
                 occurrences of `old_string` with `new_string`. Default \
                 mode requires `old_string` to appear EXACTLY ONCE in the \
                 file — pass `replace_all: true` to replace every \
                 occurrence. The file must exist; paths must resolve \
                 inside the workspace. Preserves all other content. Use \
                 this rather than `write` for any modification to an \
                 existing file — far cheaper and safer than rewriting the \
                 whole file. \
                 Arguments: { path: string, old_string: string, new_string: string, replace_all?: bool }."
            }
        }
    }

    pub fn parameters_schema(self) -> serde_json::Value {
        match self {
            Tool::Echo => serde_json::json!({
                "type": "object",
                "properties": {
                    "text": { "type": "string", "description": "The text to echo back." }
                },
                "required": ["text"]
            }),
            Tool::Bash => serde_json::json!({
                "type": "object",
                "properties": {
                    "command": {
                        "type": "string",
                        "description": "The bash command to execute. Runs with cwd=/workspace."
                    },
                    "timeout_seconds": {
                        "type": "integer",
                        "description": "Optional timeout in seconds. Default 30, max 300.",
                        "minimum": 1,
                        "maximum": 300
                    }
                },
                "required": ["command"]
            }),
            Tool::Read => serde_json::json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "Path to read. Absolute /workspace/... or workspace-relative."
                    }
                },
                "required": ["path"]
            }),
            Tool::Write => serde_json::json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "Path to write. Absolute /workspace/... or workspace-relative. Parent dir must exist."
                    },
                    "content": {
                        "type": "string",
                        "description": "The file content to write."
                    }
                },
                "required": ["path", "content"]
            }),
            Tool::Edit => serde_json::json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "Path to edit. File must exist. Absolute /workspace/... or workspace-relative."
                    },
                    "old_string": {
                        "type": "string",
                        "description": "Text to replace. Must appear in the file. Required to be unique unless replace_all=true."
                    },
                    "new_string": {
                        "type": "string",
                        "description": "Replacement text. Must differ from old_string."
                    },
                    "replace_all": {
                        "type": "boolean",
                        "description": "If true, replace ALL occurrences. Default false (require unique match)."
                    }
                },
                "required": ["path", "old_string", "new_string"]
            }),
        }
    }

    pub fn to_tool_def(self) -> ToolDef {
        ToolDef {
            kind: "function".into(),
            function: FunctionDef {
                name: self.name().into(),
                description: self.description().into(),
                parameters: self.parameters_schema(),
            },
        }
    }

    pub fn execute(self, raw_args: &str) -> Result<String> {
        match self {
            Tool::Echo => execute_echo(raw_args),
            Tool::Bash => execute_bash(raw_args, Path::new(DEFAULT_WORKSPACE)),
            Tool::Read => execute_read(raw_args, Path::new(DEFAULT_WORKSPACE)),
            Tool::Write => execute_write(raw_args, Path::new(DEFAULT_WORKSPACE)),
            Tool::Edit => execute_edit(raw_args, Path::new(DEFAULT_WORKSPACE)),
        }
    }

    pub fn from_name(name: &str) -> Option<Self> {
        match name {
            "echo" => Some(Tool::Echo),
            "bash" => Some(Tool::Bash),
            "read" => Some(Tool::Read),
            "write" => Some(Tool::Write),
            "edit" => Some(Tool::Edit),
            _ => None,
        }
    }
}

/// Dispatch a tool by name with raw JSON args. Returns the string the
/// runtime should put back into the conversation as a `role: tool`
/// message. Tool-execution errors are converted to a returned error
/// message string (not panics) so the model gets a chance to recover.
pub fn dispatch(name: &str, raw_args: &str) -> String {
    match Tool::from_name(name) {
        Some(tool) => match tool.execute(raw_args) {
            Ok(result) => result,
            Err(e) => format!("tool '{name}' returned error: {e:#}"),
        },
        None => format!(
            "tool '{name}' is not available in this runtime. \
             known tools: echo, bash, read, write, edit"
        ),
    }
}

// ─── echo ─────────────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct EchoArgs {
    text: String,
}

fn execute_echo(raw_args: &str) -> Result<String> {
    let args: EchoArgs = serde_json::from_str(raw_args)
        .with_context(|| format!("parsing echo arguments: {raw_args}"))?;
    Ok(args.text)
}

// ─── bash ─────────────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct BashArgs {
    command: String,
    #[serde(default)]
    timeout_seconds: Option<u64>,
}

fn execute_bash(raw_args: &str, workspace_root: &Path) -> Result<String> {
    let args: BashArgs = serde_json::from_str(raw_args)
        .with_context(|| format!("parsing bash arguments: {raw_args}"))?;

    let timeout_secs = args
        .timeout_seconds
        .unwrap_or(BASH_DEFAULT_TIMEOUT_SECS)
        .min(300);

    // Use `timeout` (from coreutils, present on Alpine + most Linux) so
    // we don't hand-roll a Rust timeout. macOS stock doesn't ship
    // `timeout` in PATH; falling back to direct bash there lets the
    // unit tests run on the host without forcing every dev to brew
    // install coreutils. In production (Alpine container), `timeout`
    // is always present.
    //
    // If `timeout` fires, exit code is 124 — we surface that marker
    // explicitly in the returned text.
    let output = if has_timeout_command() {
        Command::new("timeout")
            .arg(format!("{timeout_secs}"))
            .arg("bash")
            .arg("-c")
            .arg(&args.command)
            .current_dir(workspace_root)
            .output()
    } else {
        Command::new("bash")
            .arg("-c")
            .arg(&args.command)
            .current_dir(workspace_root)
            .output()
    }
    .with_context(|| format!("spawning bash for: {}", args.command))?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let exit_code = output.status.code().unwrap_or(-1);

    let timed_out_marker = if exit_code == 124 {
        format!(" (TIMED OUT after {timeout_secs}s)")
    } else {
        String::new()
    };

    Ok(format!(
        "exit: {exit_code}{timed_out_marker}\n\
         --- stdout ---\n{stdout}\n\
         --- stderr ---\n{stderr}"
    ))
}

/// Probe whether the `timeout` command is available. Alpine has it via
/// coreutils-default-symlinks; stock macOS doesn't.
fn has_timeout_command() -> bool {
    Command::new("sh")
        .arg("-c")
        .arg("command -v timeout >/dev/null 2>&1")
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

// ─── read ─────────────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct ReadArgs {
    path: String,
}

fn execute_read(raw_args: &str, workspace_root: &Path) -> Result<String> {
    let args: ReadArgs = serde_json::from_str(raw_args)
        .with_context(|| format!("parsing read arguments: {raw_args}"))?;

    let path = resolve_read(&args.path, workspace_root)?;

    let bytes = std::fs::read(&path)
        .with_context(|| format!("reading file: {path:?}"))?;

    if bytes.len() > READ_MAX_BYTES {
        let truncated = String::from_utf8_lossy(&bytes[..READ_MAX_BYTES]);
        Ok(format!(
            "{truncated}\n\n--- [truncated; original size {} bytes] ---",
            bytes.len()
        ))
    } else {
        Ok(String::from_utf8_lossy(&bytes).into_owned())
    }
}

// ─── write ────────────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct WriteArgs {
    path: String,
    content: String,
}

fn execute_write(raw_args: &str, workspace_root: &Path) -> Result<String> {
    let args: WriteArgs = serde_json::from_str(raw_args)
        .with_context(|| format!("parsing write arguments: {raw_args}"))?;

    let path = resolve_write(&args.path, workspace_root)?;

    std::fs::write(&path, args.content.as_bytes())
        .with_context(|| format!("writing file: {path:?}"))?;

    Ok(format!(
        "Wrote {} bytes to {}",
        args.content.len(),
        path.display()
    ))
}

// ─── edit ─────────────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct EditArgs {
    path: String,
    old_string: String,
    new_string: String,
    #[serde(default)]
    replace_all: bool,
}

fn execute_edit(raw_args: &str, workspace_root: &Path) -> Result<String> {
    let args: EditArgs = serde_json::from_str(raw_args)
        .with_context(|| format!("parsing edit arguments: {raw_args}"))?;

    if args.old_string.is_empty() {
        return Err(anyhow!("edit: old_string cannot be empty"));
    }
    if args.old_string == args.new_string {
        return Err(anyhow!(
            "edit: old_string and new_string are identical — no change to apply"
        ));
    }

    // File must already exist (resolve_read enforces that).
    let path = resolve_read(&args.path, workspace_root)?;

    let content = std::fs::read_to_string(&path)
        .with_context(|| format!("reading file for edit: {path:?}"))?;

    let count = content.matches(&args.old_string).count();
    if count == 0 {
        return Err(anyhow!(
            "edit: old_string not found in {path:?}. \
             Did you mean to use `write` to create a new file?"
        ));
    }
    if count > 1 && !args.replace_all {
        return Err(anyhow!(
            "edit: old_string appears {count} times in {path:?}. \
             Pass replace_all=true to replace every occurrence, or \
             provide more surrounding context to make old_string unique."
        ));
    }

    let replacements = if args.replace_all { count } else { 1 };
    let new_content = if args.replace_all {
        content.replace(&args.old_string, &args.new_string)
    } else {
        content.replacen(&args.old_string, &args.new_string, 1)
    };

    std::fs::write(&path, new_content.as_bytes())
        .with_context(|| format!("writing edited file: {path:?}"))?;

    Ok(format!(
        "Edited {} ({replacements} replacement{} applied)",
        path.display(),
        if replacements == 1 { "" } else { "s" }
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempdir::TempDir;

    fn fresh_workspace() -> TempDir {
        TempDir::new("darkmux-agent-tools-test").expect("create tempdir")
    }

    // ─── echo ─────────────────────────────────────────────────────────────

    #[test]
    fn echo_returns_text_arg() {
        let result = dispatch("echo", r#"{"text": "hello"}"#);
        assert_eq!(result, "hello");
    }

    #[test]
    fn unknown_tool_returns_error_message_not_panic() {
        let result = dispatch("teleport", r#"{}"#);
        assert!(result.contains("not available"));
    }

    // ─── bash ─────────────────────────────────────────────────────────────

    #[test]
    fn bash_returns_stdout_and_exit_code() {
        let ws = fresh_workspace();
        let raw = serde_json::json!({"command": "echo from-bash"}).to_string();
        let result = execute_bash(&raw, ws.path()).unwrap();
        assert!(result.contains("exit: 0"));
        assert!(result.contains("from-bash"));
    }

    #[test]
    fn bash_captures_stderr() {
        let ws = fresh_workspace();
        let raw = serde_json::json!({"command": "echo oops >&2"}).to_string();
        let result = execute_bash(&raw, ws.path()).unwrap();
        assert!(result.contains("oops"));
    }

    #[test]
    fn bash_runs_in_workspace_cwd() {
        let ws = fresh_workspace();
        let raw = serde_json::json!({"command": "pwd"}).to_string();
        let result = execute_bash(&raw, ws.path()).unwrap();
        let expected_pwd = ws.path().canonicalize().unwrap();
        assert!(
            result.contains(&expected_pwd.to_string_lossy().to_string()),
            "expected pwd output to contain {expected_pwd:?}, got: {result}"
        );
    }

    // ─── read ─────────────────────────────────────────────────────────────

    #[test]
    fn read_returns_file_content() {
        let ws = fresh_workspace();
        fs::write(ws.path().join("a.txt"), b"file content").unwrap();
        let raw = serde_json::json!({"path": "a.txt"}).to_string();
        let result = execute_read(&raw, ws.path()).unwrap();
        assert_eq!(result, "file content");
    }

    #[test]
    fn read_rejects_escape() {
        let ws = fresh_workspace();
        let raw = serde_json::json!({"path": "../oops.txt"}).to_string();
        let err = execute_read(&raw, ws.path()).unwrap_err();
        // either the canonicalize fails (file doesn't exist outside)
        // or our prefix check rejects. Either is fine — we just want
        // a non-Ok result.
        assert!(err.to_string().contains("escapes workspace") || err.to_string().contains("resolving"));
    }

    #[test]
    fn read_rejects_absolute_outside() {
        let ws = fresh_workspace();
        let raw = serde_json::json!({"path": "/etc/hostname"}).to_string();
        let result = dispatch_inside_workspace("read", &raw, ws.path());
        // Either fails at resolve (returns error string) or at file read
        assert!(
            result.contains("escapes workspace") || result.contains("error"),
            "expected error, got: {result}"
        );
    }

    // ─── write ────────────────────────────────────────────────────────────

    #[test]
    fn write_creates_file_in_workspace() {
        let ws = fresh_workspace();
        let raw = serde_json::json!({"path": "out.txt", "content": "hello"}).to_string();
        let result = execute_write(&raw, ws.path()).unwrap();
        assert!(result.contains("Wrote 5 bytes"));
        let written = fs::read_to_string(ws.path().join("out.txt")).unwrap();
        assert_eq!(written, "hello");
    }

    #[test]
    fn write_rejects_escape() {
        let ws = fresh_workspace();
        let raw = serde_json::json!({"path": "../escape.txt", "content": "x"}).to_string();
        let err = execute_write(&raw, ws.path()).unwrap_err();
        assert!(err.to_string().contains("escapes workspace"));
    }

    #[test]
    fn write_overwrites_existing_file() {
        let ws = fresh_workspace();
        fs::write(ws.path().join("a.txt"), b"original").unwrap();
        let raw = serde_json::json!({"path": "a.txt", "content": "replaced"}).to_string();
        execute_write(&raw, ws.path()).unwrap();
        let written = fs::read_to_string(ws.path().join("a.txt")).unwrap();
        assert_eq!(written, "replaced");
    }

    // ─── edit ─────────────────────────────────────────────────────────────

    #[test]
    fn edit_replaces_unique_occurrence() {
        let ws = fresh_workspace();
        fs::write(ws.path().join("a.txt"), b"hello world").unwrap();
        let raw = serde_json::json!({
            "path": "a.txt",
            "old_string": "world",
            "new_string": "spike"
        })
        .to_string();
        let result = execute_edit(&raw, ws.path()).unwrap();
        assert!(result.contains("1 replacement"));
        let after = fs::read_to_string(ws.path().join("a.txt")).unwrap();
        assert_eq!(after, "hello spike");
    }

    #[test]
    fn edit_rejects_non_unique_without_replace_all() {
        let ws = fresh_workspace();
        fs::write(ws.path().join("a.txt"), b"foo foo foo").unwrap();
        let raw = serde_json::json!({
            "path": "a.txt",
            "old_string": "foo",
            "new_string": "bar"
        })
        .to_string();
        let err = execute_edit(&raw, ws.path()).unwrap_err();
        assert!(err.to_string().contains("appears 3 times"));
        // File unchanged
        let after = fs::read_to_string(ws.path().join("a.txt")).unwrap();
        assert_eq!(after, "foo foo foo");
    }

    #[test]
    fn edit_replace_all_replaces_every_occurrence() {
        let ws = fresh_workspace();
        fs::write(ws.path().join("a.txt"), b"foo foo foo").unwrap();
        let raw = serde_json::json!({
            "path": "a.txt",
            "old_string": "foo",
            "new_string": "bar",
            "replace_all": true
        })
        .to_string();
        let result = execute_edit(&raw, ws.path()).unwrap();
        assert!(result.contains("3 replacements"));
        let after = fs::read_to_string(ws.path().join("a.txt")).unwrap();
        assert_eq!(after, "bar bar bar");
    }

    #[test]
    fn edit_rejects_old_string_not_found() {
        let ws = fresh_workspace();
        fs::write(ws.path().join("a.txt"), b"hello world").unwrap();
        let raw = serde_json::json!({
            "path": "a.txt",
            "old_string": "missing",
            "new_string": "x"
        })
        .to_string();
        let err = execute_edit(&raw, ws.path()).unwrap_err();
        assert!(err.to_string().contains("not found"));
    }

    #[test]
    fn edit_rejects_identical_old_and_new() {
        let ws = fresh_workspace();
        fs::write(ws.path().join("a.txt"), b"hello").unwrap();
        let raw = serde_json::json!({
            "path": "a.txt",
            "old_string": "hello",
            "new_string": "hello"
        })
        .to_string();
        let err = execute_edit(&raw, ws.path()).unwrap_err();
        assert!(err.to_string().contains("identical"));
    }

    #[test]
    fn edit_rejects_empty_old_string() {
        let ws = fresh_workspace();
        fs::write(ws.path().join("a.txt"), b"hello").unwrap();
        let raw = serde_json::json!({
            "path": "a.txt",
            "old_string": "",
            "new_string": "x"
        })
        .to_string();
        let err = execute_edit(&raw, ws.path()).unwrap_err();
        assert!(err.to_string().contains("empty"));
    }

    #[test]
    fn edit_rejects_missing_file() {
        let ws = fresh_workspace();
        let raw = serde_json::json!({
            "path": "nonexistent.txt",
            "old_string": "x",
            "new_string": "y"
        })
        .to_string();
        let err = execute_edit(&raw, ws.path()).unwrap_err();
        // resolve_read fails at canonicalize when the file doesn't exist
        assert!(err.to_string().contains("resolving"));
    }

    // ─── dispatch (integration with tool resolution) ──────────────────────

    /// Convenience helper that lets the test specify a workspace root
    /// for tools that would otherwise hit the hardcoded /workspace.
    /// Phase 4 will plumb the workspace into the runtime properly;
    /// for now this just exercises the function logic in tests.
    fn dispatch_inside_workspace(name: &str, raw_args: &str, ws: &Path) -> String {
        match Tool::from_name(name) {
            Some(Tool::Read) => match execute_read(raw_args, ws) {
                Ok(s) => s,
                Err(e) => format!("tool '{name}' returned error: {e:#}"),
            },
            Some(Tool::Write) => match execute_write(raw_args, ws) {
                Ok(s) => s,
                Err(e) => format!("tool '{name}' returned error: {e:#}"),
            },
            Some(Tool::Bash) => match execute_bash(raw_args, ws) {
                Ok(s) => s,
                Err(e) => format!("tool '{name}' returned error: {e:#}"),
            },
            _ => dispatch(name, raw_args),
        }
    }
}
