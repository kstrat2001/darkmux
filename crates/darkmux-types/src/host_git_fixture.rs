//! (#3457) Test support: a linked worktree whose `.git` a "run" replaced.
//!
//! Plays the run's part for every host-git test in the workspace: a main
//! repository with two commits and a linked worktree on branch `phase`, whose
//! `.git` pointer file [`PlantedWorktree::plant`] swaps for a git directory
//! of the run's own. That directory's config names three commands, each of
//! which touches a marker file when it runs: `core.fsmonitor`, a clean filter
//! and a `textconv`, which the planted `.gitattributes` gives every path.
//! A test asserts the markers stay absent after host git ran; plain git in
//! the tree touches them, so the fixture can fail.

use std::path::{Path, PathBuf};
use std::process::Command;

/// Fixture git, NOT [`super::HostGit`]: this is the run's side. Identity
/// rides on the command line (a local shim refuses fixture commits that
/// carry it in the environment); hooks and signing are off so the
/// operator's own config cannot interfere.
pub fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .current_dir(dir)
        .args(["-c", "user.name=t", "-c", "user.email=t@example.com", "-c", "protocol.file.allow=always"])
        .args(["-c", "init.defaultBranch=main", "-c", "core.hooksPath=/dev/null", "-c", "commit.gpgsign=false"])
        .args(args)
        .env_remove("GIT_DIR")
        .env_remove("GIT_INDEX_FILE")
        .env_remove("GIT_WORK_TREE")
        .output()
        .expect("git runs");
    assert!(out.status.success(), "git {args:?} in {}: {}", dir.display(), String::from_utf8_lossy(&out.stderr));
    String::from_utf8_lossy(&out.stdout).into_owned()
}

/// How the run leaves the tracked file `a.txt`.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum TreeChange {
    /// Same size, different bytes: a real change git must re-hash to see.
    Content,
    /// Rewritten with the same bytes: no change, but its stat changed, so
    /// git still re-hashes it (and runs a clean filter while doing so).
    TouchOnly,
}

/// Where the run puts the attributes that give every path its filter and
/// diff driver.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum AttrPlace {
    /// `.gitattributes` in the work tree.
    WorkTree,
    /// `info/attributes` inside the planted git dir.
    InfoAttributes,
    /// A file the planted config names as `core.attributesFile`.
    AttributesFile,
}

/// See the module doc.
pub struct PlantedWorktree {
    _tmp: tempfile::TempDir,
    pub root: PathBuf,
    /// The main repository (branch `main`, two commits, `keep-me` at HEAD).
    pub repo: PathBuf,
    /// The linked worktree, on branch `phase`.
    pub wt: PathBuf,
    /// `<repo>/.git/worktrees/wt`: the worktree's real git dir.
    pub admin: PathBuf,
    fsmonitor_marker: PathBuf,
    clean_marker: PathBuf,
    textconv_marker: PathBuf,
}

impl PlantedWorktree {
    /// The repository and its worktree, not yet planted.
    pub fn new() -> Self {
        let tmp = tempfile::TempDir::new().unwrap();
        // Canonical, so paths compare equal to what git prints (macOS
        // `/var` is `/private/var`).
        let root = tmp.path().canonicalize().unwrap();
        let repo = root.join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        git(&repo, &["init", "-q"]);
        std::fs::write(repo.join("a.txt"), "a\n").unwrap();
        git(&repo, &["add", "a.txt"]);
        git(&repo, &["commit", "-qm", "one"]);
        std::fs::write(repo.join("b.txt"), "b\n").unwrap();
        git(&repo, &["add", "b.txt"]);
        git(&repo, &["commit", "-qm", "two"]);
        git(&repo, &["branch", "keep-me"]);
        let wt = root.join("wt");
        git(&repo, &["worktree", "add", "-q", wt.to_str().unwrap(), "-b", "phase"]);
        let admin = repo.join(".git").join("worktrees").join("wt");
        assert!(admin.is_dir(), "admin dir at {}", admin.display());
        Self {
            fsmonitor_marker: root.join("fsmonitor-ran"),
            clean_marker: root.join("clean-ran"),
            textconv_marker: root.join("textconv-ran"),
            _tmp: tmp,
            root,
            repo,
            wt,
            admin,
        }
    }

    /// What the run does: replace `wt/.git` (a pointer file) with a clone's
    /// git directory whose HEAD names `keep-me` and whose config runs the
    /// three marker commands, give every path the filter and diff driver,
    /// then leave `a.txt` as `change` says.
    pub fn plant(&self, change: TreeChange) {
        self.plant_with(change, AttrPlace::WorkTree);
    }

    /// [`Self::plant`], with the attributes put where `place` says.
    pub fn plant_with(&self, change: TreeChange, place: AttrPlace) {
        let staging = self.root.join("planted");
        git(&self.root, &["clone", "-q", "--no-checkout", self.repo.to_str().unwrap(), staging.to_str().unwrap()]);
        git(&staging, &["branch", "-q", "keep-me", "origin/keep-me"]);
        std::fs::write(staging.join(".git").join("HEAD"), "ref: refs/heads/keep-me\n").unwrap();
        std::fs::remove_file(self.wt.join(".git")).expect("the pointer file is there to replace");
        std::fs::rename(staging.join(".git"), self.wt.join(".git")).unwrap();
        std::fs::copy(self.admin.join("index"), self.wt.join(".git").join("index")).unwrap();
        let cfg = self.wt.join(".git").join("config");
        let mut config = std::fs::read_to_string(&cfg).unwrap();
        config.push_str(&format!(
            "[core]\n\tfsmonitor = \"touch '{}'; false\"\n\
             [filter \"x\"]\n\tclean = \"touch '{}'; cat\"\n\
             [diff \"x\"]\n\ttextconv = \"touch '{}'; cat\"\n",
            self.fsmonitor_marker.display(),
            self.clean_marker.display(),
            self.textconv_marker.display(),
        ));
        let attrs = "* filter=x diff=x\n";
        match place {
            AttrPlace::WorkTree => std::fs::write(self.wt.join(".gitattributes"), attrs).unwrap(),
            AttrPlace::InfoAttributes => {
                let info = self.wt.join(".git").join("info");
                std::fs::create_dir_all(&info).unwrap();
                std::fs::write(info.join("attributes"), attrs).unwrap();
            }
            AttrPlace::AttributesFile => {
                let file = self.root.join("planted-attributes");
                std::fs::write(&file, attrs).unwrap();
                config.push_str(&format!("[core]\n\tattributesFile = {}\n", file.display()));
            }
        }
        std::fs::write(&cfg, config).unwrap();
        // Past the index's own mtime second, so the rewrite is a stat change.
        std::thread::sleep(std::time::Duration::from_millis(1100));
        let body = match change {
            TreeChange::Content => "z\n",
            TreeChange::TouchOnly => "a\n",
        };
        std::fs::write(self.wt.join("a.txt"), body).unwrap();
    }

    /// Whether the planted fsmonitor ran since the last check.
    pub fn fsmonitor_ran(&self) -> bool {
        take(&self.fsmonitor_marker)
    }

    /// Whether the planted clean filter ran since the last check.
    pub fn clean_ran(&self) -> bool {
        take(&self.clean_marker)
    }

    /// Whether the planted textconv ran since the last check.
    pub fn textconv_ran(&self) -> bool {
        take(&self.textconv_marker)
    }

    /// Whether any planted command ran since the last check.
    pub fn anything_ran(&self) -> bool {
        let (f, c, t) = (self.fsmonitor_ran(), self.clean_ran(), self.textconv_ran());
        f || c || t
    }

    /// Whether `branch` still exists in the main repository.
    pub fn branch_exists(&self, branch: &str) -> bool {
        !git(&self.repo, &["branch", "--list", branch]).trim().is_empty()
    }
}

impl Default for PlantedWorktree {
    fn default() -> Self {
        Self::new()
    }
}

fn take(marker: &Path) -> bool {
    let was = marker.exists();
    let _ = std::fs::remove_file(marker);
    was
}
