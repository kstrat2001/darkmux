//! Workspace-root path enforcement.
//!
//! All Read / Write tool paths must resolve to somewhere inside the
//! dispatch's workspace root (default: `/workspace` inside the container).
//! This module is the security-critical piece for those — they route through
//! here first. The Bash tool does NOT use this validator: its commands run
//! in the container and are bounded by the container sandbox itself, not by
//! workspace-root path resolution (#905).
//!
//! Three attack vectors the validator defends against:
//!
//! 1. **Absolute paths outside workspace** — `/etc/passwd`, `/home/...`,
//!    etc. Rejected by the post-canonicalization prefix check.
//! 2. **Relative paths with `..` escapes** — `../../etc/passwd` resolves
//!    out of workspace. Rejected after canonicalization.
//! 3. **Symlink escapes** — `ln -s /etc/passwd inside-workspace/leak`
//!    followed by `read inside-workspace/leak` would otherwise follow
//!    the link. `canonicalize()` resolves symlinks before the prefix
//!    check, so the target's real path is what we test.
//!
//! Note on the prefix check: `Path::starts_with` does **component-wise**
//! prefix matching, not substring matching. So `/workspace2/foo`
//! correctly fails the `starts_with("/workspace")` test — the components
//! are `[workspace2, foo]` and `[workspace]` which don't share a prefix.

use anyhow::{anyhow, Context, Result};
use std::path::{Path, PathBuf};

/// Default workspace root inside the dispatch container.
pub const DEFAULT_WORKSPACE: &str = "/workspace";

/// (#3112 step 2) Env override of [`DEFAULT_WORKSPACE`]: the host sets it
/// when its sandbox cannot place the workspace at `/workspace` (Seatbelt
/// sees it at its own host path). Never set for a container.
pub const WORKSPACE_ENV: &str = "DARKMUX_RUNTIME_WORKSPACE";

/// This run's workspace root: [`WORKSPACE_ENV`] when set and non-empty,
/// otherwise [`DEFAULT_WORKSPACE`].
pub fn root() -> PathBuf {
    match std::env::var_os(WORKSPACE_ENV) {
        Some(dir) if !dir.is_empty() => dir.into(),
        _ => DEFAULT_WORKSPACE.into(),
    }
}

/// (#3654) What model-facing text calls the workspace when the root moved:
/// the current directory, where every tool runs and against which every
/// relative path resolves. A sandbox on the host sees the workspace at its
/// host path, under the user's home, and that path must never reach a model.
pub const CURRENT_DIR: &str = ".";

/// Whether `root` is somewhere other than [`DEFAULT_WORKSPACE`]: a sandbox
/// on the host, which names no path of its own to a model.
fn moved(root: &Path) -> bool {
    root.as_os_str() != DEFAULT_WORKSPACE
}

/// (#3654) The runtime's own sentences that name `/workspace`, and what each
/// says when the workspace is the current directory instead. Applied before
/// the path rename in [`describing_root`], so the tool descriptions read as
/// sentences rather than as `./...`; every entry matches the runtime's own
/// text (`every_host_phrase_matches_the_runtimes_own_text`).
const HOST_PHRASES: &[(&str, &str)] = &[
    ("(cwd = /workspace)", "(the workspace is its current directory)"),
    ("Path may be absolute (/workspace/...) or relative.", "Path is relative to the current directory, which is the workspace."),
    ("Absolute /workspace/... or workspace-relative.", "Relative to the current directory, which is the workspace."),
    ("Runs with cwd=/workspace.", "Runs with the workspace as its current directory."),
    ("with cwd=/workspace", "with the workspace as its current directory"),
    ("inside /workspace", "inside the workspace"),
    ("outside /workspace", "outside the workspace"),
];

/// Model-facing text from outside the runtime (the user's message) for this
/// run's [`root`]: byte-identical when the root is the default (contract 6).
/// Under a moved root (a sandbox on the host) every `/workspace` at a path
/// boundary becomes [`CURRENT_DIR`], so `/workspace/src/x.rs` reads
/// `./src/x.rs` (#3654): no host path is ever written into it.
pub fn naming_root(text: &str) -> String {
    if !moved(&root()) {
        return text.to_string();
    }
    naming(text, CURRENT_DIR)
}

/// The runtime's own model-facing text (its tool definitions and its
/// default system prompt) for this run's [`root`]: [`naming_root`], after
/// its sentences that name `/workspace` are said for a workspace that is the
/// current directory ([`HOST_PHRASES`]), so they read as sentences rather
/// than as `./...`. Byte-identical under the default root.
pub fn describing_root(text: &str) -> String {
    if !moved(&root()) {
        return text.to_string();
    }
    let phrased = HOST_PHRASES.iter().fold(text.to_string(), |t, (from, to)| t.replace(from, to));
    naming(&phrased, CURRENT_DIR)
}

/// `text` with [`DEFAULT_WORKSPACE`] named as `name`, wherever it is the
/// path itself (#3112 review): preceded by the start or a character that
/// cannot be part of a path, and followed by the end, `/`, or a character
/// that cannot continue a name. `/workspace2`, `/workspaces`, `/workspace_x`
/// and `/data/workspace` are other paths and are left alone.
fn naming(text: &str, name: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut from = 0;
    while let Some(found) = text[from..].find(DEFAULT_WORKSPACE) {
        let at = from + found;
        let end = at + DEFAULT_WORKSPACE.len();
        // The boundary is read from the whole text, never the slice after
        // the last match: in `/workspace/workspace` the second one follows
        // an `e`, a path character, so it is a directory inside.
        let before_ok = text[..at].chars().next_back().is_none_or(|c| !is_path_char(c));
        let after_ok = text[end..].chars().next().is_none_or(|c| c == '/' || !is_name_char(c));
        out.push_str(&text[from..at]);
        out.push_str(if before_ok && after_ok { name } else { DEFAULT_WORKSPACE });
        from = end;
    }
    out.push_str(&text[from..]);
    out
}

/// (#3654) How a path a tool touched is named in what the tool returns to
/// the model. Under the default root it is the path itself, so a
/// container's tool results never change; under a moved root it is the path
/// relative to the workspace ([`CURRENT_DIR`] for the workspace itself), or
/// only that it is outside the workspace: never the host path.
#[derive(PartialEq, Eq)]
pub enum Shown {
    Path(PathBuf),
    Relative(PathBuf),
    Outside,
}

/// [`Shown`] for `path` under this run's [`root`].
pub fn shown(path: &Path) -> Shown {
    shown_under(path, &root())
}

/// [`Shown`] for `path` under the workspace root `root`.
pub fn shown_under(path: &Path, root: &Path) -> Shown {
    if !moved(root) {
        return Shown::Path(path.to_path_buf());
    }
    let canonical = root.canonicalize().ok();
    let inside = [Some(root), canonical.as_deref()].into_iter().flatten().find_map(|r| path.strip_prefix(r).ok());
    match inside {
        Some(rel) if rel.as_os_str().is_empty() => Shown::Relative(CURRENT_DIR.into()),
        Some(rel) => Shown::Relative(rel.to_path_buf()),
        None => Shown::Outside,
    }
}

/// What [`Shown::Outside`] reads as.
const OUTSIDE: &str = "a path outside the workspace";

impl std::fmt::Display for Shown {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Shown::Path(p) | Shown::Relative(p) => write!(f, "{}", p.display()),
            Shown::Outside => f.write_str(OUTSIDE),
        }
    }
}

/// `{:?}` as a path's own `Debug` (quoted), so a container's messages are
/// byte for byte what they were.
impl std::fmt::Debug for Shown {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Shown::Path(p) | Shown::Relative(p) => write!(f, "{p:?}"),
            Shown::Outside => f.write_str(OUTSIDE),
        }
    }
}

/// A character that can continue a file name (`/workspace2`, `/workspace_x`).
fn is_name_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_' || c == '-'
}

/// A character that can come before `/workspace` inside a longer path
/// (`/data/workspace`, `./workspace`).
fn is_path_char(c: char) -> bool {
    is_name_char(c) || c == '/' || c == '.' || c == '~'
}

/// Resolve and validate a path that the agent wants to **read**. The
/// path must already exist on disk so that `canonicalize` can resolve
/// it; the canonical resolution is then checked against the workspace
/// root.
///
/// `input` may be absolute (`/workspace/foo.txt`) or relative
/// (`foo.txt`, `subdir/bar.txt`, `subdir/../bar.txt`). Relative paths
/// resolve against `workspace_root`.
pub fn resolve_read(input: &str, workspace_root: &Path) -> Result<PathBuf> {
    if input.is_empty() {
        return Err(anyhow!("path is empty"));
    }

    let initial = if Path::new(input).is_absolute() {
        PathBuf::from(input)
    } else {
        workspace_root.join(input)
    };

    let canonical = initial
        .canonicalize()
        .with_context(|| format!("resolving path: {input}"))?;

    let canonical_root = workspace_root
        .canonicalize()
        .with_context(|| format!("workspace root unavailable: {:?}", shown(workspace_root)))?;

    if !canonical.starts_with(&canonical_root) {
        return Err(anyhow!(
            "path escapes workspace: {input} → {:?}", shown(&canonical)
        ));
    }

    Ok(canonical)
}

/// Resolve and validate a path that the agent wants to **write**. The
/// file itself may not exist yet, so we canonicalize the **parent** dir
/// (which must exist) and require that the parent is inside the
/// workspace root. The returned path is `canonical_parent / filename`.
///
/// Note: this rejects writes where the parent dir doesn't exist. Agents
/// that need to create deep paths should call Bash with `mkdir -p`
/// first, or future iterations can add an explicit auto-mkdir mode.
pub fn resolve_write(input: &str, workspace_root: &Path) -> Result<PathBuf> {
    if input.is_empty() {
        return Err(anyhow!("path is empty"));
    }

    let initial = if Path::new(input).is_absolute() {
        PathBuf::from(input)
    } else {
        workspace_root.join(input)
    };

    let filename = initial
        .file_name()
        .ok_or_else(|| anyhow!("path has no filename component: {input}"))?
        .to_owned();

    let parent = initial
        .parent()
        .ok_or_else(|| anyhow!("path has no parent component: {input}"))?;

    let canonical_parent = parent
        .canonicalize()
        .with_context(|| format!("resolving parent of {input}: {:?}", shown(parent)))?;

    let canonical_root = workspace_root
        .canonicalize()
        .with_context(|| format!("workspace root unavailable: {:?}", shown(workspace_root)))?;

    if !canonical_parent.starts_with(&canonical_root) {
        return Err(anyhow!(
            "write parent escapes workspace: {input} → {:?}", shown(&canonical_parent)
        ));
    }

    let target = canonical_parent.join(filename);

    // (#883) The parent is canonicalized + prefix-checked above, but the
    // final component is joined raw. If it already exists as a symlink, an
    // `fs::write` would FOLLOW it and could land outside the workspace —
    // e.g. an agent runs `ln -s /etc/passwd /workspace/x` via Bash, then
    // `write x`. lstat the final component (`symlink_metadata` does NOT
    // follow) and refuse to write through an existing symlink. `resolve_read`
    // is unaffected — it canonicalizes the full path, so a symlink target is
    // resolved before the prefix check.
    if let Ok(meta) = std::fs::symlink_metadata(&target) {
        if meta.file_type().is_symlink() {
            return Err(anyhow!(
                "refusing to write through a symlink: {input} → {:?}", shown(&target)
            ));
        }
    }

    Ok(target)
}

/// A path a tool refused because it is not a regular file: a directory, a
/// named pipe, a socket or a device. A named pipe with no writer blocks a
/// plain open for reading (and one with no reader an open for writing) for
/// ever, and a tool call has no deadline, so every file tool opens with
/// `O_NONBLOCK` and refuses anything but a regular file before it reads or
/// writes a byte.
#[derive(Debug)]
pub struct NotRegular {
    pub path: PathBuf,
    pub kind: &'static str,
}

impl std::fmt::Display for NotRegular {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:?} is {}, not a regular file; read, edit, write and search work only on regular files", shown(&self.path), self.kind)
    }
}

impl std::error::Error for NotRegular {}

/// A file a tool that takes a whole file refused, unread, for its size.
#[derive(Debug)]
pub struct TooLarge {
    pub path: PathBuf,
    pub size: u64,
    pub cap: u64,
}

impl std::fmt::Display for TooLarge {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{:?} is {} bytes, over the {}-byte limit of a tool that takes a whole file; work on it with `bash` instead",
            shown(&self.path), self.size, self.cap
        )
    }
}

impl std::error::Error for TooLarge {}

/// What a file is, in words, when it is not a regular file.
#[cfg(unix)]
fn irregular_kind(file_type: std::fs::FileType) -> Option<&'static str> {
    use std::os::unix::fs::FileTypeExt;
    let kinds = [
        (file_type.is_file(), ""),
        (file_type.is_dir(), "a directory"),
        (file_type.is_fifo(), "a named pipe (FIFO)"),
        (file_type.is_socket(), "a socket"),
        (file_type.is_char_device(), "a character device"),
        (file_type.is_block_device(), "a block device"),
    ];
    match kinds.iter().find(|(is, _)| *is) {
        Some((_, "")) => None,
        Some((_, kind)) => Some(kind),
        None => Some("not a regular file"),
    }
}

#[cfg(not(unix))]
fn irregular_kind(file_type: std::fs::FileType) -> Option<&'static str> {
    (!file_type.is_file()).then_some(if file_type.is_dir() { "a directory" } else { "not a regular file" })
}

/// `Err(NotRegular)` when `meta` is not a regular file's.
fn regular(path: &Path, meta: &std::fs::Metadata) -> Result<()> {
    match irregular_kind(meta.file_type()) {
        Some(kind) => Err(NotRegular { path: path.to_path_buf(), kind }.into()),
        None => Ok(()),
    }
}

/// `options` plus `O_NOFOLLOW` (a symlink planted at the final component,
/// e.g. by a TOCTOU race, is refused by the kernel without following it,
/// #3074) and `O_NONBLOCK` (a named pipe swapped in after a check cannot
/// block the open; a regular file ignores it).
fn no_follow_no_block(options: &mut std::fs::OpenOptions) -> &mut std::fs::OpenOptions {
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
    }
    #[cfg(not(unix))]
    {
        options
    }
}

/// Open `path` for reading, refusing anything but a regular file before a
/// byte is read: the file and its size when it was opened.
fn open_regular(path: &Path) -> Result<(std::fs::File, u64)> {
    let file = no_follow_no_block(std::fs::OpenOptions::new().read(true))
        .open(path)
        .with_context(|| format!("opening {:?} with O_NOFOLLOW", shown(path)))?;
    let meta = file.metadata().with_context(|| format!("stat {:?}", shown(path)))?;
    regular(path, &meta)?;
    Ok((file, meta.len()))
}

/// What [`read_regular`] read of a file: at most its first `cap` bytes,
/// whether that is the whole file, and its size (what it held when opened,
/// or more if it grew while being read).
#[derive(Debug)]
pub struct FileStart {
    pub bytes: Vec<u8>,
    pub whole: bool,
    pub size: u64,
}

/// At most the first `cap` bytes of the regular file at `path`, never
/// holding more than that (and one byte to tell whether there is more).
pub fn read_regular(path: &Path, cap: u64) -> Result<FileStart> {
    let (file, size) = open_regular(path)?;
    read_capped(file, size, cap, path)
}

/// At most the first `cap` bytes of `file`, whose size at its open was
/// `size`.
fn read_capped(file: std::fs::File, size: u64, cap: u64, path: &Path) -> Result<FileStart> {
    use std::io::Read;
    let mut bytes = Vec::with_capacity(size.min(cap) as usize);
    file.take(cap.saturating_add(1))
        .read_to_end(&mut bytes)
        .with_context(|| format!("reading {:?}", shown(path)))?;
    let whole = bytes.len() as u64 <= cap;
    let size = size.max(bytes.len() as u64);
    bytes.truncate(cap as usize);
    Ok(FileStart { bytes, whole, size })
}

/// The whole of the regular file at `path`, refused unread ([`TooLarge`])
/// when it is over `cap` bytes.
pub fn read_whole_regular_bytes(path: &Path, cap: u64) -> Result<Vec<u8>> {
    let (file, size) = open_regular(path)?;
    if size > cap {
        return Err(TooLarge { path: path.to_path_buf(), size, cap }.into());
    }
    let start = read_capped(file, size, cap, path)?;
    if !start.whole {
        return Err(TooLarge { path: path.to_path_buf(), size: start.size, cap }.into());
    }
    Ok(start.bytes)
}

/// [`read_whole_regular_bytes`] as text.
pub fn read_whole_regular(path: &Path, cap: u64) -> Result<String> {
    String::from_utf8(read_whole_regular_bytes(path, cap)?).with_context(|| format!("reading {:?}: not UTF-8 text", shown(path)))
}

/// Open and write to a regular file, never through a symlink at the final
/// component and never blocking on, or writing to, a named pipe or device
/// ([`no_follow_no_block`], [`NotRegular`]). A named pipe already there is
/// named as one (an open of one with no reader would fail with `ENXIO`).
pub fn write_file_nofollow(path: &Path, content: &[u8]) -> Result<()> {
    use std::io::Write;
    if let Ok(meta) = std::fs::symlink_metadata(path) {
        if !meta.file_type().is_symlink() {
            regular(path, &meta)?;
        }
    }
    let mut file = open_regular_for_write(path)?;
    file.write_all(content)
        .with_context(|| format!("writing to {:?}", shown(path)))?;
    file.flush()
        .with_context(|| format!("flushing {:?}", shown(path)))?;
    Ok(())
}

/// Open `path` to write it whole, refusing what the open finds if it is
/// not a regular file (one swapped in after any earlier check).
fn open_regular_for_write(path: &Path) -> Result<std::fs::File> {
    let file = no_follow_no_block(std::fs::OpenOptions::new().write(true).create(true).truncate(true))
        .open(path)
        .with_context(|| format!("opening {:?} with O_NOFOLLOW", shown(path)))?;
    regular(path, &file.metadata().with_context(|| format!("stat {:?}", shown(path)))?)?;
    Ok(file)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::os::unix::fs::symlink;

    /// (#3654) Under the default root, model-facing text is byte for byte
    /// what it was (contract 6); under a moved one, every mention of
    /// `/workspace` names the current directory and never the root's path.
    #[test]
    #[serial_test::serial(runtime_workspace_env)]
    fn model_facing_text_names_the_current_directory_only_when_the_root_moved() {
        let text = "runs with cwd=/workspace; paths inside /workspace/...";
        std::env::remove_var(WORKSPACE_ENV);
        assert_eq!(naming_root(text), text);
        assert_eq!(describing_root(text), text);
        std::env::set_var(WORKSPACE_ENV, "/Users/op/ws");
        let (named, described) = (naming_root(text), describing_root(text));
        // A sentence no phrase covers still names no path of the host's.
        let uncovered = describing_root("then read /workspace/a.rs");
        std::env::remove_var(WORKSPACE_ENV);
        assert_eq!(named, "runs with cwd=.; paths inside ./...");
        assert_eq!(described, "runs with the workspace as its current directory; paths inside the workspace/...");
        assert_eq!(uncovered, "then read ./a.rs");
    }

    /// (#3654) The user's message is renamed at a path boundary, to the
    /// current directory: a mission config copied before `${WORKSPACE_ROOT}`
    /// says "mounted at `/workspace/<source>`". (contract 6) `--system` is
    /// not: the host hashed it, so it already names the workspace for its
    /// sandbox (`darkmux-crew`'s `system_prompt_as_sent`) and is sent as
    /// passed.
    #[test]
    fn the_users_message_is_renamed_and_the_system_prompt_is_sent_as_passed() {
        let prompt = naming("the repository is mounted at `/workspace/<source>`, not /workspace2", CURRENT_DIR);
        assert_eq!(prompt, "the repository is mounted at `./<source>`, not /workspace2");
        let entry = include_str!("../lib.rs");
        assert!(entry.contains("let prompt = tools::workspace::naming_root(&prompt);"), "the entry renames the user's message");
        assert!(
            entry.contains("let system_prompt = args.system.take().unwrap_or_else(|| tools::workspace::describing_root(&default_system_prompt()));"),
            "the entry sends `--system` as passed and renames only its own default"
        );
    }

    /// (#3112 review) `/workspace` is renamed only where it is the path
    /// itself, at a boundary on both sides: never inside `/workspace2`,
    /// `/workspaces`, `/workspace_x` or `/data/workspace`.
    #[test]
    fn workspace_is_renamed_only_at_a_path_boundary() {
        let renamed = |t: &str| naming(t, CURRENT_DIR);
        assert_eq!(renamed("/workspace"), ".");
        assert_eq!(renamed("at `/workspace/<source>`."), "at `./<source>`.");
        assert_eq!(renamed("cd /workspace && ls /workspace/src"), "cd . && ls ./src");
        assert_eq!(renamed("(/workspace), /workspace."), "(.), ..");
        for kept in ["/workspace2/x", "/workspaces", "/workspace_x", "/workspace-y", "/data/workspace/x", "a/workspace"] {
            assert_eq!(renamed(kept), kept, "{kept}");
        }
        // A directory named `workspace` inside the workspace: only the root
        // is renamed, the next match's boundary read from the whole text.
        assert_eq!(renamed("/workspace/workspace/x"), "./workspace/x");
        assert_eq!(renamed("/workspace2/workspace"), "/workspace2/workspace");
    }

    /// (#3654) Every sentence [`HOST_PHRASES`] rewrites is one the runtime
    /// itself says (a tool description, a parameter's, its default system
    /// prompt), so none is dead and each says what it was written for.
    #[test]
    fn every_host_phrase_matches_the_runtimes_own_text() {
        let mut own = vec![crate::default_system_prompt()];
        for tool in crate::tools::Tool::ALL {
            own.push(tool.description().to_string());
            own.push(tool.parameters_schema().to_string());
        }
        for (from, _) in HOST_PHRASES {
            assert!(own.iter().any(|t| t.contains(from)), "{from:?} matches none of the runtime's own text");
        }
    }

    /// (#3112 step 2, #3654) The root is the env override when set and
    /// non-empty, else `/workspace`. The tool definitions the model sees name
    /// `/workspace` under the default root and, under a moved one, the
    /// current directory: never `/workspace`, never the root's path.
    #[test]
    #[serial_test::serial(runtime_workspace_env)]
    fn the_root_and_the_tool_definitions_follow_the_env_override() {
        std::env::remove_var(WORKSPACE_ENV);
        assert_eq!(root(), Path::new(DEFAULT_WORKSPACE));
        let read = serde_json::to_string(&crate::tools::Tool::Read.to_tool_def()).unwrap();
        assert!(read.contains("/workspace/"), "{read}");
        std::env::set_var(WORKSPACE_ENV, "");
        assert_eq!(root(), Path::new(DEFAULT_WORKSPACE), "empty is unset");
        std::env::set_var(WORKSPACE_ENV, "/Users/op/ws");
        assert_eq!(root(), Path::new("/Users/op/ws"));
        let defs: Vec<String> = crate::tools::Tool::ALL.iter().map(|t| serde_json::to_string(&t.to_tool_def()).unwrap()).collect();
        let default_prompt = describing_root(&crate::default_system_prompt());
        std::env::remove_var(WORKSPACE_ENV);
        for text in defs.iter().chain([&default_prompt]) {
            assert!(!text.contains("/workspace") && !text.contains("/Users/op"), "{text}");
        }
        let bash = &defs[crate::tools::Tool::ALL.iter().position(|t| t.name() == "bash").unwrap()];
        assert!(bash.contains("Runs with the workspace as its current directory."), "{bash}");
    }

    /// (#3654) A path in a tool's result is the path itself under the
    /// default root (a container's results are unchanged), and under a
    /// moved one is relative to the workspace, `.` for the workspace itself,
    /// or only "outside the workspace": never the host path, by either
    /// spelling of the root.
    #[test]
    fn a_path_is_shown_relative_to_a_moved_root_and_as_itself_in_a_container() {
        let ws = fresh_workspace();
        let real = ws.path().canonicalize().unwrap();
        let container = shown_under(Path::new("/workspace/src/a.rs"), Path::new(DEFAULT_WORKSPACE));
        assert_eq!(format!("{container:?} {container}"), format!("{:?} /workspace/src/a.rs", Path::new("/workspace/src/a.rs")));
        for root in [ws.path(), real.as_path()] {
            for inside in [root.join("src/a.rs"), real.join("src/a.rs")] {
                let s = shown_under(&inside, root);
                assert_eq!(format!("{s:?} {s}"), "\"src/a.rs\" src/a.rs");
            }
            assert_eq!(format!("{}", shown_under(&real, root)), ".");
            let outside = shown_under(Path::new("/Users/op/.ssh/id"), root);
            assert_eq!(format!("{outside:?} {outside}"), "a path outside the workspace a path outside the workspace");
        }
    }

    /// (#3654) A refusal a file tool returns under a moved root names no
    /// path of the host's: an escape, a missing parent, a symlink, a
    /// directory, a file over a cap.
    #[test]
    #[serial_test::serial(runtime_workspace_env)]
    fn a_refusal_under_a_moved_root_names_no_host_path() {
        let ws = fresh_workspace();
        let outside = fresh_workspace();
        fs::write(outside.path().join("secret"), "s").unwrap();
        symlink(outside.path().join("secret"), ws.path().join("link")).unwrap();
        fs::create_dir(ws.path().join("dir")).unwrap();
        fs::write(ws.path().join("big"), "0123456789").unwrap();
        std::env::set_var(WORKSPACE_ENV, ws.path());
        let errors = [
            resolve_read(&format!("../{}/secret", outside.path().file_name().unwrap().to_string_lossy()), ws.path()).unwrap_err(),
            resolve_read("link", ws.path()).unwrap_err(),
            resolve_write("missing/x", ws.path()).unwrap_err(),
            resolve_write("link", ws.path()).unwrap_err(),
            read_regular(&ws.path().canonicalize().unwrap().join("dir"), 4).unwrap_err(),
            read_whole_regular(&ws.path().canonicalize().unwrap().join("big"), 4).unwrap_err(),
        ]
        .map(|e| format!("{e:#}"));
        std::env::remove_var(WORKSPACE_ENV);
        let real = ws.path().canonicalize().unwrap();
        let tmp = std::env::temp_dir().canonicalize().unwrap();
        for text in errors {
            for private in [ws.path(), real.as_path(), outside.path(), tmp.as_path()] {
                assert!(!text.contains(&private.display().to_string()), "{text}");
            }
        }
    }

    /// Create a fresh tempdir to serve as the workspace root for one
    /// test. Returns the dir; drop = cleanup.
    fn fresh_workspace() -> tempfile::TempDir {
        tempfile::Builder::new().prefix("darkmux-runtime-workspace-test").tempdir()
            .expect("create tempdir")
    }

    #[test]
    fn read_accepts_existing_file_inside_workspace() {
        let ws = fresh_workspace();
        let file = ws.path().join("hello.txt");
        fs::write(&file, b"hi").unwrap();

        let result = resolve_read("hello.txt", ws.path()).unwrap();
        assert_eq!(result, file.canonicalize().unwrap());
    }

    #[test]
    fn read_accepts_nested_path() {
        let ws = fresh_workspace();
        let sub = ws.path().join("sub");
        fs::create_dir(&sub).unwrap();
        let file = sub.join("nested.txt");
        fs::write(&file, b"nested").unwrap();

        let result = resolve_read("sub/nested.txt", ws.path()).unwrap();
        assert_eq!(result, file.canonicalize().unwrap());
    }

    #[test]
    fn read_rejects_dotdot_escape() {
        let ws = fresh_workspace();
        // Need a file at /tmp/foo to test against
        let outside = ws.path().parent().unwrap().join("escape-target.txt");
        let _ = fs::write(&outside, b"shouldnt see this");

        // Relative escape via ..
        let escape_attempt = format!("../{}", outside.file_name().unwrap().to_string_lossy());
        let err = resolve_read(&escape_attempt, ws.path()).unwrap_err();
        assert!(
            err.to_string().contains("escapes workspace"),
            "expected escape error, got: {err}"
        );

        let _ = fs::remove_file(&outside);
    }

    #[test]
    fn read_rejects_absolute_path_outside_workspace() {
        let ws = fresh_workspace();
        // /etc/hosts is present on macOS, Linux, and Alpine — more
        // reliable cross-platform than /etc/hostname (which is
        // missing on stock macOS).
        let err = resolve_read("/etc/hosts", ws.path()).unwrap_err();
        assert!(
            err.to_string().contains("escapes workspace"),
            "expected escape error, got: {err}"
        );
    }

    #[test]
    fn read_rejects_symlink_target_outside_workspace() {
        let ws = fresh_workspace();
        let outside = ws.path().parent().unwrap().join("link-target.txt");
        fs::write(&outside, b"secret").unwrap();
        let link = ws.path().join("leak");
        symlink(&outside, &link).unwrap();

        let err = resolve_read("leak", ws.path()).unwrap_err();
        assert!(
            err.to_string().contains("escapes workspace"),
            "expected escape error, got: {err}"
        );

        let _ = fs::remove_file(&outside);
    }

    #[test]
    fn read_rejects_empty_path() {
        let ws = fresh_workspace();
        let err = resolve_read("", ws.path()).unwrap_err();
        assert!(err.to_string().contains("empty"));
    }

    #[test]
    fn read_rejects_nonexistent_file() {
        let ws = fresh_workspace();
        let err = resolve_read("nope.txt", ws.path()).unwrap_err();
        // canonicalize() returns IO error for nonexistent paths;
        // we wrap it with context so the message includes "resolving path"
        assert!(err.to_string().contains("resolving path"));
    }

    #[test]
    fn read_rejects_workspace2_lookalike_prefix() {
        // Build a workspace dir, and a sibling whose name STARTS with
        // the workspace dir's name as a substring. Path::starts_with
        // does component-wise matching, so the lookalike should fail
        // even though string-prefix would pass.
        let tmp = tempfile::Builder::new().prefix("darkmux-lookalike").tempdir().unwrap();
        let workspace = tmp.path().join("workspace");
        let workspace2 = tmp.path().join("workspace2");
        fs::create_dir(&workspace).unwrap();
        fs::create_dir(&workspace2).unwrap();
        let target = workspace2.join("foo.txt");
        fs::write(&target, b"x").unwrap();

        let absolute = target.to_string_lossy().into_owned();
        let err = resolve_read(&absolute, &workspace).unwrap_err();
        assert!(
            err.to_string().contains("escapes workspace"),
            "expected escape error, got: {err}"
        );
    }

    #[test]
    fn write_accepts_new_file_inside_workspace() {
        let ws = fresh_workspace();
        let result = resolve_write("new-file.txt", ws.path()).unwrap();
        // file doesn't exist yet but parent is workspace
        assert!(result.starts_with(ws.path().canonicalize().unwrap()));
        assert_eq!(result.file_name().unwrap(), "new-file.txt");
    }

    #[test]
    fn write_rejects_dotdot_escape() {
        let ws = fresh_workspace();
        let err = resolve_write("../escape.txt", ws.path()).unwrap_err();
        assert!(
            err.to_string().contains("escapes workspace"),
            "expected escape error, got: {err}"
        );
    }

    #[test]
    fn write_rejects_absolute_outside() {
        let ws = fresh_workspace();
        let err = resolve_write("/tmp/escape.txt", ws.path()).unwrap_err();
        assert!(
            err.to_string().contains("escapes workspace"),
            "expected escape error, got: {err}"
        );
    }

    #[test]
    fn write_rejects_path_through_symlink_escape() {
        // Create a symlink IN the workspace pointing to a directory outside
        // the workspace, then try to write through it.
        let ws = fresh_workspace();
        let outside_dir = ws.path().parent().unwrap().join("outside-dir");
        fs::create_dir(&outside_dir).unwrap();
        let leak = ws.path().join("leak");
        symlink(&outside_dir, &leak).unwrap();

        let err = resolve_write("leak/sneak.txt", ws.path()).unwrap_err();
        assert!(
            err.to_string().contains("escapes workspace"),
            "expected escape error, got: {err}"
        );

        let _ = fs::remove_dir(&outside_dir);
    }

    #[test]
    fn write_rejects_symlink_at_final_component() {
        // (#883) The FINAL write component is itself a symlink pointing
        // outside the workspace. Unlike the parent-segment case above, the
        // parent (`ws`) is inside the workspace and passes the prefix check —
        // only an lstat on the final component catches it. Pre-fix this
        // returned Ok and `fs::write` would have followed the link and
        // clobbered the outside file.
        let ws = fresh_workspace();
        let outside = ws.path().parent().unwrap().join("final-link-target.txt");
        fs::write(&outside, b"host secret").unwrap();
        let link = ws.path().join("leak");
        symlink(&outside, &link).unwrap();

        let err = resolve_write("leak", ws.path()).unwrap_err();
        assert!(
            err.to_string().contains("symlink"),
            "expected symlink refusal, got: {err}"
        );

        let _ = fs::remove_file(&outside);
    }

    #[test]
    fn write_rejects_when_parent_dir_doesnt_exist() {
        let ws = fresh_workspace();
        let err = resolve_write("does-not-exist/file.txt", ws.path()).unwrap_err();
        assert!(err.to_string().contains("resolving parent"));
    }

    #[test]
    fn write_accepts_nested_path_when_parent_exists() {
        let ws = fresh_workspace();
        fs::create_dir(ws.path().join("subdir")).unwrap();
        let result = resolve_write("subdir/file.txt", ws.path()).unwrap();
        assert!(result.starts_with(ws.path().canonicalize().unwrap()));
        assert_eq!(result.file_name().unwrap(), "file.txt");
    }

    #[test]
    fn write_file_nofollow_refuses_to_write_through_symlink() {
        let ws = fresh_workspace();
        let outside = ws.path().parent().unwrap().join("escaped-target.txt");
        fs::write(&outside, b"original").unwrap();
        let link = ws.path().join("link-to-outside.txt");
        symlink(&outside, &link).unwrap();

        let err = write_file_nofollow(&link, b"overwrite").unwrap_err();
        assert!(
            err.to_string().contains("O_NOFOLLOW")
                || format!("{err:?}").contains("Too many levels of symbolic links")
        );
        assert_eq!(fs::read_to_string(&outside).unwrap(), "original");
        let _ = fs::remove_file(&outside);
    }

    /// A file of exactly the cap is whole; one byte more is not, and is
    /// cut to the cap. The whole-file reader takes the first and refuses
    /// the second.
    #[test]
    fn a_file_of_exactly_the_cap_is_whole_and_one_byte_more_is_not() {
        let ws = fresh_workspace();
        let (at, over) = (ws.path().join("at"), ws.path().join("over"));
        fs::write(&at, vec![b'a'; 64]).unwrap();
        fs::write(&over, vec![b'a'; 65]).unwrap();
        let exact = read_regular(&at, 64).unwrap();
        assert!(exact.whole && exact.bytes.len() == 64 && exact.size == 64);
        let more = read_regular(&over, 64).unwrap();
        assert!(!more.whole && more.bytes.len() == 64 && more.size == 65);
        assert_eq!(read_whole_regular_bytes(&at, 64).unwrap().len(), 64);
        let err = read_whole_regular_bytes(&over, 64).unwrap_err();
        assert!(err.downcast_ref::<TooLarge>().is_some(), "{err:#}");
    }

    /// A named pipe that has a reader opens for writing at once: the check
    /// after the open, not the one before it, is what refuses it.
    #[test]
    fn the_write_open_refuses_a_named_pipe_that_has_a_reader() {
        use std::os::unix::fs::OpenOptionsExt;
        let ws = fresh_workspace();
        let pipe = ws.path().join("p");
        let c = std::ffi::CString::new(pipe.as_os_str().as_encoded_bytes()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o600) }, 0);
        let _reader = fs::OpenOptions::new().read(true).custom_flags(libc::O_NONBLOCK).open(&pipe).unwrap();
        let err = open_regular_for_write(&pipe).unwrap_err();
        assert!(err.downcast_ref::<NotRegular>().is_some(), "{err:#}");
        assert!(err.to_string().contains("named pipe"), "{err:#}");
    }

    #[test]
    fn read_regular_refuses_to_read_through_symlink() {
        let ws = fresh_workspace();
        let outside = ws.path().parent().unwrap().join("read-target.txt");
        fs::write(&outside, b"original").unwrap();
        let link = ws.path().join("link-to-read.txt");
        symlink(&outside, &link).unwrap();

        let err = read_regular(&link, 1024).unwrap_err();
        assert!(
            err.to_string().contains("O_NOFOLLOW")
                || format!("{err:?}").contains("Too many levels of symbolic links")
        );
        let _ = fs::remove_file(&outside);
    }
}
