//! The `--bundler` external-command contract: any executable that
//! implements `<cmd> --worktree <dir> --diff <file>` and emits a
//! [`super::BundleSet`] (the frozen JSON shape) on stdout can stand in
//! for the built-in Rust bundler. This is the escape hatch named in the
//! packet brief — a TypeScript-AST-based bundler, or a language-specific
//! one for a non-TS repo, plugs in here without touching the built-in
//! path.

use super::{Bundle, BundleSet, BundleSkipReport};
use anyhow::{anyhow, Context, Result};
use std::path::Path;
use std::process::Command;

/// (#2119) The exact phrase the `darkmux-bundler-rust` reference plugin
/// writes to stderr when a diff has no `.rs` hunks to bundle
/// (`plugins/darkmux-bundler-rust/src/main.rs`) — not a crash, not
/// malformed output, just "not my language." Any `--bundler` plugin whose
/// stderr contains this substring on a non-zero exit is read the same way
/// by [`is_plugin_decline`]: a decline, never a genuine bundler failure.
pub const PLUGIN_DECLINE_MARKER: &str = "nothing to bundle";

/// True when an [`external_bundles`] error is a plugin declaring it has
/// nothing to bundle for this diff (see [`PLUGIN_DECLINE_MARKER`]) rather
/// than a genuine failure (a crash, malformed output, an unreadable diff
/// file). `ReviewBundleStepKind::run_streaming` falls back to the
/// built-in bundler on `true`; propagates the error unchanged on `false`.
pub fn is_plugin_decline(err: &anyhow::Error) -> bool {
    err.to_string().contains(PLUGIN_DECLINE_MARKER)
}

/// Run `<cmd> --worktree <dir> --diff <file>` (worktree omitted when
/// `None`, for a `GithubApi`-sourced external bundler that has no local
/// checkout) and parse its stdout as a [`BundleSet`]. Validates LOUDLY —
/// non-JSON output, a missing required field, or an empty bundle set all
/// produce a named error rather than a silent empty result or a bare
/// serde parse failure.
pub fn external_bundles(cmd: &str, worktree: Option<&Path>, diff_path: &Path) -> Result<BundleSet> {
    let mut c = Command::new(cmd);
    if let Some(wt) = worktree {
        c.args(["--worktree", &wt.display().to_string()]);
    }
    c.args(["--diff", &diff_path.display().to_string()]);
    let output = c
        .output()
        .with_context(|| format!("running `{cmd} --worktree <dir> --diff {}`", diff_path.display()))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(anyhow!(
            "external bundler `{cmd}` exited with {}: {}",
            output.status,
            stderr.trim()
        ));
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    parse_bundle_set(&stdout).with_context(|| format!("parsing output of external bundler `{cmd}`"))
}

/// Parse + validate a `BundleSet` JSON document. Named errors for the
/// failure classes an external `--bundler` implementation is likely to
/// hit: non-JSON output, a bundle missing a required field, or an empty
/// bundle set (a bundler that silently produced nothing is a bug the
/// operator needs surfaced, not a quiet no-op).
fn parse_bundle_set(text: &str) -> Result<BundleSet> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return Err(anyhow!("external bundler produced no output"));
    }
    let value: serde_json::Value =
        serde_json::from_str(trimmed).map_err(|e| anyhow!("external bundler output is not valid JSON: {e}"))?;
    let bundles_value = value
        .get("bundles")
        .ok_or_else(|| anyhow!("external bundler output is missing the top-level `bundles` field"))?;
    let arr = bundles_value
        .as_array()
        .ok_or_else(|| anyhow!("`bundles` must be a JSON array"))?;
    if arr.is_empty() {
        return Err(anyhow!("external bundler produced an empty bundle set (0 bundles)"));
    }
    let mut bundles = Vec::with_capacity(arr.len());
    for (i, item) in arr.iter().enumerate() {
        let bundle: Bundle = serde_json::from_value(item.clone())
            .map_err(|e| anyhow!("bundles[{i}] is missing a required field or has the wrong shape: {e}"))?;
        if bundle.id.trim().is_empty() {
            return Err(anyhow!("bundles[{i}] has an empty `id`"));
        }
        if bundle.fact_family.trim().is_empty() {
            return Err(anyhow!("bundles[{i}] (id={}) has an empty `fact_family`", bundle.id));
        }
        if bundle.code.is_empty() {
            return Err(anyhow!("bundles[{i}] (id={}) has an empty `code` array", bundle.id));
        }
        bundles.push(bundle);
    }
    // (#1605) An external bundler has no notion of this crate's internal
    // decline bookkeeping — `skip` stays the honest empty default rather
    // than fabricating file-level accounting the plugin never reported.
    Ok(BundleSet { bundles, skip: BundleSkipReport::default() })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::TempDir;

    #[cfg(unix)]
    fn write_stub_script(dir: &Path, name: &str, body: &str) -> std::path::PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let path = dir.join(name);
        let mut f = std::fs::File::create(&path).unwrap();
        writeln!(f, "#!/bin/sh").unwrap();
        f.write_all(body.as_bytes()).unwrap();
        let mut perms = std::fs::metadata(&path).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&path, perms).unwrap();
        path
    }

    #[cfg(unix)]
    #[test]
    fn valid_stub_output_parses() {
        let dir = TempDir::new().unwrap();
        let script = write_stub_script(
            dir.path(),
            "good-bundler.sh",
            r#"cat <<'EOF'
{"bundles":[{"id":"foo@src/a.ts","code":[{"path":"src/a.ts","start":1,"end":3}],"facts":["whole function: 1 references to `x`"],"fact_family":"param-flow"}]}
EOF
"#,
        );
        let diff_path = dir.path().join("d.diff");
        std::fs::write(&diff_path, "").unwrap();
        let set = external_bundles(script.to_str().unwrap(), Some(dir.path()), &diff_path).unwrap();
        assert_eq!(set.bundles.len(), 1);
        assert_eq!(set.bundles[0].id, "foo@src/a.ts");
    }

    #[cfg(unix)]
    #[test]
    fn invalid_json_errors_loudly() {
        let dir = TempDir::new().unwrap();
        let script = write_stub_script(dir.path(), "bad-bundler.sh", "echo 'not json at all'\n");
        let diff_path = dir.path().join("d.diff");
        std::fs::write(&diff_path, "").unwrap();
        let err = external_bundles(script.to_str().unwrap(), Some(dir.path()), &diff_path).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("not valid JSON"), "unexpected error message: {msg}");
    }

    #[cfg(unix)]
    #[test]
    fn empty_bundle_array_errors_loudly() {
        let dir = TempDir::new().unwrap();
        let script = write_stub_script(dir.path(), "empty-bundler.sh", r#"echo '{"bundles":[]}'"#);
        let diff_path = dir.path().join("d.diff");
        std::fs::write(&diff_path, "").unwrap();
        let err = external_bundles(script.to_str().unwrap(), Some(dir.path()), &diff_path).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("empty bundle set"), "unexpected error message: {msg}");
    }

    #[cfg(unix)]
    #[test]
    fn missing_required_field_errors_loudly() {
        let dir = TempDir::new().unwrap();
        let script = write_stub_script(
            dir.path(),
            "missing-field-bundler.sh",
            r#"echo '{"bundles":[{"id":"foo@a.ts","facts":[],"fact_family":"param-flow"}]}'"#,
        );
        let diff_path = dir.path().join("d.diff");
        std::fs::write(&diff_path, "").unwrap();
        let err = external_bundles(script.to_str().unwrap(), Some(dir.path()), &diff_path).unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("missing a required field") || msg.contains("wrong shape"),
            "unexpected error message: {msg}"
        );
    }

    // (#2119) `is_plugin_decline` is the classification `ReviewBundleStepKind::
    // run_streaming` uses to tell "not my language" from a real plugin
    // failure — proven at both ends: the exact reference-plugin phrasing
    // reads as a decline, and an unrelated exit-1 message does not.
    #[cfg(unix)]
    #[test]
    fn a_decline_phrased_like_the_reference_rust_plugin_is_recognized() {
        let dir = TempDir::new().unwrap();
        let script = write_stub_script(
            dir.path(),
            "declining-bundler.sh",
            "echo 'darkmux-bundler-rust: no .rs files with reviewable hunks in this diff — \
             nothing to bundle.' >&2\nexit 1\n",
        );
        let diff_path = dir.path().join("d.diff");
        std::fs::write(&diff_path, "").unwrap();
        let err = external_bundles(script.to_str().unwrap(), Some(dir.path()), &diff_path).unwrap_err();
        assert!(is_plugin_decline(&err), "expected a decline, got: {err:#}");
    }

    #[cfg(unix)]
    #[test]
    fn a_genuine_plugin_failure_is_not_read_as_a_decline() {
        let dir = TempDir::new().unwrap();
        let script = write_stub_script(
            dir.path(),
            "crashing-bundler.sh",
            "echo 'darkmux-bundler-rust: panicked reading diff file' >&2\nexit 1\n",
        );
        let diff_path = dir.path().join("d.diff");
        std::fs::write(&diff_path, "").unwrap();
        let err = external_bundles(script.to_str().unwrap(), Some(dir.path()), &diff_path).unwrap_err();
        assert!(!is_plugin_decline(&err), "expected NOT a decline, got: {err:#}");
    }
}
