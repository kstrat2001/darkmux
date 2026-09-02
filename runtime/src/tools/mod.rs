//! Agent tool implementations.
//!
//! Tools shipped in the runtime palette:
//!
//! - `echo`   — round-trip probe, retained for unit-test coverage only;
//!   NOT exposed in the dispatch palette — see `main.rs`
//! - `bash`   — run a bash command with cwd=/workspace
//! - `read`   — read a file from inside /workspace (requires offset+limit)
//! - `write`  — write a file to inside /workspace
//! - `edit`   — apply one or more targeted patches via `edits[]` array
//! - `search` — find a substring pattern in a file or directory tree
//!
//! The shape converged through an empirical-evaluation arc against the
//! canonical Article 2 long-agentic refresh-token QA workload. The
//! load-bearing findings: `edits[]` array shape on `edit` enables batch
//! reasoning; required `offset`/`limit` on `read` forces deliberate
//! region thinking; `search` complements read for locating-by-name.
//! Attempts to make `read` array-shaped (`regions[]`) broke the model's
//! ability to call it (70% serde error rate); some tools are too
//! canonical for restructuring. Full reasoning in lab notebook Beats
//! 27-29.
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
/// (#2268) The tool set is declared ONCE: this macro emits the enum,
/// `Tool::ALL`, `Tool::name`, and `Tool::from_name` from a single
/// `Variant => "wire_name"` list, so a tool cannot exist in one of them and
/// not the others. The first version of this fix kept a hand-typed `ALL`
/// beside the enum and a hand-typed count in a test; review proved that a
/// variant added to the enum and to every match arm — the edit a developer
/// is actually compelled to make — left `ALL` stale with the suite green.
/// Membership is structural now, not asserted. (`description` stays a plain
/// exhaustive match below: long strings, and the compiler already forces it.)
macro_rules! tools {
    ($($variant:ident => $wire:literal),+ $(,)?) => {
        #[derive(Debug, Clone, Copy)]
        pub enum Tool {
            $($variant),+
        }

        impl Tool {
            /// Every variant, in declaration order — the one list.
            pub const ALL: &'static [Tool] = &[$(Tool::$variant),+];

            /// The wire name the model calls this tool by.
            pub fn name(self) -> &'static str {
                match self {
                    $(Tool::$variant => $wire),+
                }
            }

            /// Resolve a wire name; `None` for a name that is not a tool.
            pub fn from_name(name: &str) -> Option<Self> {
                match name {
                    $($wire => Some(Tool::$variant),)+
                    _ => None,
                }
            }
        }
    };
}

tools! {
    Echo => "echo",
    Bash => "bash",
    Read => "read",
    Write => "write",
    Edit => "edit",
    Search => "search",
    ReportFinding => "report_finding",
}

impl Tool {
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
                "Read content from a file. You MUST specify both \
                 `offset` (1-indexed line number to start at) and \
                 `limit` (max lines to read; 0 = read entire file \
                 from offset to end).\n\
                 \n\
                 WHEN TO USE limit > 0 (preferred):\n\
                 - After a `search` match at `path:N:content`, read \
                 around it: offset=N-10, limit=30\n\
                 - You only need a known region (imports at top, a \
                 specific function): offset=1, limit=50, etc.\n\
                 - You want to peek at a file without pulling its \
                 entire content into context\n\
                 \n\
                 WHEN TO USE limit = 0 (read entire file):\n\
                 - You genuinely need the whole file (understanding a \
                 small utility module's full structure, working with \
                 a file you know is short)\n\
                 - You've already searched and confirmed there's no \
                 smaller region that answers your question\n\
                 \n\
                 Specifying offset and limit forces you to consider \
                 what you actually need before reading. Reading whole \
                 files when you only need a slice wastes context \
                 tokens. The response indicates whether the file was \
                 fully returned or truncated at limit. Paths must \
                 resolve inside the workspace. If you have multiple \
                 reads to perform in the same turn, emit them as \
                 multiple `read` tool_calls in one assistant response.\n\
                 \n\
                 Arguments: { path: string, offset: integer (>=1), limit: integer (0 for full file, otherwise max lines) }."
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
                "Applies one or more targeted patches to an existing \
                 file in a single call. Each entry in `edits` replaces \
                 `old_string` with `new_string`. By default each edit's \
                 `old_string` must be unique in the file at the time it \
                 is applied — pass `replace_all: true` on an entry to \
                 replace every occurrence. The file must exist; paths \
                 must resolve inside the workspace. Edits apply in \
                 order against the current state (so later edits see \
                 the result of earlier ones in the same call). \
                 Prefer batching related changes into ONE call's \
                 `edits[]` array rather than emitting many edit calls — \
                 it is cheaper and the file is written atomically (if \
                 any edit in the batch fails, no write happens). \
                 Arguments: { path: string, edits: [{ old_string: string, new_string: string, replace_all?: bool }] }."
            }
            Tool::ReportFinding => {
                "Records ONE suspected issue you have found, then lets you keep \
                 working. Call it as soon as you find something rather than \
                 saving them up for the end — a run that is cut short keeps \
                 everything already reported. \
                 Arguments: { file: string, line: integer, pattern: string, \
                 evidence: string, why: string }. `evidence` MUST be the source \
                 line copied verbatim from the file, and `line` must be where it \
                 appears; a report whose evidence does not match that line is \
                 rejected and does not count. The return value tells you how many \
                 you have recorded and how many remain in this run's budget."
            }
            Tool::Search => {
                "FIRST CHOICE for locating text in a file or directory \
                 tree. Returns matching lines as \
                 `path:line_number:content`.\n\
                 \n\
                 DECISION RULE: if you would otherwise call `read` to \
                 scan a file for something specific (any string, name, \
                 phrase, identifier, or pattern you can name) — call \
                 `search` instead. Reading a whole file just to find \
                 one location is wasteful; search returns the location \
                 in one cheap call.\n\
                 \n\
                 USE search BEFORE read when:\n\
                 - You want to find where a name (function, variable, \
                 symbol, header, section) appears\n\
                 - You want to find a specific line to modify (an \
                 import, a config value, a setting)\n\
                 - You want to know which files in a directory mention \
                 a given string\n\
                 - The file you'd read is larger than a few hundred \
                 lines\n\
                 \n\
                 USE read INSTEAD OF search ONLY when:\n\
                 - You need the WHOLE file's content\n\
                 - You already know the file is small and have no \
                 specific target string\n\
                 \n\
                 The pattern is a LITERAL substring match (NOT a regex; \
                 special characters match literally; case-sensitive). \
                 Directory paths recurse and auto-skip dependency / \
                 build dirs (node_modules, dist, build, target, .git, \
                 etc.). Binary files are skipped silently. For \
                 multiple searches in one turn, emit multiple `search` \
                 tool_calls in one assistant response.\n\
                 \n\
                 Arguments: { pattern: string, path: string, max_results?: integer (default 50, max 500) }."
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
                    },
                    "offset": {
                        "type": "integer",
                        "description": "1-indexed line number to start reading at. Use 1 to start at the beginning of the file.",
                        "minimum": 1
                    },
                    "limit": {
                        "type": "integer",
                        "description": "Maximum lines to read. Use 0 to read the entire file from offset to the end. Prefer specifying a small number (e.g. 30-100) when you only need a region — reading whole files wastes context tokens if you only need a slice. The natural source of `offset` is a `search` match's line number.",
                        "minimum": 0
                    }
                },
                "required": ["path", "offset", "limit"]
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
            Tool::ReportFinding => serde_json::json!({
                "type": "object",
                "properties": {
                    "file": {
                        "type": "string",
                        "description": "Path of the file the issue is in, relative to the workspace root."
                    },
                    "line": {
                        "type": "integer",
                        "description": "1-indexed line number where the issue appears."
                    },
                    "pattern": {
                        "type": "string",
                        "description": "The named pattern you were asked to look for, copied exactly."
                    },
                    "evidence": {
                        "type": "string",
                        "description": "The source line at `line`, copied VERBATIM. Not a paraphrase, not a summary."
                    },
                    "why": {
                        "type": "string",
                        "description": "One or two sentences: why this line matches the pattern, and what it would cost."
                    }
                },
                "required": ["file", "line", "pattern", "evidence", "why"]
            }),
            Tool::Search => serde_json::json!({
                "type": "object",
                "properties": {
                    "pattern": {
                        "type": "string",
                        "description": "Literal substring to find. NOT a regex — special characters match literally. Case-sensitive."
                    },
                    "path": {
                        "type": "string",
                        "description": "File or directory to search inside. Absolute /workspace/... or workspace-relative. If a directory, search recurses (skipping hidden + common dependency dirs)."
                    },
                    "max_results": {
                        "type": "integer",
                        "description": "Optional cap on matches returned. Default 50, max 500.",
                        "minimum": 1,
                        "maximum": 500
                    }
                },
                "required": ["pattern", "path"]
            }),
            Tool::Edit => serde_json::json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "Path to edit. File must exist. Absolute /workspace/... or workspace-relative."
                    },
                    "edits": {
                        "type": "array",
                        "description": "One or more targeted replacements applied in order. Each edit operates on the current state of the file (the result of any prior edits in this same call). Prefer batching related changes into a single call rather than emitting many edit calls — it is cheaper and the file is written atomically (if any edit in the batch fails, no write happens).",
                        "items": {
                            "type": "object",
                            "properties": {
                                "old_string": {
                                    "type": "string",
                                    "description": "Text to replace. Must appear in the file at the time this edit is applied. Required to be unique unless replace_all=true."
                                },
                                "new_string": {
                                    "type": "string",
                                    "description": "Replacement text. Must differ from old_string."
                                },
                                "replace_all": {
                                    "type": "boolean",
                                    "description": "If true, replace ALL occurrences of old_string. Default false (require unique match)."
                                }
                            },
                            "required": ["old_string", "new_string"]
                        },
                        "minItems": 1
                    }
                },
                "required": ["path", "edits"]
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
            Tool::Search => execute_search(raw_args, Path::new(DEFAULT_WORKSPACE)),
            Tool::ReportFinding => execute_report_finding(
                raw_args,
                &crate::trajectory::runtime_dir(),
                Path::new(DEFAULT_WORKSPACE),
            ),
        }
    }

}

/// Dispatch a tool by name with raw JSON args. Returns the string the
/// runtime should put back into the conversation as a `role: tool`
/// message. Tool-execution errors are converted to a returned error
/// message string (not panics) so the model gets a chance to recover.
///
/// (#424) When the error came from argument parsing (the per-tool
/// `serde_json::from_str` failing with a "parsing X arguments"
/// context), the error message is enriched with the tool's
/// JSON-Schema. Without this enrichment the model sees a generic
/// parse failure and may retry with the same wrong shape; with the
/// schema in the result message the model has concrete signal about
/// what shape is expected. Reduces wasted-turn cycles where the
/// agent keeps emitting args that don't match the tool contract.
pub fn dispatch(name: &str, raw_args: &str) -> String {
    match Tool::from_name(name) {
        Some(tool) => match tool.execute(raw_args) {
            Ok(result) => result,
            Err(e) => format_dispatch_error(tool, name, &e),
        },
        None => format!(
            "tool '{name}' is not available in this runtime. \
             known tools: echo, bash, read, write, edit, search"
        ),
    }
}

/// (#424) Format a tool-dispatch error message. Preserves the
/// `"tool 'NAME' returned error:"` prefix that #419's failure-rate
/// detector matches against. For argument-parsing errors, appends
/// the tool's JSON-Schema so the model can correct its arg shape on
/// the next turn rather than blindly retrying.
///
/// **Detection safety**: the substring marker `"parsing {name}
/// arguments"` is bounded on BOTH sides by literal text from the
/// `with_context` calls in each `execute_*` function. Adding a new
/// tool with a name that's a substring of an existing tool's name
/// (e.g., `"bashlike"` vs `"bash"`) does NOT cause cross-pollution
/// because the trailing ` arguments` boundary forces an exact name
/// match — the marker `"parsing bash arguments"` does not appear in
/// the message `"parsing bashlike arguments"`. Adversarial / model-
/// supplied `raw_args` content can contain the marker text only for
/// the same tool's own marker (the `{name}` in the marker is the
/// dispatch-time tool name, not anything from `raw_args`), so the
/// only side effect is the same tool's schema being appended to a
/// non-parse error path — harmless.
fn format_dispatch_error(tool: Tool, name: &str, e: &anyhow::Error) -> String {
    let err_str = format!("{e:#}");
    let arg_parse_marker = format!("parsing {name} arguments");
    if err_str.contains(&arg_parse_marker) {
        let schema = tool.parameters_schema();
        let schema_text = serde_json::to_string_pretty(&schema)
            .unwrap_or_else(|_| schema.to_string());
        format!(
            "tool '{name}' returned error: {err_str}\n\n\
             EXPECTED argument schema for '{name}':\n{schema_text}\n\n\
             Correct the argument shape and try again."
        )
    } else {
        format!("tool '{name}' returned error: {err_str}")
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
    let shell = shell_for_commands();
    // (#905) Capture once: gates BOTH the wrapper choice and the TIMED-OUT
    // marker below, so a user command that happens to exit 124 isn't
    // mislabeled as a timeout when the wrapper was never used.
    let used_timeout = has_timeout_command();
    let output = if used_timeout {
        Command::new("timeout")
            .arg(format!("{timeout_secs}"))
            .arg(shell)
            .arg("-c")
            .arg(&args.command)
            .current_dir(workspace_root)
            .output()
    } else {
        Command::new(shell)
            .arg("-c")
            .arg(&args.command)
            .current_dir(workspace_root)
            .output()
    }
    .with_context(|| format!("spawning {shell} for: {}", args.command))?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let exit_code = output.status.code().unwrap_or(-1);

    let timed_out_marker = if used_timeout && exit_code == 124 {
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
    // (#905) Cache the probe — `execute_bash` calls this on every command and
    // the answer can't change within a dispatch; mirrors `shell_for_commands`.
    static HAS_TIMEOUT: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *HAS_TIMEOUT.get_or_init(|| {
        Command::new("sh")
            .arg("-c")
            .arg("command -v timeout >/dev/null 2>&1")
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    })
}

/// (#703 Slice 2) The shell to run agent `bash`-tool commands under. Prefer
/// `bash` (the default image ships it and most commands assume it), but fall
/// back to `sh` when bash isn't installed — so darkmux can inject into
/// bare-alpine / minimal operator images that ship only busybox `sh`. Probed
/// once via `sh` (which every Linux image has). The agent's tool is named
/// `bash` regardless; this is just which interpreter actually runs the string.
fn shell_for_commands() -> &'static str {
    use std::sync::OnceLock;
    static SHELL: OnceLock<&'static str> = OnceLock::new();
    SHELL.get_or_init(|| {
        let has_bash = Command::new("sh")
            .arg("-c")
            .arg("command -v bash >/dev/null 2>&1")
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if has_bash {
            "bash"
        } else {
            "sh"
        }
    })
}

// ─── read ─────────────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct ReadArgs {
    path: String,
    /// 1-indexed line number to start reading at.
    offset: u64,
    /// Max lines to return. `0` means "read everything from offset to
    /// EOF" — the explicit-opt-in escape hatch. Forcing offset and limit
    /// to be required parameters (rather than optional with defaults)
    /// is a deliberate Phase 6i design choice: the model has to think
    /// about what it actually needs before reading, and "I really need
    /// the whole file" becomes a conscious decision (limit=0) rather
    /// than the silent default.
    ///
    /// Phase 6l revert note: read does NOT take a `regions[]` array.
    /// Read is a "standard" tool whose canonical shape (path, offset,
    /// limit) is deeply burned into LLM training distributions; trying
    /// to restructure it into a nested array broke the model's ability
    /// to call it at all (70% serde error rate in Phase 6l). If the
    /// model needs multiple reads in the same turn, it can emit
    /// multiple `read` tool_calls in one assistant response — the
    /// OpenAI tool-calling format supports that natively.
    limit: u64,
}

fn execute_read(raw_args: &str, workspace_root: &Path) -> Result<String> {
    let args: ReadArgs = serde_json::from_str(raw_args)
        .with_context(|| format!("parsing read arguments: {raw_args}"))?;

    if args.offset < 1 {
        return Err(anyhow!("read: offset must be >= 1 (lines are 1-indexed)"));
    }

    let path = resolve_read(&args.path, workspace_root)?;

    // Read as bytes first so the 1MB safety cap still applies even
    // when limit=0. The cap is the last line of defense against a model
    // accidentally asking for an enormous binary; the offset/limit pair
    // is the first.
    let bytes = std::fs::read(&path)
        .with_context(|| format!("reading file: {path:?}"))?;

    if bytes.len() > READ_MAX_BYTES {
        let truncated = String::from_utf8_lossy(&bytes[..READ_MAX_BYTES]);
        return Ok(format!(
            "{}\n\n--- [byte safety cap fired; original file size {} bytes, truncated to {READ_MAX_BYTES}; offset/limit applied to truncated content] ---",
            slice_lines(&truncated, args.offset, args.limit),
            bytes.len()
        ));
    }

    let content = String::from_utf8_lossy(&bytes);
    let total_lines = content.lines().count() as u64;
    let (sliced, returned_lines, end_offset) =
        slice_lines_with_info(&content, args.offset, args.limit);

    let footer = if args.limit == 0 {
        format!(
            "\n\n--- [read entire file from offset {} ({total_lines} lines total)] ---",
            args.offset
        )
    } else if end_offset >= total_lines {
        format!(
            "\n\n--- [returned {returned_lines} lines; reached end of file ({total_lines} lines total)] ---"
        )
    } else {
        format!(
            "\n\n--- [returned {returned_lines} lines starting at offset {}; file has {total_lines} lines total; next region starts at offset {}] ---",
            args.offset,
            end_offset + 1
        )
    };

    Ok(format!("{sliced}{footer}"))
}

/// Slice `content` to lines [offset .. offset+limit). Returns just the
/// sliced text (no footer). `limit == 0` means "from offset to EOF".
fn slice_lines(content: &str, offset: u64, limit: u64) -> String {
    let start = offset.saturating_sub(1) as usize;
    let lines: Vec<&str> = content.lines().skip(start).collect();
    let take = if limit == 0 { lines.len() } else { limit as usize };
    lines.into_iter().take(take).collect::<Vec<_>>().join("\n")
}

/// Like `slice_lines` but also returns (lines_returned, end_offset).
/// end_offset is the 1-indexed line number of the last line returned
/// (so the next region begins at end_offset+1).
fn slice_lines_with_info(content: &str, offset: u64, limit: u64) -> (String, u64, u64) {
    let start = offset.saturating_sub(1) as usize;
    let all_lines: Vec<&str> = content.lines().skip(start).collect();
    let take = if limit == 0 { all_lines.len() } else { limit as usize };
    let kept: Vec<&str> = all_lines.into_iter().take(take).collect();
    let returned = kept.len() as u64;
    let end_offset = offset + returned.saturating_sub(1);
    (kept.join("\n"), returned, end_offset)
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
struct EditOp {
    old_string: String,
    new_string: String,
    #[serde(default)]
    replace_all: bool,
}

#[derive(Debug, Deserialize)]
struct EditArgs {
    path: String,
    edits: Vec<EditOp>,
}

fn execute_edit(raw_args: &str, workspace_root: &Path) -> Result<String> {
    let args: EditArgs = serde_json::from_str(raw_args)
        .with_context(|| format!("parsing edit arguments: {raw_args}"))?;

    if args.edits.is_empty() {
        return Err(anyhow!("edit: edits[] must contain at least one entry"));
    }

    // File must already exist (resolve_read enforces that).
    let path = resolve_read(&args.path, workspace_root)?;

    let original = std::fs::read_to_string(&path)
        .with_context(|| format!("reading file for edit: {path:?}"))?;

    // Apply all edits sequentially in memory. If any single edit fails
    // validation, the original file stays untouched — write is a single
    // atomic operation at the end.
    let mut content = original;
    let mut total_replacements: usize = 0;

    for (idx, op) in args.edits.iter().enumerate() {
        if op.old_string.is_empty() {
            return Err(anyhow!(
                "edit: edits[{idx}].old_string cannot be empty"
            ));
        }
        if op.old_string == op.new_string {
            return Err(anyhow!(
                "edit: edits[{idx}].old_string and new_string are identical — no change to apply"
            ));
        }

        let count = content.matches(&op.old_string).count();
        if count == 0 {
            return Err(anyhow!(
                "edit: edits[{idx}].old_string not found in {path:?} \
                 (checked against the current state after prior edits in this call). \
                 Did you mean to use `write` to create a new file?"
            ));
        }
        if count > 1 && !op.replace_all {
            return Err(anyhow!(
                "edit: edits[{idx}].old_string appears {count} times in {path:?}. \
                 Pass replace_all=true to replace every occurrence, or \
                 provide more surrounding context to make old_string unique."
            ));
        }

        let replacements = if op.replace_all { count } else { 1 };
        content = if op.replace_all {
            content.replace(&op.old_string, &op.new_string)
        } else {
            content.replacen(&op.old_string, &op.new_string, 1)
        };
        total_replacements += replacements;
    }

    std::fs::write(&path, content.as_bytes())
        .with_context(|| format!("writing edited file: {path:?}"))?;

    let edit_count = args.edits.len();
    Ok(format!(
        "Edited {} ({edit_count} edit{} applied; {total_replacements} replacement{} total)",
        path.display(),
        if edit_count == 1 { "" } else { "s" },
        if total_replacements == 1 { "" } else { "s" }
    ))
}

// ─── search ───────────────────────────────────────────────────────────────

/// Directories the recursive walk auto-skips. These are conventional
/// dependency / build-output / VCS dirs that almost never contain code
/// the operator wants to search and that hugely inflate result counts
/// if visited. Hidden directories (`.git`, `.cache`, anything starting
/// with `.`) are skipped via the leading-dot check, not this list.
const SEARCH_EXCLUDED_DIRS: &[&str] = &[
    "node_modules",
    "dist",
    "build",
    "target",
    "coverage",
];

/// Per-match line length cap. Lines longer than this are truncated with
/// a `...` marker so a single absurdly-long minified-JS line can't blow
/// the whole result payload.
const SEARCH_LINE_MAX_CHARS: usize = 200;

#[derive(Debug, Deserialize)]
struct SearchArgs {
    pattern: String,
    path: String,
    #[serde(default)]
    max_results: Option<usize>,
}

fn execute_search(raw_args: &str, workspace_root: &Path) -> Result<String> {
    let args: SearchArgs = serde_json::from_str(raw_args)
        .with_context(|| format!("parsing search arguments: {raw_args}"))?;

    if args.pattern.is_empty() {
        return Err(anyhow!("search: pattern cannot be empty"));
    }

    let max_results = args.max_results.unwrap_or(50).clamp(1, 500);

    let resolved = resolve_read(&args.path, workspace_root)?;
    let canonical_root = workspace_root
        .canonicalize()
        .with_context(|| format!("workspace root unavailable: {workspace_root:?}"))?;

    let meta = std::fs::symlink_metadata(&resolved)
        .with_context(|| format!("stat: {resolved:?}"))?;

    let mut hits: Vec<String> = Vec::new();
    if meta.is_file() {
        search_file(&resolved, &canonical_root, &args.pattern, &mut hits, max_results);
    } else if meta.is_dir() {
        search_dir(&resolved, &canonical_root, &args.pattern, &mut hits, max_results);
    } else {
        return Err(anyhow!(
            "search: path is neither a file nor a directory: {resolved:?}"
        ));
    }

    if hits.is_empty() {
        Ok(format!(
            "No matches for pattern {:?} in {}.",
            args.pattern, args.path
        ))
    } else {
        let capped_marker = if hits.len() >= max_results {
            format!("\n[capped at {max_results} matches; refine pattern or narrow path to see more]")
        } else {
            String::new()
        };
        Ok(format!("{}{}", hits.join("\n"), capped_marker))
    }
}

fn search_file(
    path: &Path,
    ws_root: &Path,
    pattern: &str,
    hits: &mut Vec<String>,
    max: usize,
) {
    if hits.len() >= max {
        return;
    }

    // Try to read as UTF-8. Binary / non-UTF8 files are silently skipped
    // (the model wanted text matches; returning mojibake helps nobody).
    let content = match std::fs::read_to_string(path) {
        Ok(c) => c,
        Err(_) => return,
    };

    let rel = path.strip_prefix(ws_root).unwrap_or(path);
    let rel_display = rel.display();

    for (idx, line) in content.lines().enumerate() {
        if hits.len() >= max {
            break;
        }
        if line.contains(pattern) {
            let line_str = if line.chars().count() > SEARCH_LINE_MAX_CHARS {
                let prefix: String =
                    line.chars().take(SEARCH_LINE_MAX_CHARS - 3).collect();
                format!("{prefix}...")
            } else {
                line.to_string()
            };
            hits.push(format!("{rel_display}:{}:{line_str}", idx + 1));
        }
    }
}

fn search_dir(dir: &Path, ws_root: &Path, pattern: &str, hits: &mut Vec<String>, max: usize) {
    if hits.len() >= max {
        return;
    }

    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return,
    };

    // Collect + sort for deterministic output. Bounded by directory size
    // (worst case: one big directory; we still cap by max_results below).
    let mut sorted: Vec<_> = entries.filter_map(|e| e.ok()).collect();
    sorted.sort_by_key(|e| e.file_name());

    for entry in sorted {
        if hits.len() >= max {
            break;
        }

        let name = entry.file_name();
        let name_str = name.to_string_lossy();

        // Skip hidden (any leading-dot name — covers .git, .cache, .next,
        // .env files we don't want to surface, etc.) and the excluded
        // dependency / build dirs.
        if name_str.starts_with('.') {
            continue;
        }
        if SEARCH_EXCLUDED_DIRS.contains(&name_str.as_ref()) {
            continue;
        }

        // symlink_metadata: does NOT follow symlinks. We use this to (a)
        // detect symlinks and skip them and (b) get the actual type of
        // non-symlink entries.
        let sym_meta = match std::fs::symlink_metadata(entry.path()) {
            Ok(m) => m,
            Err(_) => continue,
        };
        if sym_meta.file_type().is_symlink() {
            continue;
        }

        let path = entry.path();
        if sym_meta.is_dir() {
            search_dir(&path, ws_root, pattern, hits, max);
        } else if sym_meta.is_file() {
            search_file(&path, ws_root, pattern, hits, max);
        }
    }
}

#[cfg(test)]
mod tests {
    // (#2268) `Tool::ALL`, `name`, and `from_name` are generated from one
    // list by the `tools!` macro, so membership cannot drift and a round-trip
    // through `from_name` is true by construction (review round 3: such a
    // test pinned only the one literal it named). What a generated list CAN
    // still get wrong is the WIRE NAMES themselves — a typo, a duplicate, a
    // reorder — and those are the model-facing contract. So: a golden of the
    // whole set, in order. Adding a tool is a deliberate edit here too.
    #[test]
    fn the_wire_name_set_is_exactly_this() {
        let names: Vec<&str> = super::Tool::ALL.iter().map(|t| t.name()).collect();
        assert_eq!(names, ["echo", "bash", "read", "write", "edit", "search", "report_finding"]);
        // and no two variants share a wire name (a duplicate would also be an
        // unreachable-pattern error under -D warnings in from_name)
        let mut sorted = names.clone();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.len(), names.len(), "duplicate wire name in {names:?}");
    }


    use super::*;
    use std::fs;

    fn fresh_workspace() -> tempfile::TempDir {
        tempfile::Builder::new().prefix("darkmux-runtime-tools-test").tempdir().expect("create tempdir")
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

    // ─── report_finding: harness-captured context (#1959) ─────────────────

    // The tier that PRODUCES candidates must not also author what the tier
    // that JUDGES them gets to see. These assert the runtime reads the source
    // itself, so a crawler cannot hand its judge a context that flatters its
    // own finding.
    fn finding_workspace(body: &str) -> tempfile::TempDir {
        let ws = fresh_workspace();
        std::fs::create_dir_all(ws.path().join("src")).unwrap();
        std::fs::write(ws.path().join("src/a.rs"), body).unwrap();
        ws
    }

    fn numbered_src(n: usize, hit_line: usize, hit: &str) -> String {
        (1..=n)
            .map(|i| if i == hit_line { hit.to_string() } else { format!("// filler {i}") })
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn last_finding(out: &Path) -> serde_json::Value {
        let body = std::fs::read_to_string(out.join(FINDINGS_FILE)).expect("findings file");
        let line = body.lines().rfind(|l| !l.trim().is_empty()).expect("a record");
        serde_json::from_str(line).expect("valid json")
    }

    fn report(ws: &Path, out: &Path, line: u32, evidence: &str) -> String {
        let raw = serde_json::json!({
            "file": "src/a.rs", "line": line, "pattern": "p",
            "evidence": evidence, "why": "w",
        })
        .to_string();
        execute_report_finding(&raw, out, ws).unwrap()
    }

    #[test]
    fn the_runtime_reads_the_context_off_disk_not_from_the_model() {
        let ws = finding_workspace(&numbered_src(100, 50, "    let _ = risky();"));
        let out = fresh_workspace();
        report(ws.path(), out.path(), 50, "    let _ = risky();");

        let rec = last_finding(out.path());
        let ctx = rec["context"].as_str().expect("context recorded");
        assert!(ctx.contains("let _ = risky();"), "the cited line must be in the window: {ctx}");
        assert!(ctx.contains("filler 20"), "window must reach 30 lines BEFORE: {ctx}");
        assert!(ctx.contains("filler 80"), "window must reach 30 lines AFTER: {ctx}");
        assert!(!ctx.contains("filler 19"), "window must not exceed 30 before");
        assert!(!ctx.contains("filler 81"), "window must not exceed 30 after");
        assert_eq!(rec["context_start"], 20);
        assert_eq!(rec["context_end"], 80);
    }

    #[test]
    fn a_quote_that_disagrees_with_the_cited_line_is_rejected_not_silently_corrected() {
        // The realistic cause is a WRONG LINE NUMBER, not sloppy transcription.
        // Recording the file's version anyway would attach evidence the model
        // never examined to a `why` describing different code — a coherent-
        // looking record that is wrong, which is worse than no record.
        let ws = finding_workspace(&numbered_src(60, 30, "    let _ = actual();"));
        let out = fresh_workspace();
        let resp = report(ws.path(), out.path(), 30, "    let _ = what_the_model_claimed();");

        assert!(resp.starts_with("REJECTED:"), "a mismatched quote must not be recorded: {resp}");
        assert!(
            resp.contains("let _ = actual();"),
            "the response must show what IS there, so the model can fix its line number: {resp}"
        );
        assert!(resp.contains("did not count against your budget"), "{resp}");
        assert!(
            !out.path().join(FINDINGS_FILE).exists(),
            "nothing may be recorded for a rejected citation"
        );
    }

    #[test]
    fn whitespace_alone_does_not_count_as_a_mismatch() {
        // Rejecting on indentation would make the guard hostile rather than
        // useful — the model is quoting from a rendered `read` result.
        let ws = finding_workspace(&numbered_src(60, 30, "    let _ = actual();"));
        let out = fresh_workspace();
        let resp = report(ws.path(), out.path(), 30, "let _ = actual();");
        assert!(!resp.starts_with("REJECTED:"), "trimmed-equal quotes must pass: {resp}");
        assert_eq!(last_finding(out.path())["evidence"], "    let _ = actual();");
    }

    #[test]
    fn an_honest_quote_is_recorded_with_the_file_s_own_line() {
        let ws = finding_workspace(&numbered_src(60, 30, "    let _ = actual();"));
        let out = fresh_workspace();
        report(ws.path(), out.path(), 30, "    let _ = actual();");
        let rec = last_finding(out.path());
        assert_eq!(
            rec["evidence"], "    let _ = actual();",
            "evidence must come from disk, or 'cite the line' is a check rather than a guarantee"
        );
    }

    #[test]
    fn a_line_past_the_end_of_the_file_is_rejected_and_records_nothing() {
        let ws = finding_workspace(&numbered_src(40, 10, "    let _ = risky();"));
        let out = fresh_workspace();
        let resp = report(ws.path(), out.path(), 9999, "    let _ = risky();");

        assert!(resp.starts_with("REJECTED:"), "an unresolvable citation must not be recorded: {resp}");
        assert!(resp.contains("40 lines"), "the reason must name what the file actually has: {resp}");
        assert!(
            resp.contains("did not count against your budget"),
            "a rejected citation must not consume budget: {resp}"
        );
        assert!(
            !out.path().join(FINDINGS_FILE).exists(),
            "the findings file must not even be created by a rejected call"
        );
    }

    #[test]
    fn a_file_outside_the_workspace_is_rejected() {
        let ws = finding_workspace(&numbered_src(40, 10, "    let _ = risky();"));
        let out = fresh_workspace();
        let raw = serde_json::json!({
            "file": "../../../etc/passwd", "line": 1, "pattern": "p",
            "evidence": "root", "why": "w",
        })
        .to_string();
        let resp = execute_report_finding(&raw, out.path(), ws.path()).unwrap();
        assert!(resp.starts_with("REJECTED:"), "context capture must not escape the workspace: {resp}");
    }

    #[test]
    fn a_window_at_the_start_of_a_file_clamps_instead_of_underflowing() {
        let ws = finding_workspace(&numbered_src(80, 2, "    let _ = risky();"));
        let out = fresh_workspace();
        report(ws.path(), out.path(), 2, "    let _ = risky();");
        let rec = last_finding(out.path());
        assert_eq!(rec["context_start"], 1, "must clamp to the first line, not wrap or panic");
        assert_eq!(rec["context_end"], 32);
    }

    // ─── read ─────────────────────────────────────────────────────────────

    #[test]
    fn read_returns_full_file_when_limit_zero() {
        let ws = fresh_workspace();
        fs::write(ws.path().join("a.txt"), b"line one\nline two\nline three").unwrap();
        let raw =
            serde_json::json!({"path": "a.txt", "offset": 1, "limit": 0}).to_string();
        let result = execute_read(&raw, ws.path()).unwrap();
        assert!(result.starts_with("line one\nline two\nline three"));
        assert!(result.contains("read entire file"));
        assert!(result.contains("3 lines total"));
    }

    #[test]
    fn read_with_limit_returns_partial_slice() {
        let ws = fresh_workspace();
        let content: String = (1..=20).map(|i| format!("line {i}\n")).collect();
        fs::write(ws.path().join("a.txt"), content).unwrap();
        let raw =
            serde_json::json!({"path": "a.txt", "offset": 1, "limit": 5}).to_string();
        let result = execute_read(&raw, ws.path()).unwrap();
        assert!(result.contains("line 1"));
        assert!(result.contains("line 5"));
        assert!(!result.contains("line 6"));
        assert!(result.contains("next region starts at offset 6"));
        assert!(result.contains("20 lines total"));
    }

    #[test]
    fn read_with_offset_skips_leading_lines() {
        let ws = fresh_workspace();
        let content: String = (1..=20).map(|i| format!("line {i}\n")).collect();
        fs::write(ws.path().join("a.txt"), content).unwrap();
        let raw =
            serde_json::json!({"path": "a.txt", "offset": 10, "limit": 5}).to_string();
        let result = execute_read(&raw, ws.path()).unwrap();
        assert!(!result.contains("line 1\n"));
        assert!(!result.contains("line 9\n"));
        assert!(result.contains("line 10"));
        assert!(result.contains("line 14"));
        assert!(!result.contains("line 15\n"));
        assert!(result.contains("next region starts at offset 15"));
    }

    #[test]
    fn read_with_limit_beyond_eof_reports_end_reached() {
        let ws = fresh_workspace();
        fs::write(ws.path().join("a.txt"), b"only-line\n").unwrap();
        let raw =
            serde_json::json!({"path": "a.txt", "offset": 1, "limit": 100}).to_string();
        let result = execute_read(&raw, ws.path()).unwrap();
        assert!(result.contains("only-line"));
        assert!(result.contains("reached end of file"));
    }

    #[test]
    fn read_rejects_missing_offset_or_limit() {
        let ws = fresh_workspace();
        fs::write(ws.path().join("a.txt"), b"hello").unwrap();
        let raw = serde_json::json!({"path": "a.txt"}).to_string();
        let err = execute_read(&raw, ws.path()).unwrap_err();
        let chained = format!("{err:#}");
        assert!(
            chained.contains("offset") || chained.contains("missing"),
            "expected required-field error, got chained: {chained}"
        );
    }

    #[test]
    fn read_rejects_offset_zero() {
        let ws = fresh_workspace();
        fs::write(ws.path().join("a.txt"), b"hello").unwrap();
        let raw =
            serde_json::json!({"path": "a.txt", "offset": 0, "limit": 0}).to_string();
        let err = execute_read(&raw, ws.path()).unwrap_err();
        assert!(err.to_string().contains("offset must be >= 1"));
    }

    #[test]
    fn read_rejects_escape() {
        let ws = fresh_workspace();
        let raw =
            serde_json::json!({"path": "../oops.txt", "offset": 1, "limit": 0}).to_string();
        let err = execute_read(&raw, ws.path()).unwrap_err();
        assert!(err.to_string().contains("escapes workspace") || err.to_string().contains("resolving"));
    }

    #[test]
    fn read_rejects_absolute_outside() {
        let ws = fresh_workspace();
        let raw = serde_json::json!({"path": "/etc/hostname", "offset": 1, "limit": 0}).to_string();
        let result = dispatch_inside_workspace("read", &raw, ws.path());
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
    fn edit_single_replaces_unique_occurrence() {
        let ws = fresh_workspace();
        fs::write(ws.path().join("a.txt"), b"hello world").unwrap();
        let raw = serde_json::json!({
            "path": "a.txt",
            "edits": [{"old_string": "world", "new_string": "spike"}]
        })
        .to_string();
        let result = execute_edit(&raw, ws.path()).unwrap();
        assert!(result.contains("1 edit applied"));
        assert!(result.contains("1 replacement total"));
        let after = fs::read_to_string(ws.path().join("a.txt")).unwrap();
        assert_eq!(after, "hello spike");
    }

    #[test]
    fn edit_rejects_non_unique_without_replace_all() {
        let ws = fresh_workspace();
        fs::write(ws.path().join("a.txt"), b"foo foo foo").unwrap();
        let raw = serde_json::json!({
            "path": "a.txt",
            "edits": [{"old_string": "foo", "new_string": "bar"}]
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
            "edits": [{"old_string": "foo", "new_string": "bar", "replace_all": true}]
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
            "edits": [{"old_string": "missing", "new_string": "x"}]
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
            "edits": [{"old_string": "hello", "new_string": "hello"}]
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
            "edits": [{"old_string": "", "new_string": "x"}]
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
            "edits": [{"old_string": "x", "new_string": "y"}]
        })
        .to_string();
        let err = execute_edit(&raw, ws.path()).unwrap_err();
        // resolve_read fails at canonicalize when the file doesn't exist
        assert!(err.to_string().contains("resolving"));
    }

    #[test]
    fn edit_rejects_empty_edits_array() {
        let ws = fresh_workspace();
        fs::write(ws.path().join("a.txt"), b"hello").unwrap();
        let raw = serde_json::json!({"path": "a.txt", "edits": []}).to_string();
        let err = execute_edit(&raw, ws.path()).unwrap_err();
        assert!(err.to_string().contains("at least one"));
    }

    #[test]
    fn edit_batch_applies_multiple_independent_edits() {
        let ws = fresh_workspace();
        fs::write(ws.path().join("a.txt"), b"alpha beta gamma delta").unwrap();
        let raw = serde_json::json!({
            "path": "a.txt",
            "edits": [
                {"old_string": "alpha", "new_string": "ALPHA"},
                {"old_string": "gamma", "new_string": "GAMMA"},
                {"old_string": "delta", "new_string": "DELTA"}
            ]
        })
        .to_string();
        let result = execute_edit(&raw, ws.path()).unwrap();
        assert!(result.contains("3 edits applied"));
        assert!(result.contains("3 replacements total"));
        let after = fs::read_to_string(ws.path().join("a.txt")).unwrap();
        assert_eq!(after, "ALPHA beta GAMMA DELTA");
    }

    #[test]
    fn edit_batch_later_edit_sees_earlier_edit_result() {
        let ws = fresh_workspace();
        fs::write(ws.path().join("a.txt"), b"hello world").unwrap();
        // First edit produces "hello spike"; second edit operates on
        // that result and changes "spike" to "rocket". This verifies
        // the "applied against current state" contract.
        let raw = serde_json::json!({
            "path": "a.txt",
            "edits": [
                {"old_string": "world", "new_string": "spike"},
                {"old_string": "spike", "new_string": "rocket"}
            ]
        })
        .to_string();
        let result = execute_edit(&raw, ws.path()).unwrap();
        assert!(result.contains("2 edits applied"));
        let after = fs::read_to_string(ws.path().join("a.txt")).unwrap();
        assert_eq!(after, "hello rocket");
    }

    #[test]
    fn edit_batch_is_atomic_on_failure() {
        let ws = fresh_workspace();
        fs::write(ws.path().join("a.txt"), b"hello world").unwrap();
        // Second edit's old_string doesn't exist — the whole call must
        // fail without modifying the file.
        let raw = serde_json::json!({
            "path": "a.txt",
            "edits": [
                {"old_string": "hello", "new_string": "GOODBYE"},
                {"old_string": "missing-text", "new_string": "x"}
            ]
        })
        .to_string();
        let err = execute_edit(&raw, ws.path()).unwrap_err();
        assert!(err.to_string().contains("edits[1]"));
        assert!(err.to_string().contains("not found"));
        // File MUST be unchanged — the first edit's in-memory mutation
        // never reached disk.
        let after = fs::read_to_string(ws.path().join("a.txt")).unwrap();
        assert_eq!(after, "hello world");
    }

    // ─── search ───────────────────────────────────────────────────────────

    #[test]
    fn search_finds_pattern_in_single_file() {
        let ws = fresh_workspace();
        fs::write(
            ws.path().join("a.txt"),
            b"alpha\nbeta\ngamma\nalpha again\n",
        )
        .unwrap();
        let raw = serde_json::json!({"pattern": "alpha", "path": "a.txt"}).to_string();
        let result = execute_search(&raw, ws.path()).unwrap();
        assert!(result.contains("a.txt:1:alpha"));
        assert!(result.contains("a.txt:4:alpha again"));
        assert!(!result.contains("beta"));
        assert!(!result.contains("gamma"));
    }

    #[test]
    fn search_finds_pattern_in_directory_tree() {
        let ws = fresh_workspace();
        fs::create_dir(ws.path().join("sub")).unwrap();
        fs::write(ws.path().join("sub/a.ts"), b"foo\nbar\n").unwrap();
        fs::write(ws.path().join("sub/b.ts"), b"foo\nbaz\n").unwrap();
        let raw = serde_json::json!({"pattern": "foo", "path": "sub"}).to_string();
        let result = execute_search(&raw, ws.path()).unwrap();
        assert!(result.contains("a.ts:1:foo"));
        assert!(result.contains("b.ts:1:foo"));
    }

    #[test]
    fn search_skips_excluded_dirs() {
        let ws = fresh_workspace();
        fs::create_dir(ws.path().join("node_modules")).unwrap();
        fs::write(ws.path().join("node_modules/dep.js"), b"needle").unwrap();
        fs::write(ws.path().join("real.js"), b"needle").unwrap();
        let raw = serde_json::json!({"pattern": "needle", "path": "."}).to_string();
        let result = execute_search(&raw, ws.path()).unwrap();
        assert!(result.contains("real.js"));
        assert!(
            !result.contains("node_modules"),
            "node_modules content leaked: {result}"
        );
    }

    #[test]
    fn search_skips_hidden_dirs() {
        let ws = fresh_workspace();
        fs::create_dir(ws.path().join(".secret")).unwrap();
        fs::write(ws.path().join(".secret/a.txt"), b"needle").unwrap();
        fs::write(ws.path().join("visible.txt"), b"needle").unwrap();
        let raw = serde_json::json!({"pattern": "needle", "path": "."}).to_string();
        let result = execute_search(&raw, ws.path()).unwrap();
        assert!(result.contains("visible.txt"));
        assert!(!result.contains(".secret"));
    }

    #[test]
    fn search_skips_symlinks_during_recursion() {
        use std::os::unix::fs::symlink;
        let ws = fresh_workspace();
        let outside = ws.path().parent().unwrap().join("search-symlink-target");
        let _ = fs::create_dir_all(&outside);
        fs::write(outside.join("secret.txt"), b"needle in outside dir").unwrap();
        symlink(&outside, ws.path().join("leak")).unwrap();

        let raw = serde_json::json!({"pattern": "needle", "path": "."}).to_string();
        let result = execute_search(&raw, ws.path()).unwrap();
        assert!(
            result.contains("No matches"),
            "symlink was followed (security regression): {result}"
        );

        let _ = fs::remove_dir_all(&outside);
    }

    #[test]
    fn search_caps_at_max_results() {
        let ws = fresh_workspace();
        let content: String = (0..100).map(|i| format!("match line {i}\n")).collect();
        fs::write(ws.path().join("a.txt"), content).unwrap();
        let raw = serde_json::json!({
            "pattern": "match", "path": "a.txt", "max_results": 5
        })
        .to_string();
        let result = execute_search(&raw, ws.path()).unwrap();
        let hit_count = result.matches("a.txt:").count();
        assert_eq!(hit_count, 5, "expected 5 matches, got: {result}");
        assert!(result.contains("capped at 5"));
    }

    #[test]
    fn search_rejects_empty_pattern() {
        let ws = fresh_workspace();
        fs::write(ws.path().join("a.txt"), b"hello").unwrap();
        let raw = serde_json::json!({"pattern": "", "path": "a.txt"}).to_string();
        let err = execute_search(&raw, ws.path()).unwrap_err();
        assert!(err.to_string().contains("empty"));
    }

    #[test]
    fn search_rejects_path_outside_workspace() {
        let ws = fresh_workspace();
        let raw = serde_json::json!({"pattern": "x", "path": "/etc/hosts"}).to_string();
        let err = execute_search(&raw, ws.path()).unwrap_err();
        assert!(err.to_string().contains("escapes workspace"));
    }

    #[test]
    fn search_returns_no_match_message() {
        let ws = fresh_workspace();
        fs::write(ws.path().join("a.txt"), b"hello world").unwrap();
        let raw = serde_json::json!({"pattern": "absent", "path": "a.txt"}).to_string();
        let result = execute_search(&raw, ws.path()).unwrap();
        assert!(result.contains("No matches"));
    }

    #[test]
    fn search_truncates_overlong_lines() {
        let ws = fresh_workspace();
        let long_line = format!("{}needle\n", "x".repeat(300));
        fs::write(ws.path().join("a.txt"), long_line).unwrap();
        let raw = serde_json::json!({"pattern": "needle", "path": "a.txt"}).to_string();
        let result = execute_search(&raw, ws.path()).unwrap();
        assert!(
            result.contains("..."),
            "expected truncation marker, got: {result}"
        );
    }

    #[test]
    fn search_skips_binary_files() {
        let ws = fresh_workspace();
        let mut bytes = vec![0xFF, 0xFE, 0x00, 0x01];
        bytes.extend_from_slice(b"needle in binary");
        fs::write(ws.path().join("binary.bin"), bytes).unwrap();
        fs::write(ws.path().join("text.txt"), b"needle in text\n").unwrap();
        let raw = serde_json::json!({"pattern": "needle", "path": "."}).to_string();
        let result = execute_search(&raw, ws.path()).unwrap();
        assert!(result.contains("text.txt"));
        assert!(!result.contains("binary.bin"));
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

    // ─── #424: tool-argument pre-flight ───────────────────────────────────

    #[test]
    fn dispatch_arg_parse_error_message_includes_schema_for_bash() {
        // Malformed JSON: `command` field missing. dispatch should
        // return an error string that includes the bash JSON-Schema
        // so the model can correct on the next turn.
        let result = dispatch("bash", r#"{"not_command": "ls"}"#);
        assert!(
            result.contains("EXPECTED argument schema for 'bash'"),
            "arg-parse error must include schema-augmentation header. Got: {result}"
        );
        assert!(
            result.contains("\"command\""),
            "schema must mention the expected `command` field. Got: {result}"
        );
        // Preserves the #419 failure-rate-detection prefix.
        assert!(
            result.starts_with("tool 'bash' returned error:"),
            "must preserve the failure-marker prefix #419 depends on. Got: {result}"
        );
    }

    #[test]
    fn dispatch_arg_parse_error_message_includes_schema_for_read() {
        // Read requires `path`, `offset`, `limit`. Sending none of them.
        let result = dispatch("read", r#"{}"#);
        assert!(result.contains("EXPECTED argument schema for 'read'"));
        assert!(result.contains("\"path\""));
        assert!(result.contains("\"offset\""));
        assert!(result.contains("\"limit\""));
        assert!(result.starts_with("tool 'read' returned error:"));
    }

    #[test]
    fn dispatch_arg_parse_error_message_includes_schema_for_edit() {
        let result = dispatch("edit", r#"{"path": "/x"}"#); // missing edits[]
        assert!(result.contains("EXPECTED argument schema for 'edit'"));
        assert!(result.contains("\"edits\""));
    }

    #[test]
    fn dispatch_non_arg_parse_error_does_not_add_schema() {
        // A non-arg-parse error path — `read` with a well-shaped
        // args object but a path that doesn't exist in the test
        // environment. The arg-parser succeeds; the error fires
        // downstream (file IO). Should still get wrapped with the
        // "tool 'NAME' returned error:" prefix but NOT augmented
        // with the schema — schema enrichment is specific to the
        // arg-parsing failure mode #424 targets.
        //
        // Using `read` with a definitely-nonexistent path rather
        // than `bash`-can't-spawn-in-/workspace because the latter
        // is environment-dependent (silently becomes a different
        // test if execute_bash ever tolerates a missing cwd).
        let result = dispatch(
            "read",
            r#"{"path": "/definitely/not/a/real/path", "offset": 1, "limit": 0}"#,
        );
        assert!(
            result.starts_with("tool 'read' returned error:"),
            "non-arg-parse error must keep the wrapper prefix. Got: {result}"
        );
        assert!(
            !result.contains("EXPECTED argument schema"),
            "schema augmentation must only fire on arg-parse errors. Got: {result}"
        );
    }

    #[test]
    fn dispatch_successful_call_returns_unaugmented_result() {
        // No error, no schema in output.
        let result = dispatch("echo", r#"{"text": "hi"}"#);
        assert_eq!(result, "hi");
        assert!(!result.contains("EXPECTED argument schema"));
    }

    #[test]
    fn dispatch_unknown_tool_does_not_add_schema() {
        // Unknown tools never had a schema-aware path — message
        // shape preserved.
        let result = dispatch("nonexistent_tool", r#"{}"#);
        assert!(result.contains("not available"));
        assert!(!result.contains("EXPECTED argument schema"));
    }

    #[test]
    fn dispatch_arg_parse_error_includes_underlying_serde_error() {
        // Should preserve serde's specific error message so the model
        // gets BOTH "what was wrong" and "what's expected" in one
        // result.
        let result = dispatch("echo", r#"{"wrong_field": "hi"}"#);
        // serde error names the missing field
        assert!(result.contains("text"), "must mention the expected field name. Got: {result}");
        // schema also included
        assert!(result.contains("EXPECTED argument schema for 'echo'"));
    }
}

// ─── report_finding ───────────────────────────────────────────────────────
//
// (#1959) The crawler's output channel, and a DIFFERENT shape from escalation.
//
//   escalation  harness-produced, terminal, "I cannot proceed"      0 or 1
//   finding     MODEL-produced, mid-run, "here is an artifact"      0..N
//
// A finding is a CLAIM, unconfirmed by construction — which is exactly why it
// needs to be structured rather than narrated. A tool call is structured by
// construction; prose has to be parsed, and parsing model prose for structure
// is the same class of mistake as searching for a `</think>` the model may
// never emit.
//
// Findings append to their own file beside the trajectory rather than routing
// through it. Three things fall out for free: the run's accumulation file IS
// this file, a killed run keeps every finding already reported, and the tool
// needs no shared state — the line count IS the count.

/// Hard ceiling per dispatch. A crawler reporting more than this from one
/// scope is not being thorough, it is pattern-matching noise — so the cap is
/// also a signal, not just a limit (see the returned `budget_remaining`, and
/// the finding-rate invariant in #1956).
const MAX_FINDINGS_PER_DISPATCH: usize = 40;

/// (#1959) Lines of source captured either side of a finding's cited line.
///
/// Sized from a measured triage pass: every one of ten findings on the first
/// real corpus was judgeable inside +/-30, with zero NEEDS-WIDER-CONTEXT
/// verdicts, including one whose severity turned on code ~20 lines BELOW the
/// cited line. Widen only against evidence that a real judgment needed more.
const FINDING_CONTEXT_LINES: usize = 30;

/// The harness's own view of a cited line and its surroundings.
///
/// **Read from disk by the runtime, never supplied by the model.** That is the
/// whole point: the triage tier downstream judges real source rather than the
/// crawler's account of it, so the adversarial boundary between the tier that
/// PRODUCES candidates and the tier that JUDGES them survives the hand-off. A
/// model that could write its own context could also write a context that
/// justifies its own finding.
///
/// It also makes "cite the line" true by construction instead of by check: the
/// recorded `evidence` IS the file's line, so the post-hoc guard that compares
/// them can only ever pass. What that guard becomes instead is a MODEL-QUALITY
/// signal — `evidence_mismatch` records that the crawler misquoted, which is
/// worth surfacing and is not the same thing as an invalid finding.
struct FindingContext {
    evidence: String,
    context: String,
    start: usize,
    end: usize,
    mismatch: bool,
}

/// Resolve the cited file, verify the line exists, and capture the window.
///
/// Returns `Err` with a model-facing reason when the citation does not resolve —
/// the caller turns that into a REJECTED response that costs no budget. This is
/// the mechanical evidence guard moved to the point of REPORT, so a finding that
/// cannot point at code never reaches an artifact, let alone a frontier token.
fn capture_finding_context(
    file: &str,
    line: u32,
    claimed: &str,
    workspace_root: &Path,
) -> std::result::Result<FindingContext, String> {
    let path = resolve_read(file, workspace_root)
        .map_err(|e| format!("`file` did not resolve to a readable path in the workspace ({e})"))?;
    let body = std::fs::read_to_string(&path)
        .map_err(|e| format!("`file` could not be read ({e})"))?;
    let lines: Vec<&str> = body.lines().collect();
    let n = line as usize;
    if n > lines.len() {
        return Err(format!(
            "`line` {n} is past the end of that file, which has {} lines",
            lines.len()
        ));
    }
    let start = n.saturating_sub(FINDING_CONTEXT_LINES).max(1);
    let end = (n + FINDING_CONTEXT_LINES).min(lines.len());
    // Numbered, so a downstream judge can cite precisely from the window alone
    // without re-deriving offsets.
    let context = lines[start - 1..end]
        .iter()
        .enumerate()
        .map(|(i, l)| format!("{:>6}  {}", start + i, l))
        .collect::<Vec<_>>()
        .join("\n");
    let evidence = lines[n - 1].to_string();
    let mismatch = !claimed.trim().is_empty() && claimed.trim() != evidence.trim();
    Ok(FindingContext { evidence, context, start, end, mismatch })
}

pub const FINDINGS_FILE: &str = "findings.jsonl";

/// Argument names are ALIASED, deliberately.
///
/// Measured on the first live crawl: a model that had just made 13 `read`
/// calls filled this tool with `read`'s parameter names — `{path, offset}`
/// instead of `{file, line}`. It pattern-matched the neighbouring tool's
/// argument shape rather than reading this schema, which is a real
/// small-model failure mode and not something to prompt away.
///
/// Being liberal in what we accept costs one serde attribute and removes an
/// entire class of failure. The schema still ADVERTISES `file`/`line`; these
/// aliases just stop a near-miss from being a total loss.
#[derive(Debug, Deserialize)]
struct ReportFindingArgs {
    #[serde(alias = "path", alias = "file_path", alias = "filename")]
    file: String,
    #[serde(alias = "offset", alias = "line_number", alias = "lineno")]
    line: u32,
    #[serde(default)]
    pattern: String,
    #[serde(default, alias = "snippet", alias = "code", alias = "source")]
    evidence: String,
    #[serde(default, alias = "reason", alias = "detail", alias = "explanation")]
    why: String,
}

fn execute_report_finding(
    raw_args: &str,
    out_dir: &Path,
    workspace_root: &Path,
) -> Result<String> {
    // NEVER return Err from a model-facing tool.
    //
    // Measured on the first live crawl: one malformed call returned an error,
    // and the model concluded "the report_finding tool is not available in
    // this runtime" and abandoned the channel entirely for the rest of the
    // run — falling back to narrating findings in prose, where nothing could
    // record them. It never retried.
    //
    // A tool ERROR reads to a model as "this tool is broken or absent." A tool
    // RESPONSE reads as "try again, differently." So a malformed call gets a
    // teaching response showing exactly what a correct call looks like, and
    // the run continues.
    let args: ReportFindingArgs = match serde_json::from_str(raw_args) {
        Ok(a) => a,
        Err(e) => {
            return Ok(format!(
                "NOT RECORDED — I could not read those arguments ({e}). \
                 This tool takes exactly these five keys:\n\
                 \n  {{\"file\": \"crates/foo/src/bar.rs\", \"line\": 147, \
                 \"pattern\": \"<the pattern you were given>\", \
                 \"evidence\": \"<the source line, copied verbatim>\", \
                 \"why\": \"<one or two sentences>\"}}\n\
                 \nThe tool IS available — call it again with those keys. \
                 Nothing was counted against your budget."
            ));
        }
    };
    if args.evidence.trim().is_empty() && args.why.trim().is_empty() {
        return Ok("NOT RECORDED — `evidence` (the source line, copied verbatim) \
                   and `why` are both required. The tool IS available; call it again \
                   with those fields filled in. Nothing counted against your budget."
            .to_string());
    }

    // "Cite the line", enforced at the tool boundary rather than in triage.
    // A finding that cannot point at code is not a finding, and rejecting it
    // here costs nothing — no frontier token is ever spent on it.
    if args.evidence.trim().is_empty() {
        return Ok("REJECTED: `evidence` was empty. Copy the source line verbatim \
                   from the file and call again. This did not count against your budget."
            .to_string());
    }
    if args.line == 0 {
        return Ok("REJECTED: `line` must be the 1-indexed line where the evidence \
                   appears. This did not count against your budget."
            .to_string());
    }

    // Resolve the citation against real source BEFORE anything is recorded. A
    // finding that cannot point at code is not a finding, and rejecting it here
    // costs nothing — no artifact, no frontier token. The captured window rides
    // along so the triage tier never has to open the tree.
    let captured = match capture_finding_context(
        &args.file,
        args.line,
        &args.evidence,
        workspace_root,
    ) {
        Ok(c) => c,
        Err(reason) => {
            return Ok(format!(
                "REJECTED: {reason}. Re-check the path and the 1-indexed line, \
                 then call again. This did not count against your budget."
            ));
        }
    };
    // A quote that does not match the cited line is a WRONG LINE NUMBER far more
    // often than a sloppy transcription, and silently keeping the file's version
    // would be the worse failure of the two: the record would carry evidence the
    // model never examined, attached to a `why` describing different code. So
    // refuse it and hand back what is actually there, which is enough for the
    // model to correct the line by itself.
    //
    // This is also what the crawler role's own prompt has always promised
    // ("a report whose evidence is a paraphrase rather than the actual line is
    // rejected and does not count") — the code now keeps that promise.
    if captured.mismatch {
        let actual = captured.evidence.trim();
        return Ok(format!(
            "REJECTED: line {} of that file is:\n\n    {actual}\n\n\
             which is not what you quoted. You have most likely cited the wrong \
             line number. Find the line your evidence actually came from and call \
             again with it. This did not count against your budget.",
            args.line
        ));
    }

    let path = out_dir.join(FINDINGS_FILE);
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }

    let existing = std::fs::read_to_string(&path).unwrap_or_default();
    let count = existing.lines().filter(|l| !l.trim().is_empty()).count();
    if count >= MAX_FINDINGS_PER_DISPATCH {
        return Ok(format!(
            "REJECTED: this run's finding budget ({MAX_FINDINGS_PER_DISPATCH}) is spent. \
             Stop reporting and summarize what you covered and what you did not."
        ));
    }

    // `evidence` and `context` are the HARNESS's, read from disk, and by this
    // point the model's own quote has been checked against them. `why` is the
    // model's claim and stays unverified by design.
    let record = serde_json::json!({
        "file": args.file,
        "line": args.line,
        "pattern": args.pattern,
        "evidence": captured.evidence,
        "context": captured.context,
        "context_start": captured.start,
        "context_end": captured.end,
        "why": args.why,
        "ts": crate::trajectory::unix_ms(),
    });


    let mut line = serde_json::to_string(&record).context("serializing finding")?;
    line.push('\n');
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .with_context(|| format!("opening {}", path.display()))?;
    use std::io::Write as _;
    f.write_all(line.as_bytes())
        .with_context(|| format!("appending to {}", path.display()))?;

    let recorded = count + 1;
    let remaining = MAX_FINDINGS_PER_DISPATCH.saturating_sub(recorded);
    // Back-pressure through the return value: the model is TOLD where it stands
    // so it can self-limit, and the cap above enforces it regardless. Soft
    // signal plus hard bound, the same shape as the inactivity budget.
    Ok(format!(
        "Recorded. {recorded} finding(s) so far, {remaining} remaining in this run's budget. \
         Continue examining the scope; report the next one when you find it."
    ))
}
