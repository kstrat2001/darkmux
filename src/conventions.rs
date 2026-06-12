//! (#816) Repo-level shipping conventions — `<repo>/.darkmux/conventions.json`.
//!
//! `mission run`/`ship` default to darkmux-native output (branch
//! `darkmux/<sprint>`, sprint-description commit subject, generated PR
//! body, no labels). Repos with their own conventions — ticket-prefixed
//! branches and commits, a PR template, required labels — declare them in
//! an operator-owned file at the repo root, and the loop speaks them.
//!
//! Deliberately REPO-level, not engagement-level: per the #49 doctrine,
//! engagement never enters the CLI arg surface — but branch/commit/PR
//! conventions are properties of the repository itself, and a visible
//! file in the repo is the operator-sovereign home for them (no hidden
//! behavior, no `--engagement` flag).
//!
//! Template variables: `{ticket}` (from the mission's `ticket` field, set
//! via `mission propose --ticket`), `{sprint}`, `{mission}`, `{subject}`
//! (the default-computed commit subject). A template that references
//! `{ticket}` on a ticketless mission falls back to the built-in default
//! for that item with a soft warning — loud beats quiet, but conventions
//! never block a ship.
//!
//! Schema is lenient (all-`Option` + unknown keys ignored): a partial or
//! hand-edited file never bricks the loop; a malformed one warns and
//! behaves as absent.

use serde::Deserialize;
use std::path::Path;

#[derive(Debug, Clone, Default, Deserialize)]
pub struct Conventions {
    /// Branch name template, e.g. `"{ticket}/{sprint}"`. Default when
    /// absent: `darkmux/{sprint}`.
    #[serde(default)]
    pub branch_template: Option<String>,
    /// Commit subject template, e.g. `"{ticket}: {subject}"`.
    #[serde(default)]
    pub commit_subject_template: Option<String>,
    /// PR title template. Defaults to the commit subject when absent.
    #[serde(default)]
    pub pr_title_template: Option<String>,
    /// Repo-relative path to a PR body template file. Its content is used
    /// as the PR body with `{summary}` replaced by the generated darkmux
    /// summary; without a `{summary}` placeholder the summary is appended.
    #[serde(default)]
    pub pr_body_template: Option<String>,
    /// Labels passed to `gh pr create --label <l>` (each must already
    /// exist in the repo — gh errors otherwise, surfaced verbatim).
    #[serde(default)]
    pub pr_labels: Vec<String>,
}

/// Load `<repo_root>/.darkmux/conventions.json`. Absent file → None
/// (darkmux defaults). Malformed file → soft warning + None, never an
/// error: conventions polish output, they don't gate the loop.
pub fn load(repo_root: &Path) -> Option<Conventions> {
    let path = repo_root.join(".darkmux").join("conventions.json");
    let raw = std::fs::read_to_string(&path).ok()?;
    match serde_json::from_str::<Conventions>(&raw) {
        Ok(c) => Some(c),
        Err(e) => {
            eprintln!(
                "darkmux: warning — {} is not valid conventions JSON ({e}); \
                 using darkmux defaults",
                path.display()
            );
            None
        }
    }
}

/// The variable set a template can reference. `ticket` is None on
/// missions without one — templates referencing `{ticket}` then signal
/// the caller to fall back (see `expand`).
pub struct Vars<'a> {
    pub ticket: Option<&'a str>,
    pub sprint: &'a str,
    pub mission: &'a str,
    pub subject: &'a str,
}

/// Expand `{var}` placeholders. Returns None when the template references
/// `{ticket}` but no ticket is set — the caller falls back to its default
/// (with a soft warning at the call site). Unknown placeholders pass
/// through verbatim (visible in output = self-diagnosing typo).
pub fn expand(template: &str, vars: &Vars) -> Option<String> {
    if template.contains("{ticket}") && vars.ticket.is_none() {
        return None;
    }
    Some(
        template
            .replace("{ticket}", vars.ticket.unwrap_or(""))
            .replace("{sprint}", vars.sprint)
            .replace("{mission}", vars.mission)
            .replace("{subject}", vars.subject),
    )
}

/// Validate an expanded branch name as a safe git ref component path:
/// non-empty, chars in `[A-Za-z0-9/_.-]`, no leading `-` or `/`, no `..`.
/// Conservative on purpose — a failed validation falls back to the
/// default branch shape rather than handing git a hostile ref.
pub fn valid_branch(name: &str) -> bool {
    !name.is_empty()
        && !name.starts_with('-')
        && !name.starts_with('/')
        && !name.contains("..")
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '/' | '_' | '.' | '-'))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vars<'a>(ticket: Option<&'a str>) -> Vars<'a> {
        Vars { ticket, sprint: "s1-fix", mission: "m1", subject: "fix the page copy" }
    }

    #[test]
    fn expand_fills_all_vars() {
        let out = expand("{ticket}/{sprint}", &vars(Some("SYS-2598"))).unwrap();
        assert_eq!(out, "SYS-2598/s1-fix");
        let out = expand("{ticket}: {subject}", &vars(Some("SYS-2598"))).unwrap();
        assert_eq!(out, "SYS-2598: fix the page copy");
    }

    #[test]
    fn expand_signals_fallback_on_missing_ticket() {
        assert!(expand("{ticket}/{sprint}", &vars(None)).is_none());
        // Templates NOT referencing {ticket} still expand fine ticketless.
        assert_eq!(expand("wip/{sprint}", &vars(None)).unwrap(), "wip/s1-fix");
    }

    #[test]
    fn unknown_placeholders_pass_through_visibly() {
        assert_eq!(expand("{tikcet}/{sprint}", &vars(Some("S-1"))).unwrap(), "{tikcet}/s1-fix");
    }

    #[test]
    fn valid_branch_accepts_and_rejects() {
        assert!(valid_branch("SYS-2598/s1-fix"));
        assert!(valid_branch("darkmux/s1"));
        assert!(!valid_branch(""));
        assert!(!valid_branch("-rev"));
        assert!(!valid_branch("/abs"));
        assert!(!valid_branch("a..b"));
        assert!(!valid_branch("has space"));
    }

    #[test]
    fn load_handles_absent_and_malformed() {
        let tmp = tempfile::TempDir::new().unwrap();
        assert!(load(tmp.path()).is_none(), "absent file → None");
        std::fs::create_dir_all(tmp.path().join(".darkmux")).unwrap();
        std::fs::write(tmp.path().join(".darkmux/conventions.json"), "{not json").unwrap();
        assert!(load(tmp.path()).is_none(), "malformed → None (warn)");
        std::fs::write(
            tmp.path().join(".darkmux/conventions.json"),
            r#"{"branch_template":"{ticket}/{sprint}","pr_labels":["agent-work"],"unknown_key":1}"#,
        )
        .unwrap();
        let c = load(tmp.path()).expect("valid file loads");
        assert_eq!(c.branch_template.as_deref(), Some("{ticket}/{sprint}"));
        assert_eq!(c.pr_labels, vec!["agent-work"]);
    }
}
