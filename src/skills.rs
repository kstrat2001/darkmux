//! Skill installer — copy bundled agent-invokable skills into a Claude Code
//! (or compatible) skills directory. Driven by `darkmux init` (#1426 retired
//! the standalone `skills` verb; init is the one setup/refresh verb, and
//! `darkmux doctor` flags stale darkmux-* skills and points back at it).
//!
//! Source priority:
//!   1. On-disk `skills/` dir (if found via the search path) — preserved for
//!      developers iterating on skills without rebuilding.
//!   2. Compiled-in embedded skills (the `EMBEDDED_SKILLS` const) — the
//!      default for users installing via `cargo install --path .` from
//!      anywhere outside the source tree.
//!
//! Destination: `~/.claude/skills/<skill-name>/SKILL.md` by default.

use anyhow::{Context, Result};
use std::env;
use std::fs;
use std::path::{Path, PathBuf};

/// Skills compiled into the binary at build time. Each entry is
/// `(skill-name, SKILL.md content)` where the content is read verbatim from
/// `skills/<name>/SKILL.md` at compile time. Used as the fallback when no
/// on-disk skills source is found — makes `cargo install --path .` produce
/// a binary that can `darkmux init` from any directory.
const EMBEDDED_SKILLS: &[(&str, &str)] = &[
    (
        "darkmux-bootstrap",
        include_str!("../skills/darkmux-bootstrap/SKILL.md"),
    ),
    (
        "darkmux-mission-debrief",
        include_str!("../skills/darkmux-mission-debrief/SKILL.md"),
    ),
    (
        "darkmux-enable-audit",
        include_str!("../skills/darkmux-enable-audit/SKILL.md"),
    ),
    (
        "darkmux-add-machine",
        include_str!("../skills/darkmux-add-machine/SKILL.md"),
    ),
    (
        "darkmux-status",
        include_str!("../skills/darkmux-status/SKILL.md"),
    ),
    (
        "darkmux-list-stacks",
        include_str!("../skills/darkmux-list-stacks/SKILL.md"),
    ),
    (
        "darkmux-list-workloads",
        include_str!("../skills/darkmux-list-workloads/SKILL.md"),
    ),
    (
        "darkmux-lab-run",
        include_str!("../skills/darkmux-lab-run/SKILL.md"),
    ),
    (
        "darkmux-list-runs",
        include_str!("../skills/darkmux-list-runs/SKILL.md"),
    ),
    (
        "darkmux-analyze-run",
        include_str!("../skills/darkmux-analyze-run/SKILL.md"),
    ),
    (
        "darkmux-compare-runs",
        include_str!("../skills/darkmux-compare-runs/SKILL.md"),
    ),
    (
        "darkmux-design-profile",
        include_str!("../skills/darkmux-design-profile/SKILL.md"),
    ),
    (
        "darkmux-scan-and-suggest",
        include_str!("../skills/darkmux-scan-and-suggest/SKILL.md"),
    ),
    (
        "darkmux-qa-review",
        include_str!("../skills/darkmux-qa-review/SKILL.md"),
    ),
    (
        "darkmux-escalation-handler",
        include_str!("../skills/darkmux-escalation-handler/SKILL.md"),
    ),
];

/// On-disk `skills/darkmux-*` directories deliberately NOT embedded, and so
/// deliberately NOT shipped to `brew` / `cargo install` users.
///
/// These are MAINTAINER skills — they operate on the darkmux source tree itself
/// (cutting a release, syncing the tap), so a user who never clones the repo has
/// no use for them, and the skill's own description says as much.
///
/// The exclusion is NAMED rather than implicit, for two reasons (#1449's class:
/// we guard the generator, but the user has the artifact):
///   1. `embedded_covers_every_shipped_skill` asserts every `skills/darkmux-*`
///      dir is either embedded or named here — so a new skill cannot go
///      silently unshipped the way `darkmux-qa-review` and
///      `darkmux-escalation-handler` did.
///   2. `init`'s prune pass and the doctor's freshness check consult it, so a
///      maintainer with the repo checked out is neither told their
///      `darkmux-point-release` skill is "no longer bundled" nor has it pruned
///      out from under them.
pub const MAINTAINER_ONLY_SKILLS: &[&str] = &["darkmux-point-release"];

/// The skills compiled into this binary, as `(name, SKILL.md content)`. The
/// doctor's freshness check (#1426) consumes this via `main.rs` so the
/// `darkmux-doctor` crate — which cannot depend on this root binary crate —
/// stays a pure evaluator over caller-supplied input.
pub fn embedded_skills() -> &'static [(&'static str, &'static str)] {
    EMBEDDED_SKILLS
}

/// The directories `darkmux init` installs skills into (`~/.claude/skills`,
/// `~/.gemini/config/skills`). Exposed so the doctor's freshness check (#1426)
/// evaluates the same install locations `init` writes, with no target-
/// resolution logic duplicated into the doctor crate.
pub fn install_target_dirs() -> Result<Vec<PathBuf>> {
    default_skills_targets()
}

#[derive(Debug, Default)]
pub struct InstallOptions {
    pub target: Option<PathBuf>,
    pub force: bool,
    pub dry_run: bool,
    /// (#1426) Overwrite an already-installed `darkmux-*` skill even when
    /// `force` is false. `darkmux init` sets this so a re-run after a binary
    /// upgrade refreshes the bundled skills (the doctor freshness fix_hint
    /// depends on the refresh happening). Only ever affects `darkmux-*` names,
    /// and the installer only ever writes `darkmux-*` skills, so user skills
    /// are never touched regardless.
    pub refresh_darkmux: bool,
}

#[derive(Debug, Default)]
pub struct InstallReport {
    pub source: PathBuf,
    pub targets: Vec<PathBuf>,
    pub installed: Vec<String>,
    pub skipped: Vec<String>,
    pub overwritten: Vec<String>,
    /// (#1449) Installed `darkmux-*` skills this binary no longer bundles,
    /// removed by the prune pass. In `dry_run` these are the prospective
    /// prunes — reported, not removed.
    pub pruned: Vec<String>,
}

pub fn install_skills(opts: &InstallOptions) -> Result<InstallReport> {
    let targets = match &opts.target {
        Some(p) => vec![p.clone()],
        None => default_skills_targets()?,
    };

    let mut report = InstallReport {
        source: PathBuf::new(),
        targets: targets.clone(),
        ..Default::default()
    };

    let source = locate_on_disk_skills_source();
    report.source = source.clone().unwrap_or_else(|| PathBuf::from("<embedded>"));

    for target in &targets {
        if !opts.dry_run && !target.exists() {
            fs::create_dir_all(target)
                .with_context(|| format!("creating {}", target.display()))?;
        }

        let sub_report = if let Some(src) = &source {
            install_from_disk(src, target, opts)?
        } else {
            install_from_embedded(target, opts)?
        };

        for s in sub_report.installed {
            if !report.installed.contains(&s) {
                report.installed.push(s);
            }
        }
        for s in sub_report.overwritten {
            if !report.overwritten.contains(&s) {
                report.overwritten.push(s);
            }
        }
        for s in sub_report.skipped {
            if !report.skipped.contains(&s) {
                report.skipped.push(s);
            }
        }
    }

    // (#1449) Prune pass — remove installed `darkmux-*` skill dirs this binary
    // no longer bundles, so an upgraded machine stops teaching retired verbs.
    // The class this closes: the generator was fixed, but the ARTIFACT already
    // installed on the user's machine (`darkmux-swap-stack` running `darkmux
    // swap` + `darkmux status`) was never touched. `init` refreshes the bundled
    // skills; without this pass it leaves the retired ones behind forever.
    //
    // STRICT SCOPE (the namespace contract): only `darkmux-*`-prefixed dirs are
    // ever inspected or removed — a non-`darkmux-` dir is the operator's own
    // state and is untouchable by construction. Maintainer-only skills (present
    // on a source checkout, never embedded) are protected too, so a maintainer's
    // `darkmux-point-release` is never pruned out from under them.
    let bundled = bundled_skill_names(source.as_deref());
    for target in &targets {
        for name in retired_darkmux_skills(target, &bundled) {
            if opts.dry_run {
                if !report.pruned.contains(&name) {
                    report.pruned.push(name);
                }
                continue;
            }
            let dir = target.join(&name);
            fs::remove_dir_all(&dir)
                .with_context(|| format!("pruning retired skill {}", dir.display()))?;
            if !report.pruned.contains(&name) {
                report.pruned.push(name);
            }
        }
    }
    report.pruned.sort();

    Ok(report)
}

/// The set of `darkmux-*` skill names this binary bundles, given the located
/// on-disk source (or `None` for the embedded fallback). A skill installed on
/// disk whose name is NOT in this set (and not maintainer-only) is a retired
/// skill the prune pass removes. (#1449)
fn bundled_skill_names(source: Option<&Path>) -> std::collections::HashSet<String> {
    let mut names: std::collections::HashSet<String> = std::collections::HashSet::new();
    match source {
        Some(src) => {
            if let Ok(entries) = fs::read_dir(src) {
                for entry in entries.flatten() {
                    let path = entry.path();
                    if !path.is_dir() || !path.join("SKILL.md").exists() {
                        continue;
                    }
                    if let Some(n) = path.file_name().and_then(|n| n.to_str()) {
                        if n.starts_with("darkmux-") {
                            names.insert(n.to_string());
                        }
                    }
                }
            }
        }
        None => {
            for (name, _) in EMBEDDED_SKILLS {
                names.insert((*name).to_string());
            }
        }
    }
    // Maintainer-only skills are legitimately bundled on a source checkout even
    // when not embedded; never prune them.
    for name in MAINTAINER_ONLY_SKILLS {
        names.insert((*name).to_string());
    }
    names
}

/// Installed `darkmux-*` skill dirs in `target` that `bundled` no longer
/// contains — the prune set. Filtered HARD to the `darkmux-*` namespace: a
/// non-darkmux dir is never even read. (#1449)
fn retired_darkmux_skills(
    target: &Path,
    bundled: &std::collections::HashSet<String>,
) -> Vec<String> {
    let mut retired: Vec<String> = Vec::new();
    let Ok(entries) = fs::read_dir(target) else {
        return retired;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        // NEVER inspect non-`darkmux-*` entries — user state, off-limits.
        if !name.starts_with("darkmux-") {
            continue;
        }
        if !path.is_dir() || !path.join("SKILL.md").exists() {
            continue;
        }
        if !bundled.contains(name) {
            retired.push(name.to_string());
        }
    }
    retired.sort();
    retired
}

fn install_from_disk(
    source: &Path,
    target: &Path,
    opts: &InstallOptions,
) -> Result<InstallReport> {
    let mut report = InstallReport {
        source: source.to_path_buf(),
        targets: vec![target.to_path_buf()],
        ..Default::default()
    };

    for entry in fs::read_dir(source)
        .with_context(|| format!("reading {}", source.display()))?
    {
        let entry = entry?;
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let skill_name = path
            .file_name()
            .and_then(|n| n.to_str())
            .map(|s| s.to_string())
            .unwrap_or_default();
        if skill_name.is_empty() {
            continue;
        }
        let src_skill_md = path.join("SKILL.md");
        if !src_skill_md.exists() {
            continue;
        }
        let dst_skill_dir = target.join(&skill_name);
        let dst_skill_md = dst_skill_dir.join("SKILL.md");

        let already_exists = dst_skill_md.exists();
        // (#1426) `refresh_darkmux` refreshes an already-installed darkmux-*
        // skill without requiring --force, so `darkmux init` re-runs pick up
        // an upgraded binary's skills. Never applies to non-darkmux names.
        let refresh = opts.refresh_darkmux && skill_name.starts_with("darkmux-");
        if already_exists && !opts.force && !refresh {
            report.skipped.push(skill_name.clone());
            continue;
        }

        if !opts.dry_run {
            fs::create_dir_all(&dst_skill_dir)
                .with_context(|| format!("creating {}", dst_skill_dir.display()))?;
            fs::copy(&src_skill_md, &dst_skill_md)
                .with_context(|| {
                    format!("copying {} → {}", src_skill_md.display(), dst_skill_md.display())
                })?;
        }

        if already_exists {
            report.overwritten.push(skill_name);
        } else {
            report.installed.push(skill_name);
        }
    }

    Ok(report)
}

fn install_from_embedded(target: &Path, opts: &InstallOptions) -> Result<InstallReport> {
    let mut report = InstallReport {
        source: PathBuf::from("<embedded>"),
        targets: vec![target.to_path_buf()],
        ..Default::default()
    };

    for (skill_name, body) in EMBEDDED_SKILLS {
        let dst_skill_dir = target.join(skill_name);
        let dst_skill_md = dst_skill_dir.join("SKILL.md");

        let already_exists = dst_skill_md.exists();
        // (#1426) See install_from_disk: refresh darkmux-* skills on init
        // re-run without requiring --force.
        let refresh = opts.refresh_darkmux && skill_name.starts_with("darkmux-");
        if already_exists && !opts.force && !refresh {
            report.skipped.push((*skill_name).to_string());
            continue;
        }

        if !opts.dry_run {
            fs::create_dir_all(&dst_skill_dir)
                .with_context(|| format!("creating {}", dst_skill_dir.display()))?;
            fs::write(&dst_skill_md, body)
                .with_context(|| format!("writing {}", dst_skill_md.display()))?;
        }

        if already_exists {
            report.overwritten.push((*skill_name).to_string());
        } else {
            report.installed.push((*skill_name).to_string());
        }
    }

    Ok(report)
}

/// Sentinel file inside a directory that confirms it's a darkmux skills bundle.
const SENTINEL: &str = ".darkmux-skills";

/// Find an on-disk darkmux skills source directory, in priority order:
///   1. `$DARKMUX_SKILLS_DIR`
///   2. `<cwd>/skills/`
///   3. `~/.darkmux/skills/`
///   4. `/usr/local/share/darkmux/skills/`
///
/// Each candidate must contain the `.darkmux-skills` sentinel file to qualify.
/// This prevents the installer from picking up an unrelated `skills/` directory
/// that happens to be in the user's cwd (e.g., another project's skills).
///
/// Returns `None` if no qualifying on-disk source is found — callers should
/// fall back to the binary-embedded `EMBEDDED_SKILLS` const.
fn locate_on_disk_skills_source() -> Option<PathBuf> {
    let mut candidates: Vec<PathBuf> = Vec::new();
    // Override candidates: env(DARKMUX_SKILLS_DIR) then config.dirs.skills
    // (#661 Slice 3), ahead of cwd/home/system.
    candidates.extend(darkmux_types::config_access::skills_override_dirs());
    if let Ok(cwd) = env::current_dir() {
        candidates.push(cwd.join("skills"));
    }
    if let Some(home) = dirs::home_dir() {
        candidates.push(home.join(".darkmux").join("skills"));
    }
    candidates.push(PathBuf::from("/usr/local/share/darkmux/skills"));

    for p in &candidates {
        if p.exists() && p.is_dir() && p.join(SENTINEL).exists() {
            return Some(p.clone());
        }
    }
    None
}

/// Backwards-compatible accessor that errors when no source is available.
/// Currently unused at runtime (the embedded fallback removes the fail case)
/// but retained for tests that exercise the on-disk-only contract.
#[allow(dead_code)]
fn locate_skills_source() -> Result<PathBuf> {
    locate_on_disk_skills_source().ok_or_else(|| {
        anyhow::anyhow!(
            "no on-disk darkmux skills/ directory found (set DARKMUX_SKILLS_DIR or run from \
             the source tree). The embedded skills are still available via install_skills()."
        )
    })
}

/// Default install targets. Automatically detects `~/.claude/` and `~/.gemini/`
/// and returns both if present, defaulting to Claude as a fallback.
fn default_skills_targets() -> Result<Vec<PathBuf>> {
    let home = dirs::home_dir()
        .ok_or_else(|| anyhow::anyhow!("could not resolve home directory"))?;
    let mut targets = Vec::new();

    let claude_dir = home.join(".claude");
    if claude_dir.exists() {
        targets.push(claude_dir.join("skills"));
    }

    let gemini_dir = home.join(".gemini");
    if gemini_dir.exists() {
        targets.push(gemini_dir.join("config").join("skills"));
    }

    if targets.is_empty() {
        // Fallback to Claude target to preserve legacy default behavior
        targets.push(claude_dir.join("skills"));
    }

    Ok(targets)
}

// (#1426) The `skills list` CLI verb retired along with the whole `skills`
// family (init is now the one setup/refresh verb). This enumerator survives —
// its residual value is folding into the doctor freshness check's verbose
// detail (round-8 spec), and its own tests still exercise it — but it has no
// non-test caller in the binary today, so the bin-target build would flag it.
#[allow(dead_code)]
pub fn list_installed_skills(target: Option<&Path>) -> Result<Vec<String>> {
    let targets = match target {
        Some(p) => vec![p.to_path_buf()],
        None => default_skills_targets()?,
    };

    let mut out: Vec<String> = Vec::new();
    for dir in &targets {
        if !dir.exists() {
            continue;
        }
        for entry in fs::read_dir(dir)? {
            let entry = entry?;
            let p = entry.path();
            if p.is_dir() && p.join("SKILL.md").exists() {
                if let Some(name) = p.file_name().and_then(|n| n.to_str()) {
                    let name_str = name.to_string();
                    if !out.contains(&name_str) {
                        out.push(name_str);
                    }
                }
            }
        }
    }
    out.sort();
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn write_skill(source: &Path, name: &str, body: &str) {
        let dir = source.join(name);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("SKILL.md"), body).unwrap();
        // Ensure the sentinel exists so locate_skills_source() accepts the dir.
        let _ = fs::write(source.join(SENTINEL), "test fixture sentinel");
    }

    #[serial_test::serial]
    #[test]
    fn install_copies_all_skills_to_target() {
        let tmp = TempDir::new().unwrap();
        let src = tmp.path().join("skills");
        write_skill(&src, "list-runs", "---\nname: x\n---\nbody");
        write_skill(&src, "status", "---\nname: y\n---\nbody");

        let target = tmp.path().join("dest");
        unsafe { env::set_var("DARKMUX_SKILLS_DIR", src.to_str().unwrap()) };
        let report = install_skills(&InstallOptions {
            target: Some(target.clone()),
            force: false,
            dry_run: false,
            refresh_darkmux: false,
        })
        .unwrap();
        unsafe { env::remove_var("DARKMUX_SKILLS_DIR") };

        assert_eq!(report.installed.len(), 2);
        assert!(report.installed.contains(&"list-runs".to_string()));
        assert!(report.installed.contains(&"status".to_string()));
        assert!(target.join("list-runs/SKILL.md").exists());
        assert!(target.join("status/SKILL.md").exists());
    }

    #[serial_test::serial]
    #[test]
    fn install_skips_existing_without_force() {
        let tmp = TempDir::new().unwrap();
        let src = tmp.path().join("skills");
        write_skill(&src, "alpha", "v1");
        let target = tmp.path().join("dest");
        fs::create_dir_all(target.join("alpha")).unwrap();
        fs::write(target.join("alpha/SKILL.md"), "existing").unwrap();

        unsafe { env::set_var("DARKMUX_SKILLS_DIR", src.to_str().unwrap()) };
        let report = install_skills(&InstallOptions {
            target: Some(target.clone()),
            force: false,
            dry_run: false,
            refresh_darkmux: false,
        })
        .unwrap();
        unsafe { env::remove_var("DARKMUX_SKILLS_DIR") };

        assert!(report.skipped.contains(&"alpha".to_string()));
        assert!(report.installed.is_empty());
        assert_eq!(fs::read_to_string(target.join("alpha/SKILL.md")).unwrap(), "existing");
    }

    #[serial_test::serial]
    #[test]
    fn install_overwrites_with_force() {
        let tmp = TempDir::new().unwrap();
        let src = tmp.path().join("skills");
        write_skill(&src, "alpha", "v2-source");
        let target = tmp.path().join("dest");
        fs::create_dir_all(target.join("alpha")).unwrap();
        fs::write(target.join("alpha/SKILL.md"), "existing").unwrap();

        unsafe { env::set_var("DARKMUX_SKILLS_DIR", src.to_str().unwrap()) };
        let report = install_skills(&InstallOptions {
            target: Some(target.clone()),
            force: true,
            dry_run: false,
            refresh_darkmux: false,
        })
        .unwrap();
        unsafe { env::remove_var("DARKMUX_SKILLS_DIR") };

        assert!(report.overwritten.contains(&"alpha".to_string()));
        assert_eq!(fs::read_to_string(target.join("alpha/SKILL.md")).unwrap(), "v2-source");
    }

    /// (#1426) `refresh_darkmux: true` (what `darkmux init` passes) overwrites an
    /// already-installed `darkmux-*` skill WITHOUT requiring `--force`, so a
    /// re-run after a binary upgrade refreshes the bundled skills.
    #[serial_test::serial]
    #[test]
    fn refresh_darkmux_overwrites_existing_darkmux_skill_without_force() {
        let tmp = TempDir::new().unwrap();
        let src = tmp.path().join("skills");
        write_skill(&src, "darkmux-alpha", "v2-source");
        let target = tmp.path().join("dest");
        fs::create_dir_all(target.join("darkmux-alpha")).unwrap();
        fs::write(target.join("darkmux-alpha/SKILL.md"), "v1-stale").unwrap();

        unsafe { env::set_var("DARKMUX_SKILLS_DIR", src.to_str().unwrap()) };
        let report = install_skills(&InstallOptions {
            target: Some(target.clone()),
            force: false,
            dry_run: false,
            refresh_darkmux: true,
        })
        .unwrap();
        unsafe { env::remove_var("DARKMUX_SKILLS_DIR") };

        assert!(report.overwritten.contains(&"darkmux-alpha".to_string()));
        assert!(report.skipped.is_empty());
        assert_eq!(
            fs::read_to_string(target.join("darkmux-alpha/SKILL.md")).unwrap(),
            "v2-source"
        );
    }

    /// (#1426) `refresh_darkmux` is scoped to the `darkmux-*` namespace: a
    /// non-darkmux skill that happens to be in the install source is NOT
    /// refreshed without `--force` — user state is off-limits by construction.
    #[serial_test::serial]
    #[test]
    fn refresh_darkmux_does_not_touch_non_darkmux_skill_without_force() {
        let tmp = TempDir::new().unwrap();
        let src = tmp.path().join("skills");
        write_skill(&src, "plain-skill", "v2-source");
        let target = tmp.path().join("dest");
        fs::create_dir_all(target.join("plain-skill")).unwrap();
        fs::write(target.join("plain-skill/SKILL.md"), "user-owned").unwrap();

        unsafe { env::set_var("DARKMUX_SKILLS_DIR", src.to_str().unwrap()) };
        let report = install_skills(&InstallOptions {
            target: Some(target.clone()),
            force: false,
            dry_run: false,
            refresh_darkmux: true,
        })
        .unwrap();
        unsafe { env::remove_var("DARKMUX_SKILLS_DIR") };

        assert!(report.skipped.contains(&"plain-skill".to_string()));
        assert!(report.overwritten.is_empty());
        assert_eq!(
            fs::read_to_string(target.join("plain-skill/SKILL.md")).unwrap(),
            "user-owned"
        );
    }

    #[serial_test::serial]
    #[test]
    fn dry_run_does_not_write() {
        let tmp = TempDir::new().unwrap();
        let src = tmp.path().join("skills");
        write_skill(&src, "alpha", "v1");
        let target = tmp.path().join("dest");

        unsafe { env::set_var("DARKMUX_SKILLS_DIR", src.to_str().unwrap()) };
        let report = install_skills(&InstallOptions {
            target: Some(target.clone()),
            force: false,
            dry_run: true,
            refresh_darkmux: false,
        })
        .unwrap();
        unsafe { env::remove_var("DARKMUX_SKILLS_DIR") };

        assert!(report.installed.contains(&"alpha".to_string()));
        // Despite the report, target dir was never created.
        assert!(!target.exists() || !target.join("alpha/SKILL.md").exists());
    }

    #[serial_test::serial]
    #[test]
    fn install_skips_dirs_without_skill_md() {
        let tmp = TempDir::new().unwrap();
        let src = tmp.path().join("skills");
        // alpha has SKILL.md, beta does not
        write_skill(&src, "alpha", "v1");
        fs::create_dir_all(src.join("beta")).unwrap();
        fs::write(src.join("beta/notes.md"), "no SKILL.md here").unwrap();

        let target = tmp.path().join("dest");
        unsafe { env::set_var("DARKMUX_SKILLS_DIR", src.to_str().unwrap()) };
        let report = install_skills(&InstallOptions {
            target: Some(target.clone()),
            force: false,
            dry_run: false,
            refresh_darkmux: false,
        })
        .unwrap();
        unsafe { env::remove_var("DARKMUX_SKILLS_DIR") };

        assert!(report.installed.contains(&"alpha".to_string()));
        assert!(!report.installed.contains(&"beta".to_string()));
    }

    /// When no on-disk skills source is reachable, install_skills falls back
    /// to the binary-embedded skills rather than erroring. This is the path
    /// a user hits after `cargo install --path .` from outside the source tree.
    #[serial_test::serial]
    #[test]
    fn install_falls_back_to_embedded_when_no_on_disk_source() {
        unsafe { env::set_var("DARKMUX_SKILLS_DIR", "/no/such/path") };
        let prev = env::current_dir().unwrap();
        let tmp = TempDir::new().unwrap();
        env::set_current_dir(tmp.path()).unwrap();
        let target = tmp.path().join("dest");
        let result = install_skills(&InstallOptions {
            target: Some(target.clone()),
            force: false,
            dry_run: false,
            refresh_darkmux: false,
        });
        env::set_current_dir(prev).unwrap();
        unsafe { env::remove_var("DARKMUX_SKILLS_DIR") };

        let report = result.expect("embedded fallback should succeed");
        assert_eq!(report.source, PathBuf::from("<embedded>"));
        assert_eq!(
            report.installed.len(),
            EMBEDDED_SKILLS.len(),
            "all embedded skills should be installed on a clean target"
        );
        // Files actually exist on disk.
        for (name, _) in EMBEDDED_SKILLS {
            assert!(target.join(name).join("SKILL.md").exists(), "{name}");
        }
    }

    #[test]
    fn list_installed_returns_empty_on_missing_target() {
        let tmp = TempDir::new().unwrap();
        let target = tmp.path().join("dest-missing");
        let listed = list_installed_skills(Some(&target)).unwrap();
        assert!(listed.is_empty());
    }

    #[test]
    fn list_installed_returns_skill_names() {
        let tmp = TempDir::new().unwrap();
        let target = tmp.path().join("dest");
        fs::create_dir_all(target.join("alpha")).unwrap();
        fs::write(target.join("alpha/SKILL.md"), "x").unwrap();
        fs::create_dir_all(target.join("beta")).unwrap();
        fs::write(target.join("beta/SKILL.md"), "y").unwrap();
        // gamma has no SKILL.md
        fs::create_dir_all(target.join("gamma")).unwrap();

        let listed = list_installed_skills(Some(&target)).unwrap();
        assert_eq!(listed, vec!["alpha", "beta"]);
    }

    #[serial_test::serial]
    #[test]
    fn locate_rejects_dir_without_sentinel() {
        let tmp = TempDir::new().unwrap();
        let bad = tmp.path().join("not-darkmux-skills");
        fs::create_dir_all(bad.join("some-skill")).unwrap();
        fs::write(bad.join("some-skill/SKILL.md"), "x").unwrap();
        // Intentionally NO .darkmux-skills sentinel.

        unsafe { env::set_var("DARKMUX_SKILLS_DIR", bad.to_str().unwrap()) };
        let prev = env::current_dir().unwrap();
        env::set_current_dir(tmp.path()).unwrap();
        let result = locate_skills_source();
        env::set_current_dir(prev).unwrap();
        unsafe { env::remove_var("DARKMUX_SKILLS_DIR") };

        assert!(result.is_err());
        let msg = result.unwrap_err().to_string();
        assert!(
            msg.contains("no on-disk darkmux skills") || msg.contains("missing sentinel"), // drift-guard:allow darkmux skills — noun (the skills/ source directory), not the retired verb (#1469)
            "unexpected error: {msg}"
        );
    }

    #[serial_test::serial]
    #[test]
    fn locate_accepts_dir_with_sentinel() {
        let tmp = TempDir::new().unwrap();
        let good = tmp.path().join("good-skills");
        fs::create_dir_all(&good).unwrap();
        fs::write(good.join(SENTINEL), "marker").unwrap();

        unsafe { env::set_var("DARKMUX_SKILLS_DIR", good.to_str().unwrap()) };
        let result = locate_skills_source().unwrap();
        unsafe { env::remove_var("DARKMUX_SKILLS_DIR") };

        assert_eq!(result, good);
    }

    // ─── (#1449) prune pass — retired darkmux-* skills removed on install ─────

    #[serial_test::serial]
    #[test]
    fn install_prunes_retired_darkmux_skill_and_spares_user_state() {
        let tmp = TempDir::new().unwrap();
        let src = tmp.path().join("skills");
        // The binary bundles exactly darkmux-alpha this build.
        write_skill(&src, "darkmux-alpha", "v1");

        let target = tmp.path().join("dest");
        // Pre-seed the install target with three dirs:
        //   - darkmux-alpha:   bundled, survives (refreshed).
        //   - darkmux-retired: darkmux-* but NOT bundled → pruned.
        //   - my-skill:        non-darkmux user state → NEVER touched.
        fs::create_dir_all(target.join("darkmux-alpha")).unwrap();
        fs::write(target.join("darkmux-alpha/SKILL.md"), "old").unwrap();
        fs::create_dir_all(target.join("darkmux-retired")).unwrap();
        fs::write(target.join("darkmux-retired/SKILL.md"), "swap + status").unwrap();
        fs::create_dir_all(target.join("my-skill")).unwrap();
        fs::write(target.join("my-skill/SKILL.md"), "user-owned").unwrap();

        unsafe { env::set_var("DARKMUX_SKILLS_DIR", src.to_str().unwrap()) };
        let report = install_skills(&InstallOptions {
            target: Some(target.clone()),
            force: false,
            dry_run: false,
            refresh_darkmux: true,
        })
        .unwrap();
        unsafe { env::remove_var("DARKMUX_SKILLS_DIR") };

        assert_eq!(report.pruned, vec!["darkmux-retired".to_string()]);
        assert!(!target.join("darkmux-retired").exists(), "retired dir removed");
        assert!(target.join("darkmux-alpha/SKILL.md").exists(), "bundled survives");
        // User state is untouched — never inspected, never removed.
        assert_eq!(
            fs::read_to_string(target.join("my-skill/SKILL.md")).unwrap(),
            "user-owned"
        );
    }

    #[serial_test::serial]
    #[test]
    fn install_dry_run_lists_prospective_prunes_without_removing() {
        let tmp = TempDir::new().unwrap();
        let src = tmp.path().join("skills");
        write_skill(&src, "darkmux-alpha", "v1");

        let target = tmp.path().join("dest");
        fs::create_dir_all(target.join("darkmux-retired")).unwrap();
        fs::write(target.join("darkmux-retired/SKILL.md"), "leftover").unwrap();

        unsafe { env::set_var("DARKMUX_SKILLS_DIR", src.to_str().unwrap()) };
        let report = install_skills(&InstallOptions {
            target: Some(target.clone()),
            force: false,
            dry_run: true,
            refresh_darkmux: true,
        })
        .unwrap();
        unsafe { env::remove_var("DARKMUX_SKILLS_DIR") };

        assert_eq!(report.pruned, vec!["darkmux-retired".to_string()]);
        // Dry run touches nothing.
        assert!(target.join("darkmux-retired/SKILL.md").exists());
    }

    #[serial_test::serial]
    #[test]
    fn install_never_prunes_maintainer_only_skill() {
        let tmp = TempDir::new().unwrap();
        let src = tmp.path().join("skills");
        // Source bundles only darkmux-alpha (the maintainer skill is embedded-
        // absent), but the maintainer has darkmux-point-release installed.
        write_skill(&src, "darkmux-alpha", "v1");

        let target = tmp.path().join("dest");
        fs::create_dir_all(target.join("darkmux-point-release")).unwrap();
        fs::write(target.join("darkmux-point-release/SKILL.md"), "maintainer").unwrap();

        unsafe { env::set_var("DARKMUX_SKILLS_DIR", src.to_str().unwrap()) };
        let report = install_skills(&InstallOptions {
            target: Some(target.clone()),
            force: false,
            dry_run: false,
            refresh_darkmux: true,
        })
        .unwrap();
        unsafe { env::remove_var("DARKMUX_SKILLS_DIR") };

        assert!(report.pruned.is_empty(), "maintainer skill never pruned");
        assert!(target.join("darkmux-point-release/SKILL.md").exists());
    }

    /// (#1449) The drift that shipped `darkmux-qa-review` +
    /// `darkmux-escalation-handler` unembedded was silent by construction. This
    /// asserts every on-disk `skills/darkmux-*` dir is either embedded or a named
    /// maintainer-only exclusion — so a new skill can never go unshipped quietly.
    #[test]
    fn embedded_covers_every_shipped_skill() {
        let skills_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("skills");
        let embedded: std::collections::HashSet<&str> =
            EMBEDDED_SKILLS.iter().map(|(n, _)| *n).collect();
        let excluded: std::collections::HashSet<&str> =
            MAINTAINER_ONLY_SKILLS.iter().copied().collect();

        let mut unshipped: Vec<String> = Vec::new();
        for entry in fs::read_dir(&skills_dir).expect("skills/ dir exists") {
            let path = entry.unwrap().path();
            let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
                continue;
            };
            if !name.starts_with("darkmux-") || !path.join("SKILL.md").exists() {
                continue;
            }
            if !embedded.contains(name) && !excluded.contains(name) {
                unshipped.push(name.to_string());
            }
        }
        unshipped.sort();
        assert!(
            unshipped.is_empty(),
            "these skills/darkmux-* dirs are neither embedded nor maintainer-only \
             (add to EMBEDDED_SKILLS or MAINTAINER_ONLY_SKILLS): {unshipped:?}"
        );
    }
}
