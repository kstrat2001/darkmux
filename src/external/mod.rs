//! External-source plugins for `darkmux external pull`. Each plugin
//! emits text/markdown to stdout. Procedural-only (no AI); the AI
//! structuring lives in `darkmux mission propose` (#113 Phase 3).
//!
//! Plugin contract: take the source identifier, produce text/markdown
//! on stdout. Errors bubble up via `anyhow::Result<()>`.
//!
//! `--jira` plugin (acli wrapper) is deferred to #119 — keychain
//! integration + ADF lowering + project-context defaults each deserve
//! their own design pass.

use anyhow::{anyhow, Context, Result};
use std::io::{Read, Write};
use std::process::Command;

/// Dispatch to exactly one of the procedural plugins based on which
/// flag was supplied. The CLI layer guarantees mutual exclusion via
/// clap's `ArgGroup`, so the runtime check here is defense-in-depth
/// for direct programmatic callers.
pub fn pull(gh: Option<&str>, url: Option<&str>, stdin: bool) -> Result<()> {
    match (gh, url, stdin) {
        (Some(target), None, false) => pull_gh(target),
        (None, Some(target), false) => pull_url(target),
        (None, None, true) => pull_stdin(),
        _ => Err(anyhow!(
            "external pull: exactly one of --gh / --url / --stdin must be set"
        )),
    }
}

/// Reject a `darkmux external pull` target that a subprocess would parse as a
/// flag: empty, or starting with `-` (e.g. `--config` → curl/gh flag injection).
/// Shared by the gh + url plugins. The url plugin ALSO requires an http(s)
/// scheme (which already excludes a leading dash); this keeps the gh path safe
/// and the intent explicit. gh/curl run with explicit argv (no shell), so the
/// leading dash is the only injection vector. (#1112)
fn valid_external_target(s: &str) -> bool {
    !s.is_empty() && !s.starts_with('-')
}

/// Wrap `gh issue view <url> --comments` or `gh pr view <url> --comments`.
/// Heuristic: paths containing `/pull/` route to `pr view`; everything
/// else routes to `issue view` and lets `gh` emit the upstream error
/// when the URL is something else entirely.
fn pull_gh(target: &str) -> Result<()> {
    // (#1112) Argument-injection guard: a target starting with `-` would be
    // parsed by gh as a flag (e.g. `--gh --version`).
    if !valid_external_target(target) {
        return Err(anyhow!(
            "external pull --gh target is empty or starts with `-` \
             (would parse as a gh flag): `{target}`"
        ));
    }
    let kind = if target.contains("/pull/") { "pr" } else { "issue" };
    let output = Command::new("gh")
        .args([kind, "view", target, "--comments"])
        .output()
        .with_context(|| {
            format!("running `gh {kind} view {target}` — is `gh` on PATH?")
        })?;
    if !output.status.success() {
        return Err(anyhow!(
            "`gh {} view {}` failed (exit {}):\n{}",
            kind,
            target,
            output.status.code().unwrap_or(-1),
            String::from_utf8_lossy(&output.stderr)
        ));
    }
    std::io::stdout()
        .write_all(&output.stdout)
        .context("writing gh output to stdout")?;
    Ok(())
}

/// Wrap `curl -s -L --max-time 30 <url>`. Passes through whatever the
/// URL responds with — HTML, JSON, markdown all valid. Downstream
/// AI-structuring (Phase 3 `mission propose`) handles the lowering.
fn pull_url(target: &str) -> Result<()> {
    // (#1112) Argument-injection guard (shared with the gh path). The http(s)
    // scheme check below already excludes a leading `-`; the explicit guard
    // keeps the two plugins symmetric and the intent obvious.
    if !valid_external_target(target) {
        return Err(anyhow!(
            "external pull --url target is empty or starts with `-`: `{target}`"
        ));
    }
    // (#907) Restrict to http(s) — defense-in-depth. Today the operator
    // types the URL by hand, but allowlisting the scheme blocks `file://`,
    // `gopher://`, etc. (an SSRF/local-read shape) should this path ever be
    // wired to less-trusted input.
    // Case-insensitive scheme compare so a hand-typed `HTTP://` works, but
    // deliberately NOT trimmed — a leading-whitespace/control prefix stays
    // rejected (fail-closed) so nothing can be smuggled past the check.
    let scheme_lower = target.to_ascii_lowercase();
    if !(scheme_lower.starts_with("http://") || scheme_lower.starts_with("https://")) {
        return Err(anyhow!(
            "external pull URL must start with http:// or https:// (got: `{target}`)"
        ));
    }
    // (#1112) SSRF note — DEFERRED by design, not an oversight. `-L` follows
    // redirects with no private-IP / IMDS / loopback allowlist, so a hostile or
    // automation-derived URL could be redirected at an internal endpoint. The
    // threat model today is operator-typed URLs (self-inflicted, LOW). If this
    // path is ever wired to less-trusted input, add a resolve-then-allowlist
    // SSRF guard and drop `-L`.
    let output = Command::new("curl")
        .args(["-s", "-L", "--max-time", "30", target])
        .output()
        .with_context(|| format!("running `curl` on `{target}` — is `curl` on PATH?"))?;
    if !output.status.success() {
        return Err(anyhow!(
            "`curl {}` failed (exit {}):\n{}",
            target,
            output.status.code().unwrap_or(-1),
            String::from_utf8_lossy(&output.stderr)
        ));
    }
    std::io::stdout()
        .write_all(&output.stdout)
        .context("writing curl output to stdout")?;
    Ok(())
}

/// Read stdin to EOF and echo to stdout. Useful for
/// `pbpaste | darkmux external pull --stdin | darkmux mission propose --from-stdin`.
fn pull_stdin() -> Result<()> {
    let mut buf = Vec::new();
    std::io::stdin()
        .read_to_end(&mut buf)
        .context("reading stdin")?;
    std::io::stdout().write_all(&buf).context("writing to stdout")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pull_rejects_no_source() {
        let err = pull(None, None, false).expect_err("no source should error");
        assert!(err.to_string().contains("exactly one of"));
    }

    #[test]
    fn pull_rejects_gh_plus_url() {
        let err = pull(Some("foo"), Some("bar"), false).expect_err("multiple should error");
        assert!(err.to_string().contains("exactly one of"));
    }

    #[test]
    fn pull_rejects_gh_plus_stdin() {
        let err = pull(Some("foo"), None, true).expect_err("gh + stdin should error");
        assert!(err.to_string().contains("exactly one of"));
    }

    #[test]
    fn pull_rejects_url_plus_stdin() {
        let err = pull(None, Some("foo"), true).expect_err("url + stdin should error");
        assert!(err.to_string().contains("exactly one of"));
    }

    // (#1112) argument-injection guard
    #[test]
    fn valid_external_target_blocks_flags_and_empty() {
        assert!(valid_external_target("https://github.com/o/r/issues/1"));
        assert!(valid_external_target("o/r#5"));
        assert!(!valid_external_target("-foo")); // would parse as a gh/curl flag
        assert!(!valid_external_target("--config"));
        assert!(!valid_external_target(""));
    }

    #[test]
    fn pull_gh_rejects_leading_dash_target() {
        let err = pull(Some("--version"), None, false).expect_err("leading-dash gh target should error");
        assert!(err.to_string().contains("starts with `-`"));
    }

    #[test]
    fn pull_url_rejects_leading_dash_target() {
        let err = pull(None, Some("-O/etc/passwd"), false).expect_err("leading-dash url target should error");
        assert!(err.to_string().contains("starts with `-`"));
    }
}
