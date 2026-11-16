//! (#3394) Host-side writes into a run's sandbox that never follow a symlink.
//!
//! A run's sandbox is a clone of a fixture (`cow_clone`), and the clone keeps
//! the fixture's symlinks. A fixture may come from someone else, so a plain
//! `create_dir_all` + `fs::write` of `<sandbox>/<rel>` would follow a link at
//! any component of `rel` and write wherever it points: `scratch -> ~` plus a
//! `scratch/.zshrc` key replaces the operator's real `~/.zshrc`.
//!
//! [`write_file`] and [`copy_file`] walk `rel` from the sandbox root one
//! component at a time with `symlink_metadata` (never following), refuse a
//! symlink at any component (a link to a file, to a folder, a dangling one,
//! the first link of a chain) with an error naming it, make each missing
//! folder with `create_dir` (which refuses an entry already there, a dangling
//! link included), and put the file in place by writing a fresh temp file
//! beside it (`create_new`, which refuses any existing entry) and renaming it
//! over the name. `rename` replaces the directory entry without following
//! it, so the final step cannot write through a link either, nor into a file
//! the fixture hard-linked from elsewhere.
//!
//! Scope: the walk is a check-then-act on paths, so it is sound against what
//! the fixture holds when setup runs, which is when nothing else writes into
//! the sandbox (the agent has not started). The lab's grader walks the same
//! way before planting a bug (`grade::run_planted`).

use anyhow::{bail, Context, Result};
use std::fs;
use std::io::{self, Write};
use std::path::{Component, Path, PathBuf};

/// What sits at one path inside the sandbox, never followed.
enum Entry {
    Missing,
    Link,
    Folder,
    File(fs::Permissions),
    Other,
}

fn entry_at(path: &Path) -> io::Result<Entry> {
    match fs::symlink_metadata(path) {
        Ok(m) if m.file_type().is_symlink() => Ok(Entry::Link),
        Ok(m) if m.is_dir() => Ok(Entry::Folder),
        Ok(m) if m.is_file() => Ok(Entry::File(m.permissions())),
        Ok(_) => Ok(Entry::Other),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(Entry::Missing),
        Err(e) => Err(e),
    }
}

/// `rel`'s components, each a plain name; anything else is refused.
fn plain_names(rel: &Path) -> Result<Vec<&std::ffi::OsStr>> {
    let names: Vec<_> = rel
        .components()
        .filter_map(|c| match c {
            Component::Normal(n) => Some(Ok(n)),
            Component::CurDir => None,
            _ => Some(Err(())),
        })
        .collect::<std::result::Result<_, _>>()
        .map_err(|()| {
            anyhow::anyhow!(
                "`{}` is not a plain relative path under the sandbox",
                rel.display()
            )
        })?;
    if names.is_empty() {
        bail!("`{}` names no file under the sandbox", rel.display());
    }
    Ok(names)
}

fn refuse_link(shown: &Path) -> anyhow::Error {
    anyhow::anyhow!(
        "`{}` in the sandbox is a symlink; darkmux writes no setup file through a link, which could lead out of the sandbox",
        shown.display()
    )
}

/// Make sure `root/shown` is a real folder, creating it when missing.
fn ensure_folder(root: &Path, shown: &Path) -> Result<()> {
    let at = root.join(shown);
    match entry_at(&at).with_context(|| format!("reading {}", at.display()))? {
        Entry::Folder => Ok(()),
        Entry::Link => Err(refuse_link(shown)),
        Entry::Missing => fs::create_dir(&at).with_context(|| format!("creating {}", at.display())),
        Entry::File(_) | Entry::Other => {
            bail!("`{}` in the sandbox is not a folder", shown.display())
        }
    }
}

/// Walk `rel`'s folders under `root` (creating missing ones) and check its
/// last name. Returns the parent folder, the last name, and the mode of the
/// regular file already there, if any.
fn prepare(
    root: &Path,
    rel: &Path,
) -> Result<(PathBuf, std::ffi::OsString, Option<fs::Permissions>)> {
    let names = plain_names(rel)?;
    let (last, folders) = names
        .split_last()
        .expect("plain_names refuses an empty path");
    let mut shown = PathBuf::new();
    for name in folders {
        shown.push(name);
        ensure_folder(root, &shown)?;
    }
    let parent = root.join(&shown);
    shown.push(last);
    let existing = match entry_at(&root.join(&shown))
        .with_context(|| format!("reading {}", shown.display()))?
    {
        Entry::Missing => None,
        Entry::File(mode) => Some(mode),
        Entry::Link => return Err(refuse_link(&shown)),
        Entry::Folder | Entry::Other => {
            bail!("`{}` in the sandbox is not a regular file", shown.display())
        }
    };
    Ok((parent, last.to_os_string(), existing))
}

/// Fill a fresh temp file beside `name` in `parent`, then rename it over
/// `name`. The temp name is created with `create_new`, so nothing already
/// there (a link included) is opened.
fn replace(
    parent: &Path,
    name: &std::ffi::OsStr,
    mode: Option<fs::Permissions>,
    fill: impl FnOnce(&mut fs::File) -> io::Result<()>,
) -> io::Result<()> {
    let mut tmp_name = std::ffi::OsString::from(".");
    tmp_name.push(name);
    tmp_name.push(format!(".darkmux-setup-{}", std::process::id()));
    let tmp = parent.join(tmp_name);
    let placed = (|| {
        let mut f = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&tmp)?;
        fill(&mut f)?;
        f.flush()?;
        if let Some(mode) = mode {
            f.set_permissions(mode)?;
        }
        fs::rename(&tmp, parent.join(name))
    })();
    if placed.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    placed
}

/// Write `content` to `root/rel` without following a symlink anywhere on the
/// way. A file already there keeps its mode. See the module doc.
pub(crate) fn write_file(root: &Path, rel: &Path, content: &[u8]) -> Result<()> {
    let (parent, name, existing) = prepare(root, rel)?;
    replace(&parent, &name, existing, |f| f.write_all(content))
        .with_context(|| format!("writing {} in the sandbox", rel.display()))
}

/// Copy the regular file `src` to `root/rel` without following a symlink
/// anywhere under `root`. The copy takes `src`'s mode, as `fs::copy` does.
pub(crate) fn copy_file(src: &Path, root: &Path, rel: &Path) -> Result<()> {
    let (parent, name, _) = prepare(root, rel)?;
    let mut from = fs::File::open(src).with_context(|| format!("opening {}", src.display()))?;
    let mode = from.metadata()?.permissions();
    replace(&parent, &name, Some(mode), |f| {
        io::copy(&mut from, f).map(|_| ())
    })
    .with_context(|| {
        format!(
            "copying {} to {} in the sandbox",
            src.display(),
            rel.display()
        )
    })
}

/// Make `root/rel` a real folder, creating each missing one, without
/// following a symlink anywhere on the way.
pub(crate) fn make_folders(root: &Path, rel: &Path) -> Result<()> {
    let mut shown = PathBuf::new();
    for name in plain_names(rel)? {
        shown.push(name);
        ensure_folder(root, &shown)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;
    use tempfile::TempDir;

    #[test]
    fn plain_names_refuses_escapes_and_empty_paths() {
        for bad in ["../x", "/etc/passwd", "a/../../b", "", "."] {
            assert!(
                plain_names(Path::new(bad)).is_err(),
                "{bad} must be refused"
            );
        }
        assert_eq!(plain_names(Path::new("./a/b")).unwrap(), vec!["a", "b"]);
    }

    #[test]
    fn write_file_refuses_a_link_where_a_folder_is_needed() {
        let tmp = TempDir::new().unwrap();
        let outside = tmp.path().join("outside");
        let root = tmp.path().join("root");
        fs::create_dir_all(&outside).unwrap();
        fs::create_dir_all(&root).unwrap();
        symlink(&outside, root.join("d")).unwrap();
        let err = write_file(&root, Path::new("d/x"), b"x")
            .unwrap_err()
            .to_string();
        assert!(err.contains("`d`") && err.contains("symlink"), "{err}");
        assert!(!outside.join("x").exists());
    }

    #[test]
    fn write_file_refuses_a_file_where_a_folder_is_needed() {
        let tmp = TempDir::new().unwrap();
        fs::write(tmp.path().join("f"), "a file").unwrap();
        let err = write_file(tmp.path(), Path::new("f/x"), b"x")
            .unwrap_err()
            .to_string();
        assert!(err.contains("not a folder"), "{err}");
    }

    #[test]
    fn write_file_refuses_a_folder_as_the_file() {
        let tmp = TempDir::new().unwrap();
        fs::create_dir(tmp.path().join("d")).unwrap();
        let err = write_file(tmp.path(), Path::new("d"), b"x")
            .unwrap_err()
            .to_string();
        assert!(err.contains("not a regular file"), "{err}");
    }

    #[test]
    fn write_file_leaves_no_temp_file_behind() {
        let tmp = TempDir::new().unwrap();
        write_file(tmp.path(), Path::new("a/b.txt"), b"one").unwrap();
        write_file(tmp.path(), Path::new("a/b.txt"), b"two").unwrap();
        let names: Vec<_> = fs::read_dir(tmp.path().join("a"))
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(names, vec!["b.txt"]);
        assert_eq!(
            fs::read_to_string(tmp.path().join("a/b.txt")).unwrap(),
            "two"
        );
    }

    #[test]
    fn write_file_replaces_a_hard_link_instead_of_writing_into_it() {
        // A fixture file hard-linked from elsewhere: writing through the
        // shared inode would change the other name too.
        let tmp = TempDir::new().unwrap();
        let outside = tmp.path().join("outside.txt");
        fs::write(&outside, "keep").unwrap();
        let root = tmp.path().join("root");
        fs::create_dir(&root).unwrap();
        fs::hard_link(&outside, root.join("f.txt")).unwrap();
        write_file(&root, Path::new("f.txt"), b"new").unwrap();
        assert_eq!(fs::read_to_string(&outside).unwrap(), "keep");
        assert_eq!(fs::read_to_string(root.join("f.txt")).unwrap(), "new");
    }

    #[test]
    fn make_folders_refuses_a_link_and_makes_real_folders() {
        let tmp = TempDir::new().unwrap();
        let outside = tmp.path().join("outside");
        let root = tmp.path().join("root");
        fs::create_dir_all(&outside).unwrap();
        fs::create_dir_all(&root).unwrap();
        make_folders(&root, Path::new("a/b")).unwrap();
        assert!(root.join("a/b").is_dir());
        symlink(&outside, root.join("l")).unwrap();
        let err = make_folders(&root, Path::new("l/c"))
            .unwrap_err()
            .to_string();
        assert!(err.contains("`l`"), "{err}");
        assert!(!outside.join("c").exists());
    }
}
