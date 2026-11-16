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
        Ok(m) => Ok(entry_of(&m)),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(Entry::Missing),
        Err(e) => Err(e),
    }
}

/// What `m` (read without following) says the entry is.
fn entry_of(m: &fs::Metadata) -> Entry {
    let kind = m.file_type();
    if kind.is_symlink() {
        Entry::Link
    } else if kind.is_dir() {
        Entry::Folder
    } else if kind.is_file() {
        Entry::File(m.permissions())
    } else {
        Entry::Other
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
    let in_sandbox = || format!("`{}` in the sandbox", shown.display());
    match entry_at(&at).with_context(|| format!("reading {}", in_sandbox()))? {
        Entry::Folder => Ok(()),
        Entry::Link => Err(refuse_link(shown)),
        Entry::Missing => fs::create_dir(&at).with_context(|| format!("creating {}", in_sandbox())),
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
        .with_context(|| format!("reading `{}` in the sandbox", shown.display()))?
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

/// How many temp names [`create_temp`] tries before giving up. Each name
/// already taken is an entry the fixture (or a concurrent setup) holds.
const TEMP_ATTEMPTS: u32 = 64;

/// The `n`th temp name a setup write tries in a folder. Its length does not
/// depend on the file's own name, so a name up to the filesystem's limit
/// still has room for its temp file.
fn temp_name(n: u32) -> String {
    format!(".darkmux-setup-{}-{n}", std::process::id())
}

/// A fresh temp file in `parent` and its path. `create_new` refuses any
/// entry already at the name (a link included, never followed), and such a
/// name is skipped for the next one rather than touched.
fn create_temp(parent: &Path) -> io::Result<(fs::File, PathBuf)> {
    for n in 0..TEMP_ATTEMPTS {
        let tmp = parent.join(temp_name(n));
        match fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&tmp)
        {
            Ok(f) => return Ok((f, tmp)),
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e),
        }
    }
    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        format!("{TEMP_ATTEMPTS} temp names beside the file are all taken"),
    ))
}

/// Fill a fresh temp file in `parent`, then rename it over `name`. On a
/// failure after the temp file was made, that file (and only that one) is
/// removed.
fn replace(
    parent: &Path,
    name: &std::ffi::OsStr,
    mode: Option<fs::Permissions>,
    fill: impl FnOnce(&mut fs::File) -> io::Result<()>,
) -> io::Result<()> {
    let (mut f, tmp) = create_temp(parent)?;
    let placed = (|| {
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
        .with_context(|| format!("writing `{}` in the sandbox", rel.display()))
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
            "copying {} to `{}` in the sandbox",
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

    #[test]
    fn write_file_takes_a_name_as_long_as_the_filesystem_allows() {
        // (#3599 review) The temp name must not grow with the key: a 250-byte
        // name is legal (the limit is 255) and plain `fs::write` took it.
        let tmp = TempDir::new().unwrap();
        let name = format!("{}.txt", "a".repeat(246));
        assert_eq!(name.len(), 250);
        write_file(tmp.path(), Path::new(&name), b"long").unwrap();
        assert_eq!(fs::read_to_string(tmp.path().join(&name)).unwrap(), "long");
    }

    #[test]
    fn a_link_planted_at_the_temp_name_is_never_written_through() {
        // (#3599 review) The fixture holds a link at the name the temp file
        // would take: the write must not open it, and the link stays.
        let tmp = TempDir::new().unwrap();
        let outside = tmp.path().join("outside.txt");
        fs::write(&outside, "keep").unwrap();
        let root = tmp.path().join("root");
        fs::create_dir(&root).unwrap();
        symlink(&outside, root.join(temp_name(0))).unwrap();
        write_file(&root, Path::new("notes.txt"), b"new").unwrap();
        assert_eq!(fs::read_to_string(&outside).unwrap(), "keep");
        assert_eq!(fs::read_to_string(root.join("notes.txt")).unwrap(), "new");
        assert!(fs::symlink_metadata(root.join(temp_name(0)))
            .unwrap()
            .file_type()
            .is_symlink());
    }

    #[test]
    fn an_entry_the_fixture_holds_at_the_temp_name_survives() {
        // (#3599 review) A name `create_new` refused belongs to the fixture,
        // not to this write: the cleanup must not remove it.
        let tmp = TempDir::new().unwrap();
        fs::write(tmp.path().join(temp_name(0)), "the fixture's own").unwrap();
        write_file(tmp.path(), Path::new("notes.txt"), b"new").unwrap();
        assert_eq!(
            fs::read_to_string(tmp.path().join(temp_name(0))).unwrap(),
            "the fixture's own"
        );
        let failed = replace(tmp.path(), "x".as_ref(), None, |_| {
            Err(io::Error::other("fill failed"))
        });
        assert!(failed.is_err());
        assert_eq!(
            fs::read_to_string(tmp.path().join(temp_name(0))).unwrap(),
            "the fixture's own"
        );
    }

    #[test]
    fn a_failed_write_leaves_no_temp_file_behind() {
        let tmp = TempDir::new().unwrap();
        let failed = replace(tmp.path(), "x".as_ref(), None, |_| {
            Err(io::Error::other("fill failed"))
        });
        assert!(failed.is_err());
        // A rename that fails after the fill: the target is a non-empty folder.
        fs::create_dir(tmp.path().join("full")).unwrap();
        fs::write(tmp.path().join("full/in"), "").unwrap();
        let failed = replace(tmp.path(), "full".as_ref(), None, |f| f.write_all(b"x"));
        assert!(failed.is_err());
        let names: Vec<_> = fs::read_dir(tmp.path())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(names, vec!["full"], "only the folder is left");
    }

    #[test]
    fn create_temp_gives_up_when_every_name_is_taken() {
        let tmp = TempDir::new().unwrap();
        for n in 0..TEMP_ATTEMPTS {
            fs::write(tmp.path().join(temp_name(n)), "").unwrap();
        }
        let err = create_temp(tmp.path()).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::AlreadyExists);
    }

    #[test]
    fn a_folder_that_cannot_be_made_is_named_by_its_sandbox_path() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().join("root");
        fs::create_dir(&root).unwrap();
        fs::set_permissions(&root, fs::Permissions::from_mode(0o500)).unwrap();
        let err = write_file(&root, Path::new("a/x"), b"x").map_err(|e| format!("{e:#}"));
        fs::set_permissions(&root, fs::Permissions::from_mode(0o700)).unwrap();
        let err = err.unwrap_err();
        assert!(err.starts_with("creating `a` in the sandbox"), "{err}");
        assert!(!err.contains(&*root.to_string_lossy()), "{err}");
    }
}
