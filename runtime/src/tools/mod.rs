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
    CreateFinding => "create_finding",
    CreateMod => "create_mod",
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
                 Every line comes back as `N: content`, where N is that \
                 line's own line number in the file — the same numbering \
                 `search` uses. A read at offset=146 starts at `146: `. \
                 You never have to count lines: the number you need is \
                 already on the line. The `N: ` prefix is NOT part of the \
                 file: never put it in `edit`'s `old_string` or `new_string`, \
                 never put it in `write`'s `content` (the runtime refuses \
                 content that still carries it), and when you quote a line \
                 as `evidence` for `create_finding`, copy only the content \
                 AFTER the prefix, verbatim.\n\
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
            Tool::CreateFinding => {
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
            Tool::CreateMod => {
                "Records ONE change you are proposing — a MOD — then lets you \
                 keep working. A finding says WHAT was observed; a mod says HOW \
                 it could change. \
                 Arguments: { for?: [finding keys], kit: string, attach?: [paths] }. \
                 `kit` is the change itself, as instructions and/or data, in \
                 whatever form you choose: a diff, a sentence, a JSON value, a \
                 config line. It is stored exactly as you write it and is never \
                 parsed, so write it so that someone applying it later — with no \
                 access to this conversation — has everything they need. When this \
                 change addresses a finding, name its key in `for`: the `key` \
                 `create_finding` returned to you when it recorded that finding, or \
                 the key a `<finding key=\"...\">` block in your message names. Do \
                 not compose a key yourself — a key that names no recorded finding \
                 is refused, and one you invent addresses nothing. Omit `for` when \
                 this change addresses no recorded finding. `attach` copies files from the \
                 workspace into the mod, for data a kit needs but cannot inline. \
                 This tool RECORDS the change; it does not apply anything, and \
                 recording a mod is not a substitute for making an edit you were \
                 asked to make. The return value tells you how many you have \
                 recorded."
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
            Tool::CreateFinding => serde_json::json!({
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
            Tool::CreateMod => serde_json::json!({
                "type": "object",
                "properties": {
                    "for": {
                        "type": "array",
                        "description": "Keys of the findings this change addresses. A key is the one `create_finding` returned when it recorded the finding, or the one a `<finding key=\"...\">` block in your message names — never one you compose yourself. Omit or leave empty when you do not know of a finding this addresses.",
                        "items": { "type": "string" }
                    },
                    "kit": {
                        "type": "string",
                        "description": "The change itself, as instructions and/or data — a diff, a sentence, a JSON value, a config line. Stored exactly as written and never parsed. Write it to be self-sufficient for whoever applies it later."
                    },
                    "attach": {
                        "type": "array",
                        "description": "Workspace-relative paths of files to copy into the mod, for data the kit needs but cannot inline. Each must be an existing file inside the workspace, and they may total at most 40960 bytes.",
                        "items": { "type": "string" }
                    }
                },
                "required": ["kit"]
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

    pub fn execute(self, raw_args: &str) -> Result<ToolRun> {
        let ws = Path::new(DEFAULT_WORKSPACE);
        match self {
            Tool::Echo => execute_echo(raw_args).map(ToolRun::text),
            Tool::Bash => execute_bash(raw_args, ws).map(ToolRun::text),
            Tool::Read => execute_read(raw_args, ws).map(ToolRun::text),
            Tool::Write => execute_write(raw_args, ws).map(ToolRun::text),
            Tool::Edit => execute_edit(raw_args, ws).map(ToolRun::text),
            Tool::Search => execute_search(raw_args, ws).map(ToolRun::text),
            Tool::CreateFinding => {
                execute_create_finding(raw_args, &crate::trajectory::runtime_dir(), ws)
            }
            Tool::CreateMod => {
                execute_create_mod(raw_args, &crate::trajectory::runtime_dir(), ws)
            }
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
pub fn dispatch(name: &str, raw_args: &str) -> ToolRun {
    match Tool::from_name(name) {
        Some(tool) => match tool.execute(raw_args) {
            Ok(run) => run,
            Err(e) => ToolRun::text(format_dispatch_error(tool, name, &e)),
        },
        None => ToolRun::text(format!(
            "tool '{name}' is not available in this runtime. \
             known tools: echo, bash, read, write, edit, search"
        )),
    }
}

/// What one tool call produced: the text that goes back into the
/// conversation as the `role: tool` message, plus — for an ACCEPTED
/// `create_finding` call only — the model's emission (#2272).
///
/// `emitted` is the tool's arguments exactly as the model sent them, parsed
/// and nothing more: darkmux does not know what is inside it and must not,
/// because it has no idea where the record will end up. A hook's transform
/// composes whatever its destination needs from the record's METADATA (crawl,
/// unit, rule, source, sha, model, timestamp, `emit_seq`) plus this blob.
/// Today function calling delivers it as a JSON object; the field's type is
/// "opaque value", not "finding". The trajectory's `args` is a 512-char
/// viewer preview and cannot carry it — that is the bug this closes.
///
/// `emit_seq` is the 1-based ordinal of this acceptance within the dispatch
/// (the findings-file count, so it survives a resume). Every other tool, and
/// every rejected report, carries `None` for both.
#[derive(Debug)]
pub struct ToolRun {
    pub result: String,
    pub emitted: Option<serde_json::Value>,
    pub emit_seq: Option<usize>,
}

impl ToolRun {
    pub fn text(result: String) -> Self {
        ToolRun { result, emitted: None, emit_seq: None }
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
    slice_lines_with_info(content, offset, limit).0
}

/// Like `slice_lines` but also returns (lines_returned, end_offset).
/// end_offset is the 1-indexed line number of the last line returned
/// (so the next region begins at end_offset+1).
///
/// (#2267) Every returned line carries its own 1-indexed FILE line number as
/// `N: content` — the same `path:line:content` vocabulary `search` already
/// emits, minus the path this call already named. Unnumbered output made the
/// model count lines by hand to find the one it had been asked about (26% of
/// one measured turn's reasoning was retyped source), and that cost lands in
/// generation, where no tool-call metric sees it. No padding: alignment would
/// buy nothing and every space is a token.
fn slice_lines_with_info(content: &str, offset: u64, limit: u64) -> (String, u64, u64) {
    let start = offset.saturating_sub(1) as usize;
    let all_lines: Vec<&str> = content.lines().skip(start).collect();
    let take = if limit == 0 { all_lines.len() } else { limit as usize };
    let kept: Vec<&str> = all_lines.into_iter().take(take).collect();
    let returned = kept.len() as u64;
    let end_offset = offset + returned.saturating_sub(1);
    let numbered = kept
        .iter()
        .enumerate()
        .map(|(i, l)| format!("{}: {l}", offset + i as u64))
        .collect::<Vec<_>>()
        .join("\n");
    (numbered, returned, end_offset)
}

// ─── write ────────────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct WriteArgs {
    path: String,
    content: String,
}

/// (#2267 review) Does `text` look like a `read` result pasted back — every
/// non-empty line carrying a `N: ` prefix with consecutive N? Two or more such
/// lines is the signature; a file whose lines merely start with numbers (a
/// YAML mapping, a numbered list) does not count up by one from line to line.
fn looks_like_read_echo(text: &str) -> bool {
    let mut expected: Option<u64> = None;
    let mut seen = 0usize;
    for line in text.lines() {
        if line.trim().is_empty() {
            continue;
        }
        let Some((num, rest)) = line.split_once(':') else { return false };
        if num.is_empty() || !num.chars().all(|c| c.is_ascii_digit()) || !(rest.is_empty() || rest.starts_with(' ')) {
            return false;
        }
        let n: u64 = match num.parse() { Ok(n) => n, Err(_) => return false };
        match expected {
            Some(e) if e != n => return false,
            _ => {}
        }
        expected = Some(n + 1);
        seen += 1;
    }
    seen >= 2
}

fn execute_write(raw_args: &str, workspace_root: &Path) -> Result<String> {
    let args: WriteArgs = serde_json::from_str(raw_args)
        .with_context(|| format!("parsing write arguments: {raw_args}"))?;
    if looks_like_read_echo(&args.content) {
        return Ok("NOT WRITTEN — `content` still carries `read`'s `N: ` line-number prefix on every line. \
                   The prefix is not part of the file; remove it from each line and call `write` again. \
                   The file was not changed."
            .to_string());
    }

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
    if args.edits.iter().any(|e| looks_like_read_echo(&e.new_string) || looks_like_read_echo(&e.old_string)) {
        return Ok("NOT EDITED — an `old_string`/`new_string` still carries `read`'s `N: ` line-number prefix on every line. \
                   The prefix is not part of the file; remove it and call `edit` again. The file was not changed."
            .to_string());
    }

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
        assert_eq!(names, ["echo", "bash", "read", "write", "edit", "search", "create_finding", "create_mod"]);
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
        let result = dispatch("echo", r#"{"text": "hello"}"#).result;
        assert_eq!(result, "hello");
    }

    #[test]
    fn unknown_tool_returns_error_message_not_panic() {
        let result = dispatch("teleport", r#"{}"#).result;
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

    // ─── create_finding: harness-captured context (#1959) ─────────────────

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
        execute_create_finding(&raw, out, ws).unwrap().result
    }

    #[test]
    fn an_accepted_report_returns_the_emission_verbatim_with_its_ordinal_and_a_rejected_one_returns_none() {
        // (#2272) The trajectory's `args` is a 512-char viewer preview and
        // cannot carry a crawl's product. What the model handed the tool
        // comes back VERBATIM — darkmux does not know what is inside it;
        // a hook's transform composes the destination payload from the
        // record's metadata plus this blob — with the per-dispatch ordinal
        // a transform can interpolate as an emit number.
        let ws = finding_workspace(&numbered_src(20, 7, "  if (a && b && c) {"));
        let out = tempfile::tempdir().unwrap();
        let why = "y".repeat(2_000);
        let raw = serde_json::json!({
            "path": "src/a.rs", "line": 7, "pattern": "p",
            "evidence": "  if (a && b && c) {", "why": why,
            "extra_the_model_chose_to_add": {"rect": [1, 2, 3, 4]},
        })
        .to_string();
        let run = execute_create_finding(&raw, out.path(), ws.path()).unwrap();
        assert!(run.result.starts_with("Recorded."), "{}", run.result);
        let emitted = run.emitted.expect("an accepted report returns its emission");
        let verbatim: serde_json::Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(emitted, verbatim, "the emission is the model's arguments, untouched: no alias normalization, no dropped keys");
        assert_eq!(emitted["why"].as_str().unwrap().len(), 2_000);
        assert_eq!(run.emit_seq, Some(1), "first accepted report in this dispatch");

        let second = execute_create_finding(&raw, out.path(), ws.path()).unwrap();
        assert_eq!(second.emit_seq, Some(2), "the ordinal is the findings-file count, so it survives a resume");

        let rejected = execute_create_finding(
            &serde_json::json!({"file": "src/a.rs", "line": 3, "pattern": "p",
                "evidence": "not what line 3 says", "why": "w"}).to_string(),
            out.path(), ws.path(),
        )
        .unwrap();
        assert!(rejected.result.starts_with("REJECTED"), "{}", rejected.result);
        assert!(rejected.emitted.is_none() && rejected.emit_seq.is_none(), "a rejected report emits nothing");
    }

    // (#2267 review) A source line that ITSELF begins with `N: ` on line N:
    // the compliant quote (prefix stripped) and the verbatim read quote
    // (prefix kept) must BOTH be accepted — the first version accepted only
    // the verbatim one and rejected the form its own description asks for.
    #[test]
    fn a_line_that_literally_starts_with_its_own_number_is_accepted_in_both_quote_forms() {
        let ws = finding_workspace(&numbered_src(90, 82, "82: foo"));
        let out = tempfile::tempdir().unwrap();
        let compliant = report(ws.path(), out.path(), 82, "82: foo");
        assert!(compliant.starts_with("Recorded."), "compliant (stripped) quote must be accepted: {compliant}");
        let verbatim = report(ws.path(), out.path(), 82, "82: 82: foo");
        assert!(verbatim.starts_with("Recorded."), "verbatim read quote must be accepted: {verbatim}");
        let wrong = report(ws.path(), out.path(), 82, "83: 82: foo");
        assert!(wrong.starts_with("REJECTED"), "a prefix naming another line is still a mismatch: {wrong}");
    }

    #[test]
    fn a_leading_space_before_the_prefix_does_not_reject_the_quote() {
        let ws = finding_workspace(&numbered_src(90, 7, "  if (a && b && c) {"));
        let out = tempfile::tempdir().unwrap();
        let r = report(ws.path(), out.path(), 7, " 7:   if (a && b && c) {");
        assert!(r.starts_with("Recorded."), "{r}");
    }

    // (#2267 review, MUST FIX) The numbering is on EVERY read, and the coder
    // role reads then edits/writes. Prose in the description is not a guard:
    // the runtime refuses to write line-numbered content back into a file.
    #[test]
    fn write_refuses_content_that_still_carries_reads_line_prefixes() {
        let ws = fresh_workspace();
        fs::write(ws.path().join("a.py"), "x = 1
y = 2
").unwrap();
        let raw = serde_json::json!({"path": "a.py", "content": "1: x = 1\n2: y = 3\n"}).to_string();
        let r = execute_write(&raw, ws.path()).unwrap();
        assert!(r.starts_with("NOT WRITTEN"), "{r}");
        assert!(r.contains("line-number prefix"), "{r}");
        assert_eq!(fs::read_to_string(ws.path().join("a.py")).unwrap(), "x = 1\ny = 2\n", "the file must be untouched");
    }

    #[test]
    fn write_accepts_content_whose_lines_merely_start_with_a_number() {
        // Not consecutive-from-somewhere, so not a read echo — a real file
        // may start lines with `N: ` (a YAML mapping, a numbered list).
        let ws = fresh_workspace();
        let raw = serde_json::json!({"path": "list.md", "content": "1: apples\n1: pears\n7: plums\n"}).to_string();
        let r = execute_write(&raw, ws.path()).unwrap();
        assert!(!r.starts_with("NOT WRITTEN"), "{r}");
    }

    #[test]
    fn edit_refuses_a_new_string_that_still_carries_reads_line_prefixes() {
        let ws = fresh_workspace();
        fs::write(ws.path().join("a.py"), "x = 1\ny = 2\nz = 3\n").unwrap();
        let raw = serde_json::json!({
            "path": "a.py",
            "edits": [{"old_string": "y = 2\nz = 3", "new_string": "2: y = 20\n3: z = 30"}]
        })
        .to_string();
        let r = execute_edit(&raw, ws.path()).unwrap();
        assert!(r.starts_with("NOT EDITED"), "{r}");
        assert_eq!(fs::read_to_string(ws.path().join("a.py")).unwrap(), "x = 1\ny = 2\nz = 3\n");
    }

    // ─── create_mod (#2265) ───────────────────────────────────────────────

    fn mod_workspace() -> tempfile::TempDir {
        let ws = fresh_workspace();
        std::fs::create_dir_all(ws.path().join("src")).unwrap();
        std::fs::write(ws.path().join("src/patch.diff"), b"--- a\n+++ b\n").unwrap();
        ws
    }

    fn last_mod(out: &Path) -> serde_json::Value {
        let body = std::fs::read_to_string(out.join(MODS_FILE)).expect("mods file");
        let line = body.lines().rfind(|l| !l.trim().is_empty()).expect("a record");
        serde_json::from_str(line).expect("valid json")
    }

    /// The accepted shape, whole: the emission is the model's arguments
    /// untouched, the ordinal is the file's own line count (so it survives a
    /// resume), and the recorded line carries the kit VERBATIM plus each
    /// attachment's bytes.
    #[test]
    fn an_accepted_mod_records_a_line_and_returns_its_emission_verbatim() {
        let ws = mod_workspace();
        let out = tempfile::tempdir().unwrap();
        let raw = serde_json::json!({
            "for": ["sess-a/01", "sess-a/2"],
            "kit": "replace the `!= null` with `is_some()` on line 82",
            "attach": ["src/patch.diff"],
            "extra_the_model_chose_to_add": {"confidence": 0.4},
        })
        .to_string();
        let run = execute_create_mod(&raw, out.path(), ws.path()).unwrap();
        assert!(run.result.starts_with("Recorded mod 1"), "{}", run.result);
        let emitted = run.emitted.expect("an accepted mod returns its emission");
        let mut expected: serde_json::Value = serde_json::from_str(&raw).unwrap();
        // Verbatim EXCEPT `attach`, which becomes the resolved list the host
        // reads — see `the_emission_carries_resolved_attachments_in_the_shape_
        // the_host_reads` for why the model's path strings cannot ride.
        expected["attach"] = serde_json::json!([
            {"path": "src/patch.diff", "bytes": "LS0tIGEKKysrIGIK"}
        ]);
        assert_eq!(
            emitted, expected,
            "every key but `attach` is the model's own arguments, untouched"
        );
        assert_eq!(run.emit_seq, Some(1), "first accepted mod in this dispatch");

        let rec = last_mod(out.path());
        assert_eq!(rec["seq"], 1);
        assert_eq!(rec["kit"], "replace the `!= null` with `is_some()` on line 82");
        assert_eq!(rec["for"], serde_json::json!(["sess-a/01", "sess-a/2"]));
        assert_eq!(rec["attach"][0]["path"], "src/patch.diff");
        assert_eq!(
            rec["attach"][0]["bytes"].as_str().unwrap(),
            "LS0tIGEKKysrIGIK",
            "the attachment's bytes are base64 of the file's content"
        );
        assert!(rec["ts"].as_i64().is_some(), "the record carries its own ts: {rec}");

        let second = execute_create_mod(&raw, out.path(), ws.path()).unwrap();
        assert_eq!(second.emit_seq, Some(2), "the ordinal is the mods-file count");
    }

    /// A mod with neither `for` nor `attach` is ordinary: the kit alone is a
    /// complete mod, and a model that does not know the finding key must
    /// still be able to record the change it made.
    #[test]
    fn a_kit_alone_is_a_complete_mod() {
        let ws = mod_workspace();
        let out = tempfile::tempdir().unwrap();
        let run = execute_create_mod(
            &serde_json::json!({"kit": "bump the timeout to 30s"}).to_string(),
            out.path(),
            ws.path(),
        )
        .unwrap();
        assert!(run.result.starts_with("Recorded mod 1"), "{}", run.result);
        let rec = last_mod(out.path());
        assert_eq!(rec["for"], serde_json::json!([]));
        assert_eq!(rec["attach"], serde_json::json!([]));
    }

    /// Every rejection: names what to fix, records nothing, counts nothing,
    /// and emits nothing. A `for` key that could address no finding is
    /// refused HERE — the host must only ever see addressable keys, and a
    /// rejection the model can read is louder than a host-side stderr line
    /// it never sees.
    #[test]
    fn a_mod_is_refused_for_an_empty_kit_an_unaddressable_for_key_or_a_bad_attachment() {
        let ws = mod_workspace();
        let out = tempfile::tempdir().unwrap();
        let cases = [
            (serde_json::json!({"kit": "   "}), "kit"),
            (serde_json::json!({"for": ["not-a-key"], "kit": "k"}), "for"),
            (serde_json::json!({"for": ["sess-a/x"], "kit": "k"}), "for"),
            (serde_json::json!({"kit": "k", "attach": ["src/nope.diff"]}), "attach"),
            (serde_json::json!({"kit": "k", "attach": ["../outside"]}), "attach"),
            (serde_json::json!({"kit": "k", "attach": ["src"]}), "attach"),
        ];
        for (args, names) in cases {
            let run = execute_create_mod(&args.to_string(), out.path(), ws.path()).unwrap();
            assert!(
                run.result.starts_with("NOT RECORDED"),
                "{args} should be refused, got: {}",
                run.result
            );
            assert!(
                run.result.contains(names),
                "the refusal must name what to fix ({names}): {}",
                run.result
            );
            assert!(
                run.emitted.is_none() && run.emit_seq.is_none(),
                "a refused mod emits nothing"
            );
        }
        assert!(
            !out.path().join(MODS_FILE).exists(),
            "a refused mod writes no line at all"
        );

        // Malformed arguments get a teaching response, never an Err — a tool
        // that errors reads to a model as "this tool is broken".
        let run = execute_create_mod("{not json", out.path(), ws.path()).unwrap();
        assert!(run.result.starts_with("NOT RECORDED"), "{}", run.result);
        assert!(run.result.contains("kit"), "the teaching response shows the shape: {}", run.result);
    }

    /// The attachment cap is a REFUSAL, not a truncation: half a file is not
    /// the data the kit needs, and the emission rides a flow record.
    #[test]
    fn an_attachment_over_the_cap_is_refused_with_its_size() {
        let ws = mod_workspace();
        let out = tempfile::tempdir().unwrap();
        std::fs::write(
            ws.path().join("big.bin"),
            vec![7u8; (MAX_ATTACHMENT_TOTAL_BYTES + 1) as usize],
        )
        .unwrap();
        let run = execute_create_mod(
            &serde_json::json!({"kit": "k", "attach": ["big.bin"]}).to_string(),
            out.path(),
            ws.path(),
        )
        .unwrap();
        assert!(run.result.starts_with("NOT RECORDED"), "{}", run.result);
        assert!(run.result.contains("big.bin"), "names the file that crossed it: {}", run.result);
        assert!(
            run.result.contains(&(MAX_ATTACHMENT_TOTAL_BYTES + 1).to_string())
                && run.result.contains(&MAX_ATTACHMENT_TOTAL_BYTES.to_string()),
            "names the size AND the budget: {}",
            run.result
        );
        assert!(!out.path().join(MODS_FILE).exists(), "nothing recorded");
    }

    /// Binary content survives the round trip — an attachment is bytes, not
    /// text, and a kit's data may be an image or a compiled artifact.
    #[test]
    fn an_attachment_is_encoded_as_bytes_not_text() {
        let ws = mod_workspace();
        let out = tempfile::tempdir().unwrap();
        let bytes: Vec<u8> = (0u8..=255).collect();
        std::fs::write(ws.path().join("blob.bin"), &bytes).unwrap();
        execute_create_mod(
            &serde_json::json!({"kit": "k", "attach": ["blob.bin"]}).to_string(),
            out.path(),
            ws.path(),
        )
        .unwrap();
        let rec = last_mod(out.path());
        let encoded = rec["attach"][0]["bytes"].as_str().unwrap();
        assert_eq!(b64_decode_for_test(encoded), bytes, "byte-identical after the round trip");
    }

    fn b64_decode_for_test(s: &str) -> Vec<u8> {
        const A: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
        let mut acc: u32 = 0;
        let mut bits = 0;
        let mut out = Vec::new();
        for c in s.bytes().filter(|c| *c != b'=') {
            let v = A.iter().position(|a| *a == c).expect("base64 alphabet") as u32;
            acc = (acc << 6) | v;
            bits += 6;
            if bits >= 8 {
                bits -= 8;
                out.push((acc >> bits) as u8);
            }
        }
        out
    }

    /// The repo-root fixture both sides of the runtime->host boundary read.
    /// Loaded at test RUNTIME (never `include_str!`), so the Docker image
    /// build — which compiles this crate with no test targets and no repo
    /// checkout above `runtime/` — is unaffected.
    fn wire_fixture() -> serde_json::Value {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../tests/fixtures/create_mod_wire.json");
        serde_json::from_str(&std::fs::read_to_string(&path).expect("the wire fixture")).unwrap()
    }

    /// (#2265 review, CRITICAL 1) The emission is the model's arguments with
    /// `attach` REPLACED by the resolved `[{path, bytes}]` list — the shape
    /// the HOST reads. The first version emitted the model's path strings,
    /// so an accepted mod WITH an attachment recorded nothing at all: the
    /// host read `a["path"]`, found a string, and bailed. Both sides now
    /// assert against this one fixture.
    #[test]
    fn the_emission_carries_resolved_attachments_in_the_shape_the_host_reads() {
        let fx = wire_fixture();
        let ws = fresh_workspace();
        let rel = fx["attachment"]["path"].as_str().unwrap();
        let path = ws.path().join(rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, fx["attachment"]["content"].as_str().unwrap()).unwrap();
        let out = tempfile::tempdir().unwrap();

        let run = execute_create_mod(&fx["args"].to_string(), out.path(), ws.path()).unwrap();
        assert!(run.result.starts_with("Recorded mod 1"), "{}", run.result);
        assert_eq!(
            run.emitted.expect("an accepted mod emits"),
            fx["emitted"],
            "the emission must match the wire fixture the host's own test reads"
        );
    }

    /// The two `for`-key predicates must agree, or a key the model is allowed
    /// to send is a mod the host silently drops. One table, both crates.
    #[test]
    fn the_for_key_predicate_agrees_with_the_hosts_on_the_shared_table() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../tests/fixtures/finding_key_cases.json");
        let fx: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).expect("the key fixture")).unwrap();
        for case in fx["cases"].as_array().unwrap() {
            let key = case["key"].as_str().unwrap();
            let valid = case["valid"].as_bool().unwrap();
            assert_eq!(
                finding_key_shape_ok(key),
                valid,
                "the runtime and the host must agree about {key:?}"
            );
        }
    }

    /// The attachment budget is TOTAL and sized so the emission stays under
    /// the host's own 64 KiB bound: an emission cut in half loses the kit,
    /// which is the whole product of the call.
    #[test]
    fn attachments_are_bounded_in_total_so_the_emission_never_truncates() {
        let ws = mod_workspace();
        let out = tempfile::tempdir().unwrap();
        // Two files, each under any per-file limit, together over the budget.
        let half = (MAX_ATTACHMENT_TOTAL_BYTES / 2) + 1024;
        std::fs::write(ws.path().join("a.bin"), vec![1u8; half as usize]).unwrap();
        std::fs::write(ws.path().join("b.bin"), vec![2u8; half as usize]).unwrap();
        let run = execute_create_mod(
            &serde_json::json!({"kit": "k", "attach": ["a.bin", "b.bin"]}).to_string(),
            out.path(),
            ws.path(),
        )
        .unwrap();
        assert!(run.result.starts_with("NOT RECORDED"), "{}", run.result);
        assert!(
            run.result.contains(&MAX_ATTACHMENT_TOTAL_BYTES.to_string()),
            "the refusal names the budget: {}",
            run.result
        );
        assert!(!out.path().join(MODS_FILE).exists(), "nothing recorded");

        // And the whole emission is bounded too, so a huge kit plus a legal
        // attachment cannot push it past what the host will forward.
        let big_kit = "x".repeat(MAX_EMISSION_BYTES + 10);
        let run = execute_create_mod(
            &serde_json::json!({"kit": big_kit}).to_string(),
            out.path(),
            ws.path(),
        )
        .unwrap();
        assert!(run.result.starts_with("NOT RECORDED"), "{}", run.result);
        assert!(run.result.contains("65536"), "names the bound: {}", run.result);
    }

    /// (#2265 review, NIT 7) `for` as a bare string is the near-miss a model
    /// makes when it has exactly one finding. Accept it as a one-element
    /// list rather than handing back a generic parse failure.
    #[test]
    fn a_single_for_key_may_be_a_bare_string() {
        let ws = mod_workspace();
        let out = tempfile::tempdir().unwrap();
        let run = execute_create_mod(
            &serde_json::json!({"for": "sess-a/1", "kit": "k"}).to_string(),
            out.path(),
            ws.path(),
        )
        .unwrap();
        assert!(run.result.starts_with("Recorded mod 1"), "{}", run.result);
        assert_eq!(last_mod(out.path())["for"], serde_json::json!(["sess-a/1"]));
    }

    /// `create_mod` is palette-named, exactly like `create_finding`: a role
    /// that does not name it never sees it.
    #[test]
    fn create_mod_is_in_the_tool_list_and_is_not_a_general_catalog_tool() {
        assert!(matches!(Tool::from_name("create_mod"), Some(Tool::CreateMod)));
        let d = Tool::CreateMod.description();
        assert!(d.contains("kit"), "the description names the kit: {d}");
        let schema = Tool::CreateMod.parameters_schema();
        assert_eq!(schema["required"], serde_json::json!(["kit"]));
        assert!(schema["properties"]["for"].is_object());
        assert!(schema["properties"]["attach"].is_object());
    }

    // ─── (#2386) no invented finding key anywhere the model can read ──

    /// The measured defect: the description said "the form
    /// `<dispatch>/<seq>`, e.g. `sess-abc/1`", and a reviewer seat
    /// (qwen3.6-35b-a3b-turboquant-mlx, mission
    /// `review-v2-1788589360-944ac8`) copied the example onto six mods,
    /// plus the `sess-1` variations it derived from it. Every model-facing
    /// string this tool can emit is checked, not just the description: the
    /// schema the model is handed, the teaching shape every refusal ends
    /// with, and the shape refusal itself.
    #[test]
    fn no_model_facing_create_mod_text_contains_an_example_finding_key() {
        let schema = Tool::CreateMod.parameters_schema().to_string();
        let shape_refusal = execute_create_mod(
            &serde_json::json!({"for": ["not-a-key"], "kit": "k"}).to_string(),
            fresh_workspace().path(),
            fresh_workspace().path(),
        )
        .unwrap()
        .result;
        for (what, text) in [
            ("description", Tool::CreateMod.description().to_string()),
            ("schema", schema),
            ("CREATE_MOD_SHAPE", CREATE_MOD_SHAPE.to_string()),
            ("the shape refusal", shape_refusal),
        ] {
            assert!(
                !text.contains("sess-abc") && !text.contains("sess-1"),
                "{what} still shows an example finding key a model can copy: {text}"
            );
        }
    }

    /// Removing the example is only half of it — a model with no key at all
    /// would omit `for` and the link would be lost anyway. Every one of
    /// those strings must say WHERE a real key comes from.
    #[test]
    fn create_mod_text_points_the_model_at_the_key_create_finding_returned() {
        for (what, text) in [
            ("description", Tool::CreateMod.description().to_string()),
            ("schema", Tool::CreateMod.parameters_schema().to_string()),
            ("CREATE_MOD_SHAPE", CREATE_MOD_SHAPE.to_string()),
        ] {
            assert!(text.contains("create_finding"), "{what} must name the source of a key: {text}");
        }
    }

    /// The other half of the repair: `create_finding` hands the key back, so
    /// the model HAS one to name. Before #2386 it answered "Recorded." and
    /// the only key-shaped text in its whole context was the example above.
    #[test]
    fn create_finding_hands_back_the_key_the_store_will_use() {
        let m = recorded_finding_message(Some(&finding_key_for("sess-x", 3)), 3, 17);
        assert!(m.contains("`sess-x/3`"), "the key the host will file this under: {m}");
        assert!(m.contains("create_mod"), "and what it is FOR: {m}");
        assert!(m.contains("3 finding(s) so far, 17 remaining"), "budget back-pressure kept: {m}");
        // A host that passed no --session-id gets the pre-#2386 wording
        // rather than an invented key.
        let none = recorded_finding_message(None, 1, 19);
        assert!(none.starts_with("Recorded. "), "{none}");
        assert!(!none.contains('/'), "no key is better than a guessed one: {none}");
    }

    /// The key form MUST equal the host's own
    /// `findings::build_record` -> `format!("{session_id}/{seq}")`. A key the
    /// model is told to use that the store files elsewhere is worse than none.
    #[test]
    fn the_key_form_is_session_slash_seq() {
        assert_eq!(finding_key_for("step-abc", 1), "step-abc/1");
        assert!(finding_key_shape_ok(&finding_key_for("step-abc", 12)));
    }

    #[test]
    fn a_for_key_this_run_recorded_is_accepted() {
        assert_eq!(refuse_for_key("me/1", Some("me"), 1, ""), None);
        assert_eq!(refuse_for_key("me/2", Some("me"), 3, ""), None);
    }

    #[test]
    fn a_for_key_ahead_of_what_this_run_recorded_is_rejected() {
        let r = refuse_for_key("me/4", Some("me"), 3, "").expect("refused");
        assert!(r.starts_with("REJECTED:"), "failure_rate.rs keys on this prefix: {r}");
        assert!(r.contains("`me/4`") && r.contains("recorded 3"), "{r}");
        let none_yet = refuse_for_key("me/1", Some("me"), 0, "").expect("refused");
        assert!(none_yet.contains("recorded none"), "{none_yet}");
    }

    /// The parroted key, exactly as it arrived in the live run.
    #[test]
    fn an_invented_key_from_another_dispatch_is_rejected() {
        let r = refuse_for_key("sess-abc/1", Some("crawl-unit-0001"), 2, "your brief text")
            .expect("refused");
        assert!(r.starts_with("REJECTED:"), "{r}");
        assert!(r.contains("sess-abc/1") && r.contains("does not name it"), "{r}");
    }

    /// The coder seat's ordinary case: the finding was recorded by an
    /// EARLIER dispatch and handed over in this one's brief, which
    /// `findings::brief_block` renders as `<finding key="...">`. Refusing
    /// that would break the review pipeline's whole finding -> mod path.
    #[test]
    fn a_foreign_key_this_dispatch_was_briefed_with_is_accepted() {
        let brief = "<finding key=\"crawl-x-0001/2\">\n context: ...\n</finding>";
        assert_eq!(refuse_for_key("crawl-x-0001/2", Some("me"), 0, brief), None);
    }

    /// (#2386 review, item 3) The brief check was `brief.contains(key)`.
    /// Probe 1 — PREFIX: a brief handing over `crawl-x/11` must not also
    /// hand over `crawl-x/1`, which is a different finding and a substring
    /// of the first.
    #[test]
    fn a_key_that_is_only_a_prefix_of_a_briefed_key_is_rejected() {
        let brief = "<finding key=\"crawl-x/11\">\n context: ...\n</finding>";
        assert_eq!(refuse_for_key("crawl-x/11", Some("me"), 0, brief), None, "the real one passes");
        let r = refuse_for_key("crawl-x/1", Some("me"), 0, brief).expect("the prefix is refused");
        assert!(r.starts_with("REJECTED:"), "{r}");
    }

    /// Probe 2 — PROSE: a key spelled in a sentence, a kit, or an earlier
    /// mod's text is a MENTION, not a handover. Only the `key="…"` attribute
    /// of a `<finding …>` tag hands a key over.
    #[test]
    fn a_key_merely_mentioned_in_prose_is_not_a_handover() {
        let brief = "Earlier someone proposed a change for sess-old/4; do not repeat it.";
        let r = refuse_for_key("sess-old/4", Some("me"), 0, brief).expect("refused");
        assert!(r.contains("does not name it"), "{r}");
        // And a `key="…"` outside a `<finding>` opening tag is not one either.
        let sneaky = "<mod key=\"mod-1-aaa\">kit mentions key=\"sess-old/4\"</mod>";
        assert!(refuse_for_key("sess-old/4", Some("me"), 0, sneaky).is_some());
    }

    /// Probe 3 — CANONICAL SEQ: `sess-a/02` and `sess-a/2` are ONE address
    /// (`mods::canonical_finding_key` renumbers on the host). Both branches
    /// of the check must agree with that and with each other.
    #[test]
    fn a_briefed_key_matches_across_seq_zero_padding() {
        let brief = "<finding key=\"sess-a/02\">x</finding>";
        assert_eq!(refuse_for_key("sess-a/2", Some("me"), 0, brief), None, "padded brief, bare for");
        let brief2 = "<finding key=\"sess-a/2\">x</finding>";
        assert_eq!(refuse_for_key("sess-a/02", Some("me"), 0, brief2), None, "bare brief, padded for");
        // The same-dispatch branch canonicalizes too, so the two halves of
        // this check never disagree about one address.
        assert_eq!(refuse_for_key("me/02", Some("me"), 2, ""), None);
        assert!(refuse_for_key("me/03", Some("me"), 2, "").is_some());
    }

    #[test]
    fn brief_finding_keys_reads_only_the_finding_tags_own_attribute() {
        let brief = "<finding key=\"a/1\">body</finding>\n<finding key=\"b/02\">body</finding>";
        let keys = brief_finding_keys(brief);
        assert!(keys.contains("a/1") && keys.contains("b/2"), "{keys:?}");
        assert_eq!(keys.len(), 2, "{keys:?}");
        assert!(brief_finding_keys("no tags here key=\"c/1\"").is_empty());
    }

    /// (#2386 review, item 4) The degraded mode must ANNOUNCE itself — a
    /// silent fallback is what makes a host/image version mismatch present
    /// as "mods link to nothing" with no way back to the cause.
    #[test]
    fn a_dispatch_with_no_session_id_says_so_and_one_with_an_id_stays_quiet() {
        let notice = leniency_notice(None).expect("the degraded mode announces itself");
        assert!(notice.starts_with("[darkmux-runtime]"), "{notice}");
        assert!(notice.contains("--session-id") && notice.contains("Upgrade"), "{notice}");
        assert_eq!(leniency_notice(Some("step-x")), None, "a grounded dispatch says nothing");
    }

    /// A runtime the host never told who it is cannot tell an invented key
    /// from one of its own, so it accepts — a false refusal costs the model
    /// its entire call, and the host still drops an unusable link.
    #[test]
    fn without_a_dispatch_identity_the_check_is_lenient() {
        assert_eq!(refuse_for_key("sess-abc/1", None, 0, ""), None);
    }

    /// End to end through the tool: a `for` key this run never recorded is
    /// NOT RECORDED, and nothing lands in `mods.jsonl`.
    #[test]
    fn create_mod_refuses_an_unrecorded_for_key_and_writes_nothing() {
        let ws = fresh_workspace();
        let out = fresh_workspace();
        let resp = execute_create_mod_with(
            &serde_json::json!({"for": ["sess-abc/1"], "kit": "a real kit"}).to_string(),
            out.path(),
            ws.path(),
            Some("crawl-unit-0001"),
            "no finding block here",
        )
        .unwrap()
        .result;
        assert!(resp.starts_with("REJECTED:"), "{resp}");
        assert!(
            !out.path().join(MODS_FILE).exists(),
            "a refused mod records nothing — the host must never see the dangling link"
        );
    }

    /// And the accept side still writes: the same call with a key this run
    /// DID record goes through unchanged.
    #[test]
    fn create_mod_still_records_when_the_for_key_is_this_runs_own() {
        let ws = finding_workspace(&numbered_src(40, 10, "    let _ = risky();"));
        let out = fresh_workspace();
        report(ws.path(), out.path(), 10, "    let _ = risky();");
        let resp = execute_create_mod_with(
            &serde_json::json!({"for": ["me/1"], "kit": "a real kit"}).to_string(),
            out.path(),
            ws.path(),
            Some("me"),
            "",
        )
        .unwrap()
        .result;
        assert!(!resp.starts_with("REJECTED:"), "{resp}");
        assert_eq!(last_mod(out.path())["for"], serde_json::json!(["me/1"]));
    }

    #[test]
    fn read_tool_description_covers_edit_and_write_not_just_create_finding() {
        let d = Tool::Read.description();
        assert!(d.contains("old_string") && d.contains("write"), "the prefix rule must name edit/write: {d}");
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
        let resp = execute_create_finding(&raw, out.path(), ws.path()).unwrap().result;
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

    // (#2267) `read` hands back `N: content`, N being the line's own number in
    // the FILE. Measured cost of not doing this: on crawl mission
    // `crawl-1788339231-148c18` a model spent 26% of one turn's reasoning
    // retyping source as `NNN: …` to locate a line by hand, because the unit
    // prompt named `App.tsx:186` and `read` returned 198 unnumbered lines.

    #[test]
    fn read_numbers_each_line_with_its_own_file_line_number() {
        let ws = fresh_workspace();
        let content: String = (1..=200).map(|i| format!("line {i}\n")).collect();
        fs::write(ws.path().join("a.txt"), content).unwrap();
        let raw =
            serde_json::json!({"path": "a.txt", "offset": 146, "limit": 3}).to_string();
        let result = execute_read(&raw, ws.path()).unwrap();
        assert!(
            result.starts_with("146: line 146\n147: line 147\n148: line 148"),
            "a read at offset 146 must start numbering at 146, got: {result}"
        );
        assert!(result.contains("returned 3 lines starting at offset 146"), "{result}");
    }

    #[test]
    fn read_numbers_an_empty_line_as_number_colon_space() {
        let ws = fresh_workspace();
        fs::write(ws.path().join("a.txt"), b"\nsecond\n").unwrap();
        let raw =
            serde_json::json!({"path": "a.txt", "offset": 1, "limit": 2}).to_string();
        let result = execute_read(&raw, ws.path()).unwrap();
        assert!(
            result.starts_with("1: \n2: second"),
            "an empty line renders as number, colon, space and nothing else, got: {result:?}"
        );
    }

    #[test]
    fn read_byte_cap_branch_numbers_from_the_offset() {
        let ws = fresh_workspace();
        // Each line is 16 bytes, so this comfortably exceeds READ_MAX_BYTES and
        // the truncating branch is the one that renders.
        let content: String = (1..=200_000).map(|i| format!("{i:0>14}\n")).collect();
        assert!(content.len() > READ_MAX_BYTES);
        fs::write(ws.path().join("big.txt"), content).unwrap();
        let raw =
            serde_json::json!({"path": "big.txt", "offset": 10, "limit": 2}).to_string();
        let result = execute_read(&raw, ws.path()).unwrap();
        assert!(
            result.starts_with("10: 00000000000010\n11: 00000000000011"),
            "the byte-cap branch numbers from the offset too, got: {result}"
        );
        assert!(result.contains("byte safety cap fired"), "{result}");
    }

    #[test]
    fn create_finding_accepts_evidence_that_kept_its_own_line_prefix() {
        // The model quotes what `read` handed it, prefix and all. That is the
        // literal line it examined, so it is accepted rather than rejected as
        // a mismatch.
        let ws = finding_workspace(&numbered_src(100, 82, "    let _ = risky();"));
        let out = fresh_workspace();
        let msg = report(ws.path(), out.path(), 82, "82:     let _ = risky();");
        assert!(msg.starts_with("Recorded."), "expected acceptance, got: {msg}");
        let rec = last_finding(out.path());
        assert_eq!(rec["evidence"], "    let _ = risky();");
    }

    #[test]
    fn create_finding_rejects_a_prefix_naming_a_different_line() {
        // Only the CITED line's own prefix is strippable. `83: ` on a finding
        // that cites line 82 is exactly the wrong-line-number error the
        // mismatch guard exists to catch.
        let ws = finding_workspace(&numbered_src(100, 82, "    let _ = risky();"));
        let out = fresh_workspace();
        let msg = report(ws.path(), out.path(), 82, "83:     let _ = risky();");
        assert!(
            msg.starts_with("REJECTED: line 82"),
            "a prefix naming another line must not be stripped, got: {msg}"
        );
    }

    #[test]
    fn create_finding_still_rejects_a_plain_mismatch() {
        let ws = finding_workspace(&numbered_src(100, 82, "    let _ = risky();"));
        let out = fresh_workspace();
        let msg = report(ws.path(), out.path(), 82, "    let _ = safe();");
        assert!(msg.starts_with("REJECTED: line 82"), "{msg}");
    }

    #[test]
    fn read_tool_description_states_the_numbered_form() {
        let d = Tool::Read.description();
        assert!(d.contains("N: content"), "description must name the numbered form: {d}");
        assert!(
            d.contains("create_finding"),
            "description must say what to copy when quoting evidence: {d}"
        );
    }

    #[test]
    fn read_returns_full_file_when_limit_zero() {
        let ws = fresh_workspace();
        fs::write(ws.path().join("a.txt"), b"line one\nline two\nline three").unwrap();
        let raw =
            serde_json::json!({"path": "a.txt", "offset": 1, "limit": 0}).to_string();
        let result = execute_read(&raw, ws.path()).unwrap();
        // (#2267) Was `starts_with("line one\nline two\nline three")`. Numbering
        // applies to the whole-file read too, so this is flipped to the numbered
        // form rather than relaxed — it still pins the exact rendering.
        assert!(result.starts_with("1: line one\n2: line two\n3: line three"), "{result}");
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
            _ => dispatch(name, raw_args).result,
        }
    }

    // ─── #424: tool-argument pre-flight ───────────────────────────────────

    #[test]
    fn dispatch_arg_parse_error_message_includes_schema_for_bash() {
        // Malformed JSON: `command` field missing. dispatch should
        // return an error string that includes the bash JSON-Schema
        // so the model can correct on the next turn.
        let result = dispatch("bash", r#"{"not_command": "ls"}"#).result;
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
        let result = dispatch("read", r#"{}"#).result;
        assert!(result.contains("EXPECTED argument schema for 'read'"));
        assert!(result.contains("\"path\""));
        assert!(result.contains("\"offset\""));
        assert!(result.contains("\"limit\""));
        assert!(result.starts_with("tool 'read' returned error:"));
    }

    #[test]
    fn dispatch_arg_parse_error_message_includes_schema_for_edit() {
        let result = dispatch("edit", r#"{"path": "/x"}"#).result; // missing edits[]
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
        ).result;
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
        let result = dispatch("echo", r#"{"text": "hi"}"#).result;
        assert_eq!(result, "hi");
        assert!(!result.contains("EXPECTED argument schema"));
    }

    #[test]
    fn dispatch_unknown_tool_does_not_add_schema() {
        // Unknown tools never had a schema-aware path — message
        // shape preserved.
        let result = dispatch("nonexistent_tool", r#"{}"#).result;
        assert!(result.contains("not available"));
        assert!(!result.contains("EXPECTED argument schema"));
    }

    #[test]
    fn dispatch_arg_parse_error_includes_underlying_serde_error() {
        // Should preserve serde's specific error message so the model
        // gets BOTH "what was wrong" and "what's expected" in one
        // result.
        let result = dispatch("echo", r#"{"wrong_field": "hi"}"#).result;
        // serde error names the missing field
        assert!(result.contains("text"), "must mention the expected field name. Got: {result}");
        // schema also included
        assert!(result.contains("EXPECTED argument schema for 'echo'"));
    }
}

// ─── (#2386) this dispatch's finding-key identity ─────────────────────────
//
// The finding STORE is host-side: `dispatch_internal.rs::materialize_finding`
// keys every accepted `create_finding` call as `<session-id>/<emit_seq>`.
// Until #2386 the runtime knew neither half, so `create_finding` could only
// answer "Recorded." — the model had no key to name in a later `create_mod`'s
// `for`, and the only key text anywhere in its context was the tool
// description's own invented example (`sess-abc/1`). A reviewer seat copied
// it onto six mods in one live run; each was stored as a link to nothing, so
// the coder phase redid work that had already been done.
//
// The host now passes `--session-id`, which is enough for both halves of the
// repair: `create_finding` hands back the REAL key, and `create_mod` can tell
// a key this run actually minted (or was handed in its brief) from one the
// model invented.
static DISPATCH_ID: std::sync::OnceLock<String> = std::sync::OnceLock::new();
static DISPATCH_BRIEF: std::sync::OnceLock<String> = std::sync::OnceLock::new();

/// Record what this dispatch is, once, before the loop runs. `session_id` is
/// `None` when the host passed no `--session-id` (an older host, or a
/// non-dispatch invocation), which leaves both tools at their pre-#2386
/// behavior rather than guessing an identity.
/// (#2386 review, item 4) What a dispatch with no `--session-id` says on
/// stderr — `None` when there is nothing to say.
///
/// Without an id this runtime silently drops to pre-#2386 behavior:
/// `create_finding` returns no key, and `create_mod` accepts any shape-valid
/// `for`. The usual cause is a version mismatch (an OLD darkmux binary
/// driving a NEW image), and the symptom — mods that link to nothing — shows
/// up far from the cause. One line here is the difference between a
/// diagnosable mismatch and a mystery.
///
/// A `[darkmux-runtime]` prefix, per the self-identifying-provenance
/// convention for text the runtime speaks in its own voice.
fn leniency_notice(session_id: Option<&str>) -> Option<&'static str> {
    session_id.is_none().then_some(
        "[darkmux-runtime] no --session-id was passed: finding keys cannot be grounded on this \
         dispatch, so create_finding cannot return one and create_mod cannot refuse an invented \
         `for` key. Upgrade the darkmux binary driving this container.",
    )
}

pub fn set_dispatch_context(session_id: Option<String>, brief: &str) {
    if let Some(notice) = leniency_notice(session_id.as_deref()) {
        eprintln!("{notice}");
    }
    if let Some(id) = session_id {
        let _ = DISPATCH_ID.set(id);
    }
    let _ = DISPATCH_BRIEF.set(brief.to_string());
}

fn dispatch_id() -> Option<&'static str> {
    DISPATCH_ID.get().map(String::as_str)
}

fn dispatch_brief() -> &'static str {
    DISPATCH_BRIEF.get().map(String::as_str).unwrap_or("")
}

/// The key `create_finding`'s host-side record will carry for the `seq`-th
/// finding of this dispatch — the address the model names in `create_mod`'s
/// `for`. `None` when this run has no identity to build one from.
///
/// It MUST agree with `crate::findings::build_record`'s own
/// `format!("{session_id}/{seq}")` on the host: a key the model is told to
/// use and the store then files under a different address is worse than no
/// key at all.
fn finding_key(seq: usize) -> Option<String> {
    dispatch_id().map(|d| finding_key_for(d, seq))
}

fn finding_key_for(dispatch: &str, seq: usize) -> String {
    format!("{dispatch}/{seq}")
}

/// The `create_finding` response text. Pure, so the key-bearing and
/// key-less forms are both testable without touching the process-wide
/// dispatch identity.
fn recorded_finding_message(key: Option<&str>, recorded: usize, remaining: usize) -> String {
    // Back-pressure through the return value: the model is TOLD where it
    // stands so it can self-limit, and the cap enforces it regardless. Soft
    // signal plus hard bound, the same shape as the inactivity budget.
    let tail = format!(
        "{recorded} finding(s) so far, {remaining} remaining in this run's budget. \
         Continue examining the scope; report the next one when you find it."
    );
    match key {
        // The key is FIRST and quoted, because it is the one part of this
        // response the model has to carry forward into another call.
        Some(k) => format!("Recorded finding `{k}`. Name `{k}` in `for` if you propose a change for it with `create_mod`. {tail}"),
        None => format!("Recorded. {tail}"),
    }
}

/// Split a key into `(dispatch, seq)` with the seq PARSED — the one place
/// this crate decides what a key means.
///
/// **Must agree with the host's `mods::canonical_finding_key`**, which
/// renumbers the seq so `sess-a/01` and `sess-a/1` are one address. Comparing
/// raw text instead lets the two halves of this check disagree with each
/// other and with the store: `/02` in a brief would not match `/2` in a
/// `for`, while the same-dispatch branch (which parses) would accept it.
fn canonical_parts(key: &str) -> Option<(&str, u64)> {
    let (dispatch, seq) = key.rsplit_once('/')?;
    Some((dispatch, seq.parse().ok()?))
}

fn canonical_key(key: &str) -> Option<String> {
    let (dispatch, seq) = canonical_parts(key)?;
    Some(format!("{dispatch}/{seq}"))
}

/// Every finding key this dispatch's brief DECLARES, canonical — read only
/// from the `key="…"` attribute of a `<finding …>` opening tag, which is the
/// shape `findings::brief_block` renders.
///
/// **Not a substring search.** The first version asked `brief.contains(key)`,
/// which accepts three things it should not: `crawl-x/1` whenever the brief
/// mentions `crawl-x/11` (a prefix of a longer key), any key a kit or a prose
/// sentence happens to spell (a mention is not a handover), and — because it
/// compares raw text — it disagrees with the same-dispatch branch about
/// `/02` versus `/2`. A key is handed over by the brief's own structure or it
/// is not handed over.
fn brief_finding_keys(brief: &str) -> std::collections::BTreeSet<String> {
    let mut keys = std::collections::BTreeSet::new();
    let mut rest = brief;
    while let Some(at) = rest.find("<finding") {
        let after = &rest[at + "<finding".len()..];
        // Bound the search to THIS opening tag, so a `key="…"` in some later
        // element's body can never be read as this one's attribute.
        let tag = match after.find('>') {
            Some(end) => &after[..end],
            None => after,
        };
        if let Some(k) = tag.find("key=\"") {
            let v = &tag[k + 5..];
            if let Some(end) = v.find('"') {
                if let Some(canonical) = canonical_key(&v[..end]) {
                    keys.insert(canonical);
                }
            }
        }
        rest = after;
    }
    keys
}

/// (#2386) Whether a `for` key can address a finding AT ALL from inside this
/// dispatch, and why not when it cannot. `None` = accept.
///
/// Two, and only two, sources give a model a real finding key:
///
/// 1. a `create_finding` call THIS run made (`<my-session>/<seq>`, and the
///    seq cannot run ahead of what has actually been recorded), or
/// 2. a `<finding key="…">` block in this dispatch's own brief — the coder
///    seat's ordinary case, where the finding was recorded by an EARLIER
///    dispatch (`findings::brief_block` renders the key verbatim).
///
/// Anything else was invented. Refusing it here rather than host-side is
/// deliberate and is the whole point: the host's materializer runs after the
/// tool already answered, so its only options are to drop the link silently
/// or throw away a good kit. At the tool boundary the model READS the
/// refusal and can call again with the right key — `failure_rate.rs`
/// classifies a `REJECTED:` reply from this tool as a repairable failure.
///
/// Lenient when this run has no identity (`dispatch_id()` is `None`): a
/// runtime the host did not tell who it is cannot distinguish (1) from an
/// invention, and a false refusal costs the model its whole call.
fn refuse_for_key(key: &str, my_dispatch: Option<&str>, recorded: usize, brief: &str) -> Option<String> {
    let me = my_dispatch?;
    let (dispatch, n) = canonical_parts(key)?;
    if dispatch == me {
        let n = n as usize;
        if n >= 1 && n <= recorded {
            return None;
        }
        return Some(format!(
            "REJECTED: `{key}` is not a finding this run recorded — {}. Use the key \
             `create_finding` returned to you, or drop `for` entirely if this change \
             addresses no recorded finding.",
            match recorded {
                0 => "this run has recorded none".to_string(),
                1 => format!("this run has recorded 1, `{me}/1`"),
                n => format!("this run has recorded {n}, `{me}/1` through `{me}/{n}`"),
            }
        ));
    }
    if brief_finding_keys(brief).contains(&canonical_key(key)?) {
        return None;
    }
    Some(format!(
        "REJECTED: `{key}` is not a finding this run recorded, and your message does not \
         name it either. A key comes from the `create_finding` call that recorded the \
         finding, or from a `<finding key=\"...\">` block in your message — never from an \
         example. Drop `for` entirely if this change addresses no recorded finding."
    ))
}

// ─── create_finding ───────────────────────────────────────────────────────
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
    /// (#2267) Whether the model's quote arrived carrying `read`'s own
    /// `N: ` prefix. Recorded so a later reader can tell which form the
    /// tier produced without re-deriving it.
    evidence_had_line_prefix: bool,
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
    // (#2267) `read` hands lines back as `N: content`, so a model quoting what
    // it was shown may quote the prefix too. That is the line it examined, not
    // a paraphrase, so accept it — but strip ONLY this line's own exact prefix
    // (`{line}: `). A generic `\d+: ` strip would silently repair a citation
    // naming the WRONG line, which is the error this guard exists to catch, and
    // would also mangle source lines that legitimately begin with digits and a
    // colon.
    // (#2267 review) Accept EITHER form: the compliant quote (prefix
    // stripped) and the verbatim read quote (prefix kept). A source line that
    // itself begins with `{n}: ` on line n makes them differ, and the first
    // version rejected exactly the form its own description asks for.
    let own_prefix = format!("{n}: ");
    let claimed_ns = claimed.trim_start();
    let stripped = claimed_ns.strip_prefix(own_prefix.as_str()).unwrap_or(claimed_ns);
    let mismatch = !claimed.trim().is_empty()
        && stripped.trim() != evidence.trim()
        && claimed_ns.trim() != evidence.trim();
    // Set only when the STRIPPED form is the one that matched — a verbatim
    // quote of a line that itself starts with `{n}: ` carried no read prefix.
    let evidence_had_line_prefix = stripped.len() != claimed_ns.len() && stripped.trim() == evidence.trim();
    Ok(FindingContext {
        evidence,
        context,
        start,
        end,
        mismatch,
        evidence_had_line_prefix,
    })
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
struct CreateFindingArgs {
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

fn execute_create_finding(
    raw_args: &str,
    out_dir: &Path,
    workspace_root: &Path,
) -> Result<ToolRun> {
    // NEVER return Err from a model-facing tool.
    //
    // Measured on the first live crawl: one malformed call returned an error,
    // and the model concluded "the create_finding tool is not available in
    // this runtime" and abandoned the channel entirely for the rest of the
    // run — falling back to narrating findings in prose, where nothing could
    // record them. It never retried.
    //
    // A tool ERROR reads to a model as "this tool is broken or absent." A tool
    // RESPONSE reads as "try again, differently." So a malformed call gets a
    // teaching response showing exactly what a correct call looks like, and
    // the run continues.
    // (#2272) The emission, verbatim, kept from the ONE parse the runtime
    // already does — before the struct below normalizes aliases and drops
    // whatever keys it does not know. This is what rides the event.
    let verbatim: Option<serde_json::Value> = serde_json::from_str(raw_args).ok();
    let args: CreateFindingArgs = match serde_json::from_str(raw_args) {
        Ok(a) => a,
        Err(e) => {
            return Ok(ToolRun::text(format!(
                "NOT RECORDED — I could not read those arguments ({e}). \
                 This tool takes exactly these five keys:\n\
                 \n  {{\"file\": \"crates/foo/src/bar.rs\", \"line\": 147, \
                 \"pattern\": \"<the pattern you were given>\", \
                 \"evidence\": \"<the source line, copied verbatim>\", \
                 \"why\": \"<one or two sentences>\"}}\n\
                 \nThe tool IS available — call it again with those keys. \
                 Nothing was counted against your budget."
            )));
        }
    };
    if args.evidence.trim().is_empty() && args.why.trim().is_empty() {
        return Ok(ToolRun::text("NOT RECORDED — `evidence` (the source line, copied verbatim) \
                   and `why` are both required. The tool IS available; call it again \
                   with those fields filled in. Nothing counted against your budget."
            .to_string()));
    }

    // "Cite the line", enforced at the tool boundary rather than in triage.
    // A finding that cannot point at code is not a finding, and rejecting it
    // here costs nothing — no frontier token is ever spent on it.
    if args.evidence.trim().is_empty() {
        return Ok(ToolRun::text("REJECTED: `evidence` was empty. Copy the source line verbatim \
                   from the file and call again. This did not count against your budget."
            .to_string()));
    }
    if args.line == 0 {
        return Ok(ToolRun::text("REJECTED: `line` must be the 1-indexed line where the evidence \
                   appears. This did not count against your budget."
            .to_string()));
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
            return Ok(ToolRun::text(format!(
                "REJECTED: {reason}. Re-check the path and the 1-indexed line, \
                 then call again. This did not count against your budget."
            )));
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
        return Ok(ToolRun::text(format!(
            "REJECTED: line {} of that file is:\n\n    {actual}\n\n\
             which is not what you quoted. You have most likely cited the wrong \
             line number. Find the line your evidence actually came from and call \
             again with it. This did not count against your budget.",
            args.line
        )));
    }

    let path = out_dir.join(FINDINGS_FILE);
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }

    let existing = std::fs::read_to_string(&path).unwrap_or_default();
    let count = existing.lines().filter(|l| !l.trim().is_empty()).count();
    if count >= MAX_FINDINGS_PER_DISPATCH {
        return Ok(ToolRun::text(format!(
            "REJECTED: this run's finding budget ({MAX_FINDINGS_PER_DISPATCH}) is spent. \
             Stop reporting and summarize what you covered and what you did not."
        )));
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
        "evidence_had_line_prefix": captured.evidence_had_line_prefix,
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
    // (#2386) `recorded` IS the `emit_seq` the host keys the stored record
    // by, so the key handed back here is the address the store will use.
    Ok(ToolRun {
        result: recorded_finding_message(finding_key(recorded).as_deref(), recorded, remaining),
        emitted: verbatim,
        emit_seq: Some(recorded),
    })
}

// ─── create_mod ───────────────────────────────────────────────────────────
//
// (#2265) The MOD channel: how something could change, recorded from inside a
// dispatch. Its sibling `create_finding` records WHAT was observed; this one
// records the HOW, as a KIT — instructions and/or data in whatever form the
// model chose, stored verbatim and never parsed.
//
// Same shape as the finding channel for the same reasons: a per-dispatch
// append-only file beside the trajectory (so a killed run keeps every mod
// already recorded, and the line count IS the ordinal), a model-facing
// response that never returns `Err` (a tool that errors reads to a model as
// "this tool is broken" and gets abandoned), and an `emitted` that rides the
// `tool.completed` event so the host can materialize the durable record.

/// The per-dispatch mod file, beside `findings.jsonl` in the runtime dir.
pub const MODS_FILE: &str = "mods.jsonl";

/// TOTAL raw bytes of attachments one mod may carry, before encoding.
///
/// **Sized against the HOST's emission bound, not against what a file system
/// would tolerate.** The emission rides a `dispatch.tool` flow record, which
/// `bound_emitted` cuts at 64 KiB — and a cut emission has no `kit` at all, so
/// an over-large attachment would silently cost the model the entire product
/// of its call. Base64 costs 4 bytes per 3, so 40 KiB raw is ~54.6 KiB
/// encoded and leaves room for the kit and the JSON around it. The first
/// version's 1 MiB per-file ceiling was ~20x past the point where the record
/// stopped carrying the kit.
///
/// A file bigger than this is `darkmux mod create --attach` territory: that
/// producer runs on the host, copies from a path, and rides no flow record.
const MAX_ATTACHMENT_TOTAL_BYTES: u64 = 40 * 1024;

/// Bound on the SERIALIZED emission, mirroring the host's `MAX_EMITTED_BYTES`
/// (`crates/darkmux-crew/src/dispatch_internal.rs`). Checked here so the
/// truncation is never reachable from this tool: over the bound the call is
/// REFUSED with something the model can act on, rather than accepted and then
/// quietly stripped of its kit somewhere downstream.
pub const MAX_EMISSION_BYTES: usize = 64 * 1024;

/// The teaching response every refusal ends with — a model that cannot read a
/// rejection cannot correct it, and a `create_mod` that reads as broken gets
/// abandoned the way `create_finding` was on the first live crawl.
///
/// (#2386) The `for` slot is a PLACEHOLDER, never a literal key. The first
/// version showed `["sess-abc/1"]` here and in the description, and a
/// reviewer seat copied that example onto six mods in one live run —
/// every one of them a link to a finding that does not exist. A model with
/// no key of its own reads an example key as an answer.
const CREATE_MOD_SHAPE: &str = "\n\n  {\"for\": [\"<the key create_finding returned>\"], \
     \"kit\": \"<the change, as instructions and/or data>\", \
     \"attach\": [\"path/inside/the/workspace\"]}\n\n\
     Only `kit` is required. Nothing was recorded.";

/// Accept `"for": "sess-a/1"` as well as `"for": ["sess-a/1"]`.
///
/// A model with exactly ONE finding writes the bare string; refusing that with
/// a generic parse error teaches nothing and costs the whole call. Same
/// liberal-in-what-we-accept reasoning as `CreateFindingArgs`'s aliases.
fn string_or_list<'de, D>(d: D) -> std::result::Result<Vec<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum OneOrMany {
        One(String),
        Many(Vec<String>),
    }
    Ok(match OneOrMany::deserialize(d)? {
        OneOrMany::One(s) => vec![s],
        OneOrMany::Many(v) => v,
    })
}

#[derive(Debug, Deserialize)]
struct CreateModArgs {
    /// Findings this change addresses. Optional and may be empty: a model that
    /// does not know a finding key still made the change.
    #[serde(
        default,
        rename = "for",
        alias = "findings",
        alias = "for_findings",
        deserialize_with = "string_or_list"
    )]
    r#for: Vec<String>,
    /// The change itself. Required, stored verbatim, never parsed.
    #[serde(default, alias = "change", alias = "instructions")]
    kit: String,
    /// Workspace-relative paths copied into the mod.
    #[serde(default, alias = "attachments", alias = "files")]
    attach: Vec<String>,
}

/// Whether a string could address a finding: `<dispatch>/<seq>`, split on the
/// LAST separator, with a dispatch half that is a safe path segment and a seq
/// that parses.
///
/// **Validated HERE rather than host-side**, deliberately. The host cannot
/// teach the model anything — a stderr line on a successful dispatch is kept
/// only as a byte count — so a key that can address no finding has to be
/// refused where the model will read the refusal and can call again with the
/// right one. It also means the host only ever sees addressable keys, which
/// is what lets it canonicalize without a second failure mode.
///
/// The canonical FORM (`sess-a/01` and `sess-a/1` are one address) is the
/// host's to compute — `mods::canonical_finding_key` — because the store is
/// what has to agree with itself. This checks shape only.
fn finding_key_shape_ok(key: &str) -> bool {
    let Some((dispatch, seq)) = key.rsplit_once('/') else {
        return false;
    };
    // EXACTLY `findings::is_safe_dispatch_segment` (darkmux-crew), duplicated
    // rather than shared because this crate cannot depend on the workspace —
    // and pinned to its twin by `the_for_key_predicate_agrees_with_the_hosts_
    // on_the_shared_table`, which reads the same checked-in table the host's
    // own test reads. A rule that drifts here is a mod the host drops: the
    // first version omitted the `/` check, so `sess/extra/1` passed the model-
    // facing gate and then failed silently on the far side.
    !dispatch.is_empty()
        && !dispatch.starts_with('.')
        && !dispatch.contains('/')
        && !dispatch.contains('\\')
        && !dispatch.contains('\0')
        && seq.parse::<u64>().is_ok()
}

/// Standard base64, inline rather than a dependency (the dep set is
/// deliberately small; this is the whole encoder). An attachment is BYTES —
/// an image, a compiled artifact — and JSON strings hold only valid UTF-8, so
/// the bytes are encoded rather than lossily stringified.
fn b64_encode(bytes: &[u8]) -> String {
    const A: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b = [chunk[0], *chunk.get(1).unwrap_or(&0), *chunk.get(2).unwrap_or(&0)];
        let n = ((b[0] as u32) << 16) | ((b[1] as u32) << 8) | b[2] as u32;
        out.push(A[(n >> 18) as usize & 63] as char);
        out.push(A[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 { A[(n >> 6) as usize & 63] as char } else { '=' });
        out.push(if chunk.len() > 2 { A[n as usize & 63] as char } else { '=' });
    }
    out
}

/// Production entry: this dispatch's own identity and brief, read from the
/// process-wide context the host set before the loop started.
fn execute_create_mod(raw_args: &str, out_dir: &Path, workspace_root: &Path) -> Result<ToolRun> {
    execute_create_mod_with(raw_args, out_dir, workspace_root, dispatch_id(), dispatch_brief())
}

/// (#2386) The same tool with its dispatch context passed IN — the form the
/// tests drive, so a test that needs an identity does not have to set a
/// process-wide `OnceLock` that every later test in the binary would then
/// inherit.
fn execute_create_mod_with(
    raw_args: &str,
    out_dir: &Path,
    workspace_root: &Path,
    my_dispatch: Option<&str>,
    brief: &str,
) -> Result<ToolRun> {
    // NEVER return Err from a model-facing tool — see
    // `execute_create_finding`'s own doc for the measured reason.
    // (#2265) The emission, verbatim, from the ONE parse the runtime already
    // does — before the struct below normalizes aliases and drops keys it does
    // not know. This is what rides the event and becomes the host's record.
    let verbatim: Option<serde_json::Value> = serde_json::from_str(raw_args).ok();
    let args: CreateModArgs = match serde_json::from_str(raw_args) {
        Ok(a) => a,
        Err(e) => {
            return Ok(ToolRun::text(format!(
                "NOT RECORDED — I could not read those arguments ({e}). \
                 This tool takes:{CREATE_MOD_SHAPE} \
                 The tool IS available — call it again with that shape."
            )));
        }
    };
    if args.kit.trim().is_empty() {
        return Ok(ToolRun::text(format!(
            "NOT RECORDED — `kit` is required and must not be empty: it IS the \
             change, as instructions and/or data, and it is stored exactly as \
             you write it.{CREATE_MOD_SHAPE}"
        )));
    }
    // A key of the wrong shape can address no finding, so it would be stored
    // as a link nothing could follow. Refused with the form, not repaired.
    if let Some(bad) = args.r#for.iter().find(|k| !finding_key_shape_ok(k)) {
        return Ok(ToolRun::text(format!(
            "NOT RECORDED — {bad:?} in `for` is not a finding key. A key is the \
             `<dispatch>/<seq>` address `create_finding` returned when it recorded \
             the finding, or the one a `<finding key=\"...\">` block in your message \
             names. Drop `for` entirely if you do not know which finding this \
             addresses.{CREATE_MOD_SHAPE}"
        )));
    }
    // (#2386) The shape is right; now — can this key address a finding at
    // all? A key naming this run's own dispatch must name a finding this run
    // actually recorded, and a key naming any other dispatch must be one
    // this dispatch's message handed it. Anything else was invented, and a
    // stored link nothing can follow is exactly what `canonical_for_keys`'s
    // own doc says it exists to prevent.
    {
        let recorded = std::fs::read_to_string(out_dir.join(FINDINGS_FILE))
            .unwrap_or_default()
            .lines()
            .filter(|l| !l.trim().is_empty())
            .count();
        for key in &args.r#for {
            if let Some(refusal) = refuse_for_key(key, my_dispatch, recorded, brief) {
                return Ok(ToolRun::text(format!("{refusal}{CREATE_MOD_SHAPE}")));
            }
        }
    }

    // Attachments are read BEFORE anything is written, so a mod is never
    // recorded naming a file it does not carry.
    let mut attached: Vec<serde_json::Value> = Vec::new();
    let mut total_bytes: u64 = 0;
    for rel in &args.attach {
        let path = match resolve_read(rel, workspace_root) {
            Ok(p) => p,
            Err(e) => {
                return Ok(ToolRun::text(format!(
                    "NOT RECORDED — `attach` names {rel:?}, which did not resolve to a \
                     readable path inside the workspace ({e}). Attach only files that \
                     exist in the workspace, by their workspace-relative path."
                )));
            }
        };
        let meta = match std::fs::metadata(&path) {
            Ok(m) if m.is_file() => m,
            Ok(_) => {
                return Ok(ToolRun::text(format!(
                    "NOT RECORDED — `attach` names {rel:?}, which is not a file. \
                     Attach individual files, not directories."
                )));
            }
            Err(e) => {
                return Ok(ToolRun::text(format!(
                    "NOT RECORDED — `attach` names {rel:?}, which could not be read ({e})."
                )));
            }
        };
        // A cap is a REFUSAL, not a truncation: half a file is not the data the
        // kit needs, and the emission rides a flow record the host bounds.
        total_bytes += meta.len();
        if total_bytes > MAX_ATTACHMENT_TOTAL_BYTES {
            return Ok(ToolRun::text(format!(
                "NOT RECORDED — the attachments total {total_bytes} bytes, over this \
                 tool's budget of {MAX_ATTACHMENT_TOTAL_BYTES} bytes across ALL \
                 attachments on one mod ({rel:?} is the one that crossed it). Attach \
                 less, or name in the `kit` where the data lives instead — the record \
                 travels on a size-bounded channel, and a mod cut in half would lose \
                 its kit."
            )));
        }
        let bytes = match std::fs::read(&path) {
            Ok(b) => b,
            Err(e) => {
                return Ok(ToolRun::text(format!(
                    "NOT RECORDED — `attach` names {rel:?}, which could not be read ({e})."
                )));
            }
        };
        attached.push(serde_json::json!({ "path": rel, "bytes": b64_encode(&bytes) }));
    }

    let path = out_dir.join(MODS_FILE);
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    // The line count IS the ordinal, so it survives a resume — the same
    // property the findings file has, for the same reason.
    let existing = std::fs::read_to_string(&path).unwrap_or_default();
    let count = existing.lines().filter(|l| !l.trim().is_empty()).count();

    // (#2265 review, CRITICAL 1) The emission is the model's arguments
    // VERBATIM, with ONE substitution: `attach` becomes the RESOLVED
    // `[{path, bytes}]` list. The host has no access to this container's
    // workspace, so the model's path strings would name files it can never
    // open — the first version emitted them, and every accepted mod with an
    // attachment vanished on the far side. Every other key, including ones
    // darkmux does not know, rides untouched. An alias key the model used
    // (`files`, `attachments`) stays verbatim beside the canonical `attach`
    // the host reads.
    let emitted = verbatim.map(|mut v| {
        if !attached.is_empty() {
            if let Some(obj) = v.as_object_mut() {
                obj.insert("attach".to_string(), serde_json::Value::Array(attached.clone()));
            }
        }
        v
    });
    // The whole emission is bounded HERE so the host's own bound is never
    // reached: over there, an over-size emission is replaced by a truncation
    // marker with no `kit` in it at all, and the kit is the product.
    if let Some(e) = &emitted {
        let size = e.to_string().len();
        if size > MAX_EMISSION_BYTES {
            return Ok(ToolRun::text(format!(
                "NOT RECORDED — these arguments serialize to {size} bytes, over the \
                 {MAX_EMISSION_BYTES}-byte limit for one mod. Shorten the `kit`, or \
                 drop an attachment and say in the `kit` where the data lives. \
                 Recording it as-is would cut the record and lose the kit."
            )));
        }
    }

    let record = serde_json::json!({
        "seq": count + 1,
        "for": args.r#for,
        // Byte-exact. darkmux never types a kit and never opens it.
        "kit": args.kit,
        "attach": attached,
        "ts": crate::trajectory::unix_ms(),
    });
    let mut line = serde_json::to_string(&record).context("serializing mod")?;
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
    Ok(ToolRun {
        result: format!(
            "Recorded mod {recorded}. It is a record of the change, not the change \
             itself — carry on with the work you were asked to do."
        ),
        emitted,
        emit_seq: Some(recorded),
    })
}
