//! (#816) Repo-level worktree-branch conventions —
//! `<repo>/.darkmux/conventions.json`.
//!
//! A coder-phase mission's worktree branches `darkmux/<phase>` by default.
//! Repos with their own branch shape — ticket-prefixed, say — declare a
//! `branch_template` in an operator-owned file at the repo root, and the
//! worktree-creation step speaks it.
//!
//! (#1463) The PR-authoring conventions (commit-subject/PR-title/PR-body
//! templates, labels, the bot commit identity) retired with the `ship` verb:
//! the frontier orchestrator now does the git/gh work by hand and owns those
//! choices natively. Only the branch template — a property the darkmux-created
//! worktree needs — remains here.
//!
//! Deliberately REPO-level, not engagement-level: per the #49 doctrine,
//! engagement never enters the CLI arg surface — but the branch shape is a
//! property of the repository itself, and a visible file in the repo is the
//! operator-sovereign home for it (no hidden behavior, no `--engagement` flag).
//!
//! Template variables: `{ticket}` (from the mission's `ticket` field, set
//! via `mission propose --ticket`), `{phase}`, `{mission}`. A template that
//! references `{ticket}` on a ticketless mission falls back to the built-in
//! default with a soft warning — loud beats quiet, but conventions never
//! block a launch.
//!
//! Schema is lenient (all-`Option` + unknown keys ignored): a partial or
//! hand-edited file never bricks the loop; a malformed one warns and
//! behaves as absent.

use serde::Deserialize;
use std::path::Path;

#[derive(Debug, Clone, Default, Deserialize)]
pub struct Conventions {
    /// Branch name template, e.g. `"{ticket}/{phase}"`. Default when
    /// absent: `darkmux/{phase}`.
    #[serde(default)]
    pub branch_template: Option<String>,
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
    pub phase: &'a str,
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
            .replace("{phase}", vars.phase)
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
        Vars { ticket, phase: "s1-fix", mission: "m1", subject: "fix the page copy" }
    }

    #[test]
    fn expand_fills_all_vars() {
        let out = expand("{ticket}/{phase}", &vars(Some("SYS-2598"))).unwrap();
        assert_eq!(out, "SYS-2598/s1-fix");
        let out = expand("{ticket}: {subject}", &vars(Some("SYS-2598"))).unwrap();
        assert_eq!(out, "SYS-2598: fix the page copy");
    }

    #[test]
    fn expand_signals_fallback_on_missing_ticket() {
        assert!(expand("{ticket}/{phase}", &vars(None)).is_none());
        // Templates NOT referencing {ticket} still expand fine ticketless.
        assert_eq!(expand("wip/{phase}", &vars(None)).unwrap(), "wip/s1-fix");
    }

    #[test]
    fn unknown_placeholders_pass_through_visibly() {
        assert_eq!(expand("{tikcet}/{phase}", &vars(Some("S-1"))).unwrap(), "{tikcet}/s1-fix");
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
        // Lenient on read: unknown keys (including the retired PR-authoring
        // fields, #1463) are ignored; only `branch_template` is parsed now.
        std::fs::write(
            tmp.path().join(".darkmux/conventions.json"),
            r#"{"branch_template":"{ticket}/{phase}","pr_labels":["agent-work"],"unknown_key":1}"#,
        )
        .unwrap();
        let c = load(tmp.path()).expect("valid file loads");
        assert_eq!(c.branch_template.as_deref(), Some("{ticket}/{phase}"));
    }
}
