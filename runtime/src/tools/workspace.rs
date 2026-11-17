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

/// (#3112) Env: the mounts other than the workspace and the out-dir that a
/// sandbox on the host sees away from their container path, as a JSON array
/// of `[container path, host path]` pairs (a mod's attachments, the shared
/// toolchain cache). Never set for a container.
pub const PATHS_ENV: &str = "DARKMUX_RUNTIME_PATHS";

/// The pairs [`PATHS_ENV`] names, none when it is unset or empty, or why
/// it does not parse.
pub fn extra_placements() -> Result<Vec<(String, String)>> {
    match std::env::var(PATHS_ENV) {
        Ok(v) if !v.is_empty() => serde_json::from_str(&v)
            .with_context(|| format!("{PATHS_ENV} is not a JSON array of [container path, host path] pairs")),
        _ => Ok(Vec::new()),
    }
}

/// Where this run sees each mount a model is told about, as `(the path the
/// model is shown, the path on this machine)`: the workspace at `root`, the
/// out-dir, and [`extra_placements`]. Only the mounts placed away from their
/// container path, so none in a container.
///
/// The model is shown the container path on every backend (contract 6: one
/// system prompt, one set of tool descriptions, whatever the sandbox; and no
/// host path, which names the user's home folder, in a request body). The
/// runtime maps between the two: [`to_host`] for a command the model wrote,
/// [`on_host`] for a path a file tool is given, [`to_model`] for what a tool
/// hands back.
pub fn placements(root: &Path) -> Vec<(String, String)> {
    let out = crate::trajectory::out_base().to_string_lossy().into_owned();
    let mut all = vec![
        (DEFAULT_WORKSPACE.to_string(), root.to_string_lossy().into_owned()),
        (crate::trajectory::RUNTIME_OUT_BASE.to_string(), out),
    ];
    all.extend(extra_placements().unwrap_or_default());
    all.retain(|(at, host)| is_placed_elsewhere(at, host));
    all
}

/// A pair worth mapping: two different absolute paths, neither of them `/`.
fn is_placed_elsewhere(at: &str, host: &str) -> bool {
    at != host && [at, host].iter().all(|p| p.len() > 1 && p.starts_with('/'))
}

/// `text` (a command the model wrote) with every path it names that the
/// model was shown ([`placements`]) named where it is on this machine.
/// Unchanged in a container.
pub fn to_host(text: &str, root: &Path) -> String {
    rename(text, &placements(root))
}

/// `text` (what a tool hands back) with every host path of a mount
/// ([`placements`] under this run's [`root`]), as given or resolved
/// (`/var` is `/private/var` on macOS), named as the path the model was
/// shown. Unchanged in a container.
pub fn to_model(text: &str) -> String {
    let mut back: Vec<(String, String)> = Vec::new();
    for (at, host) in placements(&root()) {
        back.extend(resolved(&host).map(|real| (real, at.clone())));
        back.push((host, at));
    }
    back.retain(|(host, at)| is_placed_elsewhere(host, at));
    rename(text, &back)
}

/// `host` with its links resolved, when that is another path.
fn resolved(host: &str) -> Option<String> {
    let real = Path::new(host).canonicalize().ok()?.to_string_lossy().into_owned();
    (real != host).then_some(real)
}

/// A path a file tool was given, on this machine: one under the workspace
/// the model was shown (`/workspace/...`) is under `root`; any other is
/// absolute as given or relative to `root`. Unchanged in a container.
pub fn on_host(input: &str, root: &Path) -> PathBuf {
    let path = Path::new(input);
    match path.strip_prefix(DEFAULT_WORKSPACE) {
        Ok(rest) if root != Path::new(DEFAULT_WORKSPACE) => root.join(rest),
        _ => root.join(path),
    }
}

/// `text` with each `from` of `pairs` written as its `to`, wherever it is
/// the path itself (#3112 review): preceded by the start or a character
/// that cannot be part of a path, and followed by the end, `/`, or a
/// character that cannot continue a name. `/workspace2`, `/workspaces`,
/// `/workspace_x` and `/data/workspace` are other paths and are left alone.
/// One pass, the longest `from` first where two start at one place, so a
/// replacement is never itself rewritten.
fn rename(text: &str, pairs: &[(String, String)]) -> String {
    if pairs.is_empty() {
        return text.to_string();
    }
    let mut longest_first: Vec<&(String, String)> = pairs.iter().collect();
    longest_first.sort_by_key(|(from, _)| std::cmp::Reverse(from.len()));
    let mut out = String::with_capacity(text.len());
    let mut from = 0;
    for (at, _) in text.match_indices('/') {
        if at < from {
            continue;
        }
        if let Some((name, to)) = longest_first.iter().find(|(name, _)| names_path_at(text, at, name)) {
            out.push_str(&text[from..at]);
            out.push_str(to);
            from = at + name.len();
        }
    }
    out.push_str(&text[from..]);
    out
}

/// Whether `text` names the path `name` itself at byte `at`. The boundary
/// is read from the whole text, never the slice after the last match: in
/// `/workspace/workspace` the second one follows an `e`, a path character,
/// so it is a directory inside.
fn names_path_at(text: &str, at: usize, name: &str) -> bool {
    let end = at + name.len();
    text[at..].starts_with(name)
        && text[..at].chars().next_back().is_none_or(|c| !is_path_char(c))
        && text[end..].chars().next().is_none_or(|c| c == '/' || !is_name_char(c))
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

    let initial = on_host(input, workspace_root);

    let canonical = initial
        .canonicalize()
        .with_context(|| format!("resolving path: {input}"))?;

    let canonical_root = workspace_root
        .canonicalize()
        .with_context(|| format!("workspace root unavailable: {workspace_root:?}"))?;

    if !canonical.starts_with(&canonical_root) {
        // Where it leads is outside every mount, so on a host it is a host
        // path, which a model is never shown: the path as given says which.
        return Err(anyhow!("path escapes workspace: {input}"));
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

    let initial = on_host(input, workspace_root);

    let filename = initial
        .file_name()
        .ok_or_else(|| anyhow!("path has no filename component: {input}"))?
        .to_owned();

    let parent = initial
        .parent()
        .ok_or_else(|| anyhow!("path has no parent component: {input}"))?;

    let canonical_parent = parent
        .canonicalize()
        .with_context(|| format!("resolving parent of {input}: {parent:?}"))?;

    let canonical_root = workspace_root
        .canonicalize()
        .with_context(|| format!("workspace root unavailable: {workspace_root:?}"))?;

    if !canonical_parent.starts_with(&canonical_root) {
        return Err(anyhow!("write parent escapes workspace: {input}"));
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
                "refusing to write through a symlink: {input} → {target:?}"
            ));
        }
    }

    Ok(target)
}

/// (#3074) Open and write to a file with `O_NOFOLLOW` so a symlink planted
/// at the final component (e.g. via a TOCTOU race) is rejected atomically
/// by the kernel without following it.
#[cfg(unix)]
pub fn write_file_nofollow(path: &Path, content: &[u8]) -> Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
        .with_context(|| format!("opening {path:?} with O_NOFOLLOW"))?;
    file.write_all(content)
        .with_context(|| format!("writing to {path:?}"))?;
    file.flush()
        .with_context(|| format!("flushing {path:?}"))?;
    Ok(())
}

#[cfg(not(unix))]
pub fn write_file_nofollow(path: &Path, content: &[u8]) -> Result<()> {
    std::fs::write(path, content)
        .with_context(|| format!("writing to {path:?}"))
}

/// Open and read a file with `O_NOFOLLOW` to guarantee symlinks are not followed on read.
#[cfg(unix)]
pub fn read_file_nofollow(path: &Path) -> Result<String> {
    use std::io::Read;
    use std::os::unix::fs::OpenOptionsExt;
    let mut file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
        .with_context(|| format!("opening {path:?} with O_NOFOLLOW"))?;
    let mut s = String::new();
    file.read_to_string(&mut s)
        .with_context(|| format!("reading {path:?}"))?;
    Ok(s)
}

#[cfg(not(unix))]
pub fn read_file_nofollow(path: &Path) -> Result<String> {
    std::fs::read_to_string(path)
        .with_context(|| format!("reading {path:?}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::os::unix::fs::symlink;

    fn pairs(list: &[(&str, &str)]) -> Vec<(String, String)> {
        list.iter().map(|(a, b)| (a.to_string(), b.to_string())).collect()
    }

    /// (#3112) A command the model wrote names the workspace where this
    /// sandbox sees it; in a container nothing changes.
    #[test]
    #[serial_test::serial(runtime_workspace_env)]
    fn a_command_names_the_workspace_where_the_sandbox_sees_it() {
        std::env::remove_var(PATHS_ENV);
        let text = "cd /workspace && cat /workspace/src/x.rs";
        assert_eq!(to_host(text, Path::new(DEFAULT_WORKSPACE)), text);
        assert_eq!(to_host(text, Path::new("/Users/op/ws")), "cd /Users/op/ws && cat /Users/op/ws/src/x.rs");
    }

    /// (#3112 review) A path is renamed only where it is the path itself, at
    /// a boundary on both sides: never inside `/workspace2`, `/workspaces`,
    /// `/workspace_x` or `/data/workspace`.
    #[test]
    fn a_path_is_renamed_only_at_a_path_boundary() {
        let renamed = |t: &str| rename(t, &pairs(&[("/workspace", "/Users/op/ws")]));
        assert_eq!(renamed("/workspace"), "/Users/op/ws");
        assert_eq!(renamed("at `/workspace/<source>`."), "at `/Users/op/ws/<source>`.");
        assert_eq!(renamed("cd /workspace && ls /workspace/src"), "cd /Users/op/ws && ls /Users/op/ws/src");
        assert_eq!(renamed("(/workspace), /workspace."), "(/Users/op/ws), /Users/op/ws.");
        for kept in ["/workspace2/x", "/workspaces", "/workspace_x", "/workspace-y", "/data/workspace/x", "a/workspace"] {
            assert_eq!(renamed(kept), kept, "{kept}");
        }
        // A directory named `workspace` inside the workspace: only the root
        // is renamed, the next match's boundary read from the whole text.
        assert_eq!(renamed("/workspace/workspace/x"), "/Users/op/ws/workspace/x");
        assert_eq!(renamed("/workspace2/workspace"), "/workspace2/workspace");
    }

    /// One pass, longest first: a replacement is never rewritten by a
    /// shorter pair, and a host path inside a longer one (`/var/...` in
    /// `/private/var/...`) is left alone.
    #[test]
    fn renaming_is_one_pass_longest_first() {
        let back = pairs(&[("/var/t/ws", "/workspace"), ("/private/var/t/ws", "/workspace"), ("/workspace", "/x")]);
        assert_eq!(rename("/private/var/t/ws/a and /var/t/ws/b", &back), "/workspace/a and /workspace/b");
        let nested = pairs(&[("/workspace", "/h/ws"), ("/workspace/.git", "/h/git")]);
        assert_eq!(rename("/workspace/.git/HEAD /workspace/a", &nested), "/h/git/HEAD /h/ws/a");
    }

    /// (#3112) What a tool hands back names each mount where the model was
    /// shown it, the host path as given or resolved; the out-dir and the
    /// extra mounts too. In a container nothing changes.
    #[test]
    #[serial_test::serial(runtime_workspace_env)]
    fn what_a_tool_hands_back_names_the_paths_the_model_was_shown() {
        let ws = tempfile::tempdir().unwrap();
        let host = ws.path().display().to_string();
        let real = ws.path().canonicalize().unwrap().display().to_string();
        let text = format!("pwd: {real}\nfile: {host}/a.rs\nout: /h/out/x\nmods: /h/mods/k/f\nother: /etc/hosts");
        std::env::remove_var(WORKSPACE_ENV);
        std::env::remove_var(PATHS_ENV);
        std::env::remove_var(crate::trajectory::OUT_DIR_ENV);
        assert_eq!(to_model(&text), text, "a container's text is unchanged");
        std::env::set_var(WORKSPACE_ENV, &host);
        std::env::set_var(crate::trajectory::OUT_DIR_ENV, "/h/out");
        std::env::set_var(PATHS_ENV, r#"[["/darkmux-mods/k","/h/mods/k"]]"#);
        let mapped = to_model(&text);
        std::env::remove_var(WORKSPACE_ENV);
        std::env::remove_var(PATHS_ENV);
        std::env::remove_var(crate::trajectory::OUT_DIR_ENV);
        assert_eq!(
            mapped,
            "pwd: /workspace\nfile: /workspace/a.rs\nout: /darkmux-out/x\nmods: /darkmux-mods/k/f\nother: /etc/hosts"
        );
    }

    /// Only two different absolute paths make a pair: never `/`, which
    /// would rename every path, and never a relative one.
    #[test]
    fn a_pair_is_two_different_absolute_paths_neither_the_filesystem_root() {
        assert!(is_placed_elsewhere("/workspace", "/Users/op/ws"));
        for (at, host) in [("/workspace", "/workspace"), ("/workspace", "/"), ("/", "/h"), ("/workspace", "ws"), ("", "/h")] {
            assert!(!is_placed_elsewhere(at, host), "{at:?} {host:?}");
        }
    }

    /// (#3112) A malformed [`PATHS_ENV`] is said, never read as no pairs.
    #[test]
    #[serial_test::serial(runtime_workspace_env)]
    fn a_malformed_paths_env_is_refused() {
        std::env::set_var(PATHS_ENV, "not json");
        let err = extra_placements().unwrap_err();
        std::env::remove_var(PATHS_ENV);
        assert!(format!("{err:#}").contains(PATHS_ENV), "{err:#}");
        assert!(extra_placements().unwrap().is_empty());
    }

    /// (#3112) A file tool's `/workspace/...` is under the root; any other
    /// path is as given or relative to the root.
    #[test]
    fn a_file_tools_path_is_under_the_root() {
        let root = Path::new("/Users/op/ws");
        assert_eq!(on_host("/workspace/a/b.rs", root), Path::new("/Users/op/ws/a/b.rs"));
        assert_eq!(on_host("a/b.rs", root), Path::new("/Users/op/ws/a/b.rs"));
        assert_eq!(on_host("/Users/op/ws/a", root), Path::new("/Users/op/ws/a"));
        assert_eq!(on_host("/workspace2/a", root), Path::new("/workspace2/a"));
        let container = Path::new(DEFAULT_WORKSPACE);
        assert_eq!(on_host("/workspace/a", container), Path::new("/workspace/a"));
        assert_eq!(on_host("a", container), Path::new("/workspace/a"));
    }

    /// (#3112 review) Line 3 of `templates/builtin/roles/tool-bench.md`.
    pub(crate) const TOOL_BENCH_WORKSPACE_SENTENCE: &str = "You are an assistant operating inside a container. A working directory is mounted at `/workspace` and you have tools to read files, search file contents, run shell commands, and write or edit files.";

    /// (#3112) A shipped role whose frozen prompt says the working directory
    /// is mounted at `/workspace` (`tool-bench`) reaches the model as
    /// written on every backend: the runtime's entry sends `--system` as
    /// given, and a command naming `/workspace` runs where the sandbox sees
    /// it. The sentence is held here rather than read from `templates/`,
    /// which is outside this crate (the PR-diff mutation job copies only
    /// `runtime/`); `darkmux-crew`'s
    /// `the_tool_bench_role_still_names_the_workspace_the_runtime_maps` keeps
    /// the two in step.
    #[test]
    fn a_shipped_role_prompt_naming_workspace_reaches_the_model_as_written() {
        let entry = include_str!("../lib.rs");
        assert!(
            entry.contains("let system_prompt = args.system.take().unwrap_or_else(default_system_prompt);"),
            "the runtime's entry sends `--system` as given"
        );
        assert!(TOOL_BENCH_WORKSPACE_SENTENCE.contains("`/workspace`"));
    }

    /// (#3112 step 2) The root is the env override when set and non-empty,
    /// else `/workspace`; the tool definitions the model sees name
    /// `/workspace` whichever it is (contract 6).
    #[test]
    #[serial_test::serial(runtime_workspace_env)]
    fn the_root_follows_the_env_override_and_the_tool_definitions_do_not() {
        std::env::remove_var(WORKSPACE_ENV);
        assert_eq!(root(), Path::new(DEFAULT_WORKSPACE));
        let read = serde_json::to_string(&crate::tools::Tool::Read.to_tool_def()).unwrap();
        assert!(read.contains("/workspace/"), "{read}");
        std::env::set_var(WORKSPACE_ENV, "");
        assert_eq!(root(), Path::new(DEFAULT_WORKSPACE), "empty is unset");
        std::env::set_var(WORKSPACE_ENV, "/Users/op/ws");
        assert_eq!(root(), Path::new("/Users/op/ws"));
        let moved = serde_json::to_string(&crate::tools::Tool::Read.to_tool_def()).unwrap();
        std::env::remove_var(WORKSPACE_ENV);
        assert_eq!(moved, read);
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

    #[test]
    fn read_file_nofollow_refuses_to_read_through_symlink() {
        let ws = fresh_workspace();
        let outside = ws.path().parent().unwrap().join("read-target.txt");
        fs::write(&outside, b"original").unwrap();
        let link = ws.path().join("link-to-read.txt");
        symlink(&outside, &link).unwrap();

        let err = read_file_nofollow(&link).unwrap_err();
        assert!(
            err.to_string().contains("O_NOFOLLOW")
                || format!("{err:?}").contains("Too many levels of symbolic links")
        );
        let _ = fs::remove_file(&outside);
    }
}
