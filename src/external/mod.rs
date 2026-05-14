//! External-source plugins for `darkmux external pull`. Each plugin
//! emits text/markdown to stdout. Procedural-only (no AI); the AI
//! structuring lives in `darkmux mission propose` (#113 Sprint 3).
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

/// Wrap `gh issue view <url> --comments` or `gh pr view <url> --comments`.
/// Heuristic: paths containing `/pull/` route to `pr view`; everything
/// else routes to `issue view` and lets `gh` emit the upstream error
/// when the URL is something else entirely.
fn pull_gh(target: &str) -> Result<()> {
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
/// AI-structuring (Sprint 3 `mission propose`) handles the lowering.
fn pull_url(target: &str) -> Result<()> {
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
}
