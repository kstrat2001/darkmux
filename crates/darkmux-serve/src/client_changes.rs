//! The files a workspace's branch holds that its base lacks, as `/changes`
//! returns them: read through [`crate::client_git`], so every call names the
//! repository darkmux holds and carries host git's hardening.

use crate::client_git::WorkCopy;
use crate::client_wire::BranchFile;

/// Why the branch's files could not be read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ChangesError {
    /// Listing what changed between base and head failed.
    Listing,
    /// Reading this changed file's content failed.
    Unreadable(String),
    /// The changed files' contents add up to more than the cap.
    TooLarge,
}

/// Every file that differs between `base` and `head`, in git's path order:
/// `{path, deleted: true}`, or the file's content (base64) and mode at
/// `head`. [`ChangesError::TooLarge`] once the contents pass `cap` bytes.
///
/// Two git calls whatever the branch holds: `diff --raw` names each file
/// with its mode and blob id at `head`, and one `cat-file --batch` streams
/// every blob (raw bytes, no filter or textconv). A submodule, or a changed
/// name that is not UTF-8 (the reply's paths are strings), is
/// [`ChangesError::Unreadable`].
pub(crate) fn branch_files(wc: &WorkCopy, base: &str, head: &str, cap: usize) -> Result<Vec<BranchFile>, ChangesError> {
    let listing = crate::client_git::run(wc, None, &["diff", "--no-renames", "--raw", "--no-abbrev", "-z", base, head])
        .ok_or(ChangesError::Listing)?;
    let changes = parse_raw(&listing.stdout);
    // Blobs are read up to the first file that can't be, so a cap passed
    // before it still answers TooLarge.
    let wanted: Vec<&Change> = changes.iter().take_while(|c| c.readable()).filter(|c| !c.deleted).collect();
    let mut blobs = read_blobs(wc, &wanted, cap)?.into_iter();
    let mut files = Vec::with_capacity(changes.len());
    for change in &changes {
        let path = String::from_utf8_lossy(&change.path).into_owned();
        if change.deleted {
            files.push(BranchFile { path, deleted: Some(true), mode: None, content: None });
            continue;
        }
        let Some(blob) = blobs.next() else {
            return Err(ChangesError::Unreadable(path));
        };
        files.push(BranchFile {
            path,
            deleted: None,
            mode: Some(change.mode.clone()),
            content: Some(crate::client_workspace::base64_encode(&blob)),
        });
    }
    Ok(files)
}

/// One file `diff --raw` names: its path as git stores it, and at `head`
/// its mode and blob id, unless it is gone there.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Change {
    path: Vec<u8>,
    deleted: bool,
    mode: String,
    oid: String,
}

impl Change {
    /// Whether it can be sent by name: a deletion, or a file whose name is
    /// UTF-8. (A submodule is refused when its "blob" turns out not to be.)
    fn readable(&self) -> bool {
        self.deleted || std::str::from_utf8(&self.path).is_ok()
    }
}

/// `git diff --raw -z --no-abbrev` with no renames: `:<old mode> <new mode>
/// <old id> <new id> <status>` NUL `<path>` NUL, per file.
fn parse_raw(raw: &[u8]) -> Vec<Change> {
    let mut fields = raw.split(|b| *b == 0).filter(|f| !f.is_empty());
    let mut out = Vec::new();
    while let (Some(meta), Some(path)) = (fields.next(), fields.next()) {
        let meta = String::from_utf8_lossy(meta);
        let parts: Vec<&str> = meta.trim_start_matches(':').split(' ').collect();
        let [_, mode, _, oid, status] = parts.as_slice() else { continue };
        out.push(Change {
            path: path.to_vec(),
            deleted: status.starts_with('D'),
            mode: mode.to_string(),
            oid: oid.to_string(),
        });
    }
    out
}

/// Each change's blob, in order, from one `git cat-file --batch`; stops at
/// [`ChangesError::TooLarge`] as soon as the sizes pass `cap`.
fn read_blobs(wc: &WorkCopy, wanted: &[&Change], cap: usize) -> Result<Vec<Vec<u8>>, ChangesError> {
    use std::io::{BufRead, BufReader, Read, Write};
    use std::process::Stdio;
    let Some(first) = wanted.first() else {
        return Ok(Vec::new());
    };
    let unreadable = |c: &Change| ChangesError::Unreadable(String::from_utf8_lossy(&c.path).into_owned());
    let mut child = crate::client_git::command(wc, None, &["cat-file", "--batch"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|_| unreadable(first))?;
    let (Some(mut stdin), Some(stdout)) = (child.stdin.take(), child.stdout.take()) else {
        let _ = child.kill();
        let _ = child.wait();
        return Err(unreadable(first));
    };
    let ids: String = wanted.iter().map(|c| format!("{}\n", c.oid)).collect();
    std::thread::scope(|s| {
        // Written from its own thread: git answers as it reads, and a full
        // pipe each way would otherwise stall both.
        s.spawn(move || {
            let _ = stdin.write_all(ids.as_bytes());
        });
        let mut out = BufReader::new(stdout);
        let mut blobs = Vec::with_capacity(wanted.len());
        let mut total = 0usize;
        let mut read = || -> Result<(), ChangesError> {
            for change in wanted {
                let mut header = String::new();
                out.read_line(&mut header).map_err(|_| unreadable(change))?;
                let size = blob_size(&header).ok_or_else(|| unreadable(change))?;
                total += size;
                if total > cap {
                    return Err(ChangesError::TooLarge);
                }
                let mut blob = vec![0u8; size + 1];
                out.read_exact(&mut blob).map_err(|_| unreadable(change))?;
                blob.pop();
                blobs.push(blob);
            }
            Ok(())
        };
        let result = read();
        // Killed rather than drained: a refused read stops git at once, and
        // the writer then fails on the closed pipe instead of blocking.
        let _ = child.kill();
        let _ = child.wait();
        result.map(|()| blobs)
    })
}

/// The size `cat-file --batch`'s header gives for a blob: `<oid> blob
/// <size>`. `None` for anything else (`<oid> missing`, a submodule's
/// commit).
fn blob_size(header: &str) -> Option<usize> {
    let mut parts = header.trim_end_matches('\n').split(' ').skip(1);
    match (parts.next(), parts.next()) {
        (Some("blob"), Some(size)) => size.parse().ok(),
        _ => None,
    }
}

#[cfg(test)]
#[path = "client_changes_tests.rs"]
mod tests;
