//! `darkmux skills install` — copy bundled agent-invokable skills into a
//! Claude Code (or compatible) skills directory.
//!
//! Source priority:
//!   1. On-disk `skills/` dir (if found via the search path) — preserved for
//!      developers iterating on skills without rebuilding.
//!   2. Compiled-in embedded skills (the `EMBEDDED_SKILLS` const) — the
//!      default for users installing via `cargo install --path .` from
//!      anywhere outside the source tree.
//!
//! Destination: `~/.claude/skills/<skill-name>/SKILL.md` by default,
//! or whatever the user passes via `--target`.

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
        "darkmux-status",
        include_str!("../skills/darkmux-status/SKILL.md"),
    ),
    (
        "darkmux-list-stacks",
        include_str!("../skills/darkmux-list-stacks/SKILL.md"),
    ),
    (
        "darkmux-swap-stack",
        include_str!("../skills/darkmux-swap-stack/SKILL.md"),
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
];

#[derive(Debug, Default)]
pub struct InstallOptions {
    pub target: Option<PathBuf>,
    pub force: bool,
    pub dry_run: bool,
}

#[derive(Debug, Default)]
pub struct InstallReport {
    pub source: PathBuf,
    pub target: PathBuf,
    pub installed: Vec<String>,
    pub skipped: Vec<String>,
    pub overwritten: Vec<String>,
}

pub fn install_skills(opts: &InstallOptions) -> Result<InstallReport> {
    let target = match &opts.target {
        Some(p) => p.clone(),
        None => default_skills_target()?,
    };

    if !opts.dry_run && !target.exists() {
        fs::create_dir_all(&target)
            .with_context(|| format!("creating {}", target.display()))?;
    }

    // Prefer on-disk skills if available (lets developers iterate without
    // rebuilding); fall back to embedded skills (the typical end-user path).
    if let Some(source) = locate_on_disk_skills_source() {
        install_from_disk(&source, &target, opts)
    } else {
        install_from_embedded(&target, opts)
    }
}

fn install_from_disk(
    source: &Path,
    target: &Path,
    opts: &InstallOptions,
) -> Result<InstallReport> {
    let mut report = InstallReport {
        source: source.to_path_buf(),
        target: target.to_path_buf(),
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
        if already_exists && !opts.force {
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
        target: target.to_path_buf(),
        ..Default::default()
    };

    for (skill_name, body) in EMBEDDED_SKILLS {
        let dst_skill_dir = target.join(skill_name);
        let dst_skill_md = dst_skill_dir.join("SKILL.md");

        let already_exists = dst_skill_md.exists();
        if already_exists && !opts.force {
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
    if let Ok(p) = env::var("DARKMUX_SKILLS_DIR") {
        candidates.push(PathBuf::from(p));
    }
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

/// Default install target: `~/.claude/skills/`.
///
/// Skills install flat (each as `~/.claude/skills/<skill-name>/SKILL.md`) so
/// they're invokable as `/<skill-name>`. Skill names already carry the
/// `darkmux-` prefix (e.g. `darkmux-swap-stack`) so they're discoverable as
/// a group without nesting under a subdirectory.
///
/// Override with `--target` to install into another agent runtime's skill dir.
fn default_skills_target() -> Result<PathBuf> {
    let home = dirs::home_dir()
        .ok_or_else(|| anyhow::anyhow!("could not resolve home directory"))?;
    Ok(home.join(".claude").join("skills"))
}

pub fn list_installed_skills(target: Option<&Path>) -> Result<Vec<String>> {
    let dir = match target {
        Some(p) => p.to_path_buf(),
        None => default_skills_target()?,
    };
    if !dir.exists() {
        return Ok(Vec::new());
    }
    let mut out: Vec<String> = Vec::new();
    for entry in fs::read_dir(&dir)? {
        let entry = entry?;
        let p = entry.path();
        if p.is_dir() && p.join("SKILL.md").exists() {
            if let Some(name) = p.file_name().and_then(|n| n.to_str()) {
                out.push(name.to_string());
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
        write_skill(&src, "swap-stack", "---\nname: x\n---\nbody");
        write_skill(&src, "status", "---\nname: y\n---\nbody");

        let target = tmp.path().join("dest");
        unsafe { env::set_var("DARKMUX_SKILLS_DIR", src.to_str().unwrap()) };
        let report = install_skills(&InstallOptions {
            target: Some(target.clone()),
            force: false,
            dry_run: false,
        })
        .unwrap();
        unsafe { env::remove_var("DARKMUX_SKILLS_DIR") };

        assert_eq!(report.installed.len(), 2);
        assert!(report.installed.contains(&"swap-stack".to_string()));
        assert!(report.installed.contains(&"status".to_string()));
        assert!(target.join("swap-stack/SKILL.md").exists());
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
        })
        .unwrap();
        unsafe { env::remove_var("DARKMUX_SKILLS_DIR") };

        assert!(report.overwritten.contains(&"alpha".to_string()));
        assert_eq!(fs::read_to_string(target.join("alpha/SKILL.md")).unwrap(), "v2-source");
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
            msg.contains("no on-disk darkmux skills") || msg.contains("missing sentinel"),
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
}
