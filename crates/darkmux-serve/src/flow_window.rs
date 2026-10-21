//! (#2891) The flow day files the daemon's `/runs` folds, read once and kept.
//!
//! `/runs` folds every record of its window (14 days of day files) on every
//! request, and the viewer polls it every few seconds. Reading that history
//! from disk and parsing it from scratch was most of the request's cost, and
//! it grows by a day file a day. A [`FlowRecordStore`] remembers, per day
//! file, how far it has read and the lines it kept, so a later read touches
//! only the bytes appended since. Nothing is served from a timer: every read
//! stats each file in the window, so a record written a moment ago is in the
//! next build.
//!
//! What it keeps is the line text, not parsed JSON: a parsed
//! `serde_json::Value` costs several times its text in memory, and the store
//! lives as long as the daemon. A kept line is parsed again as it is folded,
//! one record at a time, so a build never holds the window parsed. Only the
//! daemon uses a store; a one-shot build (`darkmux run list`) walks the files
//! once and holds nothing.
//!
//! What a read yields is exactly what a from-scratch walk of the same bytes
//! would visit, in the same order:
//!
//! - Day files are `YYYY-MM-DD.jsonl`, dated at or after the window's cutoff,
//!   in name order.
//! - Each newline-terminated line is trimmed and parsed; blank lines,
//!   unparseable lines and the schema header are skipped.
//! - A line that is not UTF-8 ends that file's walk, as `BufRead::lines`
//!   ending on its error did.
//! - The bytes after the last newline (a record still being written, or a
//!   last line with no newline) are read as one more line on every read and
//!   never kept, so the next read sees them whole.
//! - A record the store's `keep` predicate refuses is not kept. The predicate
//!   must name only records every consumer would ignore.
//!
//! Flow archives are append-only and never rewritten (`docs/agents/CONTRACTS.md`,
//! schema versioning). A file that breaks that anyway is read again from its
//! start when the store can tell: its identity changed (device and inode,
//! a rotation), it is now shorter than what was read (a truncation), or the
//! last bytes read differ (a rewrite in place). A file that is gone, or that
//! left the window, is forgotten.

use std::collections::BTreeMap;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use serde_json::Value;

/// How many bytes before the end of what was read are compared to notice a
/// rewrite that kept the file's identity and did not shorten it.
const FINGERPRINT_BYTES: u64 = 64;

/// One day file's kept lines as of a read, then the unterminated tail's.
struct DayLines {
    committed: Arc<Vec<Box<str>>>,
    tail: Vec<Box<str>>,
}

/// Every kept line of a window's day files, oldest file first.
#[derive(Default)]
pub(crate) struct WindowRecords(Vec<DayLines>);

impl WindowRecords {
    /// Visit each kept record in order, parsed one at a time.
    pub(crate) fn for_each(&self, mut visit: impl FnMut(&Value)) {
        for day in &self.0 {
            for line in day.committed.iter().chain(day.tail.iter()) {
                // Every kept line parsed once already; a line that somehow no
                // longer does is skipped, as the walk skips one.
                if let Some(v) = darkmux_flow::reader::parse_value(line) {
                    visit(&v);
                }
            }
        }
    }
}

/// The kept day files of one flows directory. One slot, keyed by directory:
/// a read of another directory starts over. Single-flight: the lock is held
/// across a read, so concurrent callers read new bytes once.
pub(crate) struct FlowRecordStore {
    keep: fn(&Value) -> bool,
    slot: Mutex<Option<DirState>>,
}

struct DirState {
    dir: PathBuf,
    files: BTreeMap<String, DayFile>,
    /// Lines parsed off disk over this state's life: how a test proves a
    /// repeat read read nothing.
    #[cfg(test)]
    lines_parsed: u64,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct FileId {
    dev: u64,
    ino: u64,
}

#[derive(Default)]
struct DayFile {
    id: Option<FileId>,
    /// Bytes read and kept: always the end of a newline.
    committed: u64,
    last: Vec<u8>,
    lines: Arc<Vec<Box<str>>>,
    /// A non-UTF-8 line ended this file's walk; nothing after it is read.
    stopped: bool,
}

enum Line {
    Keep(Box<str>),
    Skip,
    Stop,
}

impl FlowRecordStore {
    pub(crate) const fn new(keep: fn(&Value) -> bool) -> Self {
        Self { keep, slot: Mutex::new(None) }
    }

    /// The window's kept lines: day files in `flows_dir` dated at or after
    /// `cutoff`. Kept files dated before it, or no longer on disk, are
    /// forgotten.
    pub(crate) fn read(&self, flows_dir: &Path, cutoff: &str) -> WindowRecords {
        let mut slot = self.slot.lock().unwrap_or_else(|p| p.into_inner());
        if slot.as_ref().map(|s| s.dir.as_path()) != Some(flows_dir) {
            *slot = Some(DirState {
                dir: flows_dir.to_path_buf(),
                files: BTreeMap::new(),
                #[cfg(test)]
                lines_parsed: 0,
            });
        }
        let state = slot.as_mut().expect("the slot was just filled");
        state.read(cutoff, self.keep)
    }

    /// Lines parsed off disk so far for the directory last read.
    #[cfg(test)]
    pub(crate) fn lines_parsed(&self) -> u64 {
        self.slot.lock().unwrap_or_else(|p| p.into_inner()).as_ref().map_or(0, |s| s.lines_parsed)
    }
}

impl DirState {
    fn read(&mut self, cutoff: &str, keep: fn(&Value) -> bool) -> WindowRecords {
        let Ok(entries) = std::fs::read_dir(&self.dir) else {
            self.files.clear();
            return WindowRecords::default();
        };
        let mut on_disk: BTreeMap<String, PathBuf> = BTreeMap::new();
        for entry in entries.flatten() {
            let file_name = entry.file_name();
            let Some(name) = file_name.to_str() else { continue };
            // A plain length check, not calendar validation: the goal is
            // bounding which files get opened.
            let Some(date) = name.strip_suffix(".jsonl").filter(|d| d.len() == 10) else {
                continue;
            };
            if date >= cutoff {
                on_disk.insert(date.to_string(), entry.path());
            }
        }
        self.files.retain(|date, _| on_disk.contains_key(date));
        let mut out = Vec::new();
        for (date, path) in &on_disk {
            let file = self.files.entry(date.clone()).or_default();
            #[cfg(test)]
            let parsed = &mut self.lines_parsed;
            #[cfg(not(test))]
            let parsed = &mut 0u64;
            match file.refresh(path, keep, parsed) {
                Some(tail) => out.push(DayLines { committed: file.lines.clone(), tail }),
                None => {
                    self.files.remove(date);
                }
            }
        }
        WindowRecords(out)
    }
}

impl DayFile {
    /// Bring this file's kept lines up to date with the bytes on disk.
    /// Returns the unterminated tail's line, or `None` when the file cannot
    /// be read (a walk skips it).
    fn refresh(&mut self, path: &Path, keep: fn(&Value) -> bool, parsed: &mut u64) -> Option<Vec<Box<str>>> {
        let mut file = std::fs::File::open(path).ok()?;
        let len = self.check(&mut file)?;
        if self.stopped || len == self.committed {
            return Some(Vec::new());
        }
        let start = self.committed;
        let mut buf = Vec::with_capacity((len - start) as usize);
        file.seek(SeekFrom::Start(start)).ok()?;
        (&mut file).take(len - start).read_to_end(&mut buf).ok()?;
        // Just past the last newline: the bytes before it are whole lines.
        let complete = buf.iter().rposition(|b| *b == b'\n').map_or(0, |p| p + 1);
        self.keep_lines(&buf[..complete], keep, parsed);
        self.committed = start + complete as u64;
        self.last = last_bytes(&mut file, self.committed);
        Some(self.tail(&buf[complete..], keep, parsed))
    }

    /// The file's length, after starting this file over when it is not the
    /// one read last time: another identity (a rotation), shorter than what
    /// was read (a truncation; checked first so `len - start` cannot
    /// underflow, though a shorter file also fails the fingerprint), or
    /// different last bytes (a rewrite in place).
    fn check(&mut self, file: &mut std::fs::File) -> Option<u64> {
        let meta = file.metadata().ok()?;
        let (len, id) = (meta.len(), file_id(&meta));
        if self.id != id || len < self.committed || last_bytes(file, self.committed) != self.last {
            *self = DayFile { id, ..DayFile::default() };
        }
        Some(len)
    }

    /// Keep the wanted lines of `whole` (newline-terminated lines), stopping
    /// the file at a non-UTF-8 one.
    fn keep_lines(&mut self, whole: &[u8], keep: fn(&Value) -> bool, parsed: &mut u64) {
        let Some((_, lines)) = whole.split_last() else { return };
        let mut fresh = Vec::new();
        for raw in lines.split(|b| *b == b'\n') {
            match parse_line(raw, keep, parsed) {
                Line::Keep(line) => fresh.push(line),
                Line::Skip => {}
                Line::Stop => {
                    self.stopped = true;
                    break;
                }
            }
        }
        if !fresh.is_empty() {
            // Copies the file's lines only while a concurrent build still
            // holds the previous read's snapshot of them.
            Arc::make_mut(&mut self.lines).extend(fresh);
        }
    }

    /// The unterminated tail read as one more line, never kept.
    fn tail(&self, bytes: &[u8], keep: fn(&Value) -> bool, parsed: &mut u64) -> Vec<Box<str>> {
        if self.stopped || bytes.is_empty() {
            return Vec::new();
        }
        match parse_line(bytes, keep, parsed) {
            Line::Keep(line) => vec![line],
            _ => Vec::new(),
        }
    }
}

/// The last [`FINGERPRINT_BYTES`] of a file's first `upto` bytes (shorter
/// when unreadable, which then fails to match a full print).
fn last_bytes(file: &mut std::fs::File, upto: u64) -> Vec<u8> {
    let n = upto.min(FINGERPRINT_BYTES);
    let mut buf = Vec::with_capacity(n as usize);
    if file.seek(SeekFrom::Start(upto - n)).is_ok() {
        let _ = file.take(n).read_to_end(&mut buf);
    }
    buf
}

#[cfg(unix)]
fn file_id(meta: &std::fs::Metadata) -> Option<FileId> {
    use std::os::unix::fs::MetadataExt;
    Some(FileId { dev: meta.dev(), ino: meta.ino() })
}

#[cfg(not(unix))]
fn file_id(_meta: &std::fs::Metadata) -> Option<FileId> {
    None
}

fn parse_line(raw: &[u8], keep: fn(&Value) -> bool, parsed: &mut u64) -> Line {
    *parsed += 1;
    let Ok(line) = std::str::from_utf8(raw) else {
        return Line::Stop;
    };
    let line = line.trim();
    if line.is_empty() {
        return Line::Skip;
    }
    let Some(v) = darkmux_flow::reader::parse_value(line) else {
        return Line::Skip;
    };
    if v.get("_type").and_then(|t| t.as_str()) == Some("schema") || !keep(&v) {
        return Line::Skip;
    }
    Line::Keep(line.into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::TempDir;

    fn keep_all(_: &Value) -> bool {
        true
    }

    fn keep_sessions(v: &Value) -> bool {
        v.get("session_id").is_some()
    }

    /// The walk `/runs` made before #2891 (`for_each_flow_record_from`),
    /// kept verbatim as the oracle every read must equal.
    fn oracle(dir: &Path, cutoff: &str, keep: fn(&Value) -> bool) -> Vec<Value> {
        use std::io::BufRead;
        let mut out = Vec::new();
        let Ok(entries) = std::fs::read_dir(dir) else { return out };
        let mut day_files: Vec<PathBuf> = Vec::new();
        for entry in entries.flatten() {
            let file_name = entry.file_name();
            let Some(name) = file_name.to_str() else { continue };
            let Some(date) = name.strip_suffix(".jsonl") else { continue };
            if date.len() != 10 || date < cutoff {
                continue;
            }
            day_files.push(entry.path());
        }
        day_files.sort();
        for path in day_files {
            let Ok(file) = std::fs::File::open(&path) else { continue };
            for line in std::io::BufReader::new(file).lines() {
                let Ok(line) = line else { break };
                let line = line.trim();
                if line.is_empty() {
                    continue;
                }
                let Some(v) = darkmux_flow::reader::parse_value(line) else { continue };
                if v.get("_type").and_then(|t| t.as_str()) == Some("schema") || !keep(&v) {
                    continue;
                }
                out.push(v);
            }
        }
        out
    }

    fn flat(w: &WindowRecords) -> Vec<Value> {
        let mut out = Vec::new();
        w.for_each(|v| out.push(v.clone()));
        out
    }

    fn rec(n: u32) -> String {
        format!("{{\"ts\":\"2026-01-01T00:00:{:02}Z\",\"action\":\"dispatch.turn\",\"session_id\":\"s{n}\"}}\n", n % 60)
    }

    fn append(dir: &Path, date: &str, bytes: &[u8]) {
        let mut f = std::fs::OpenOptions::new().create(true).append(true).open(dir.join(format!("{date}.jsonl"))).unwrap();
        f.write_all(bytes).unwrap();
    }

    /// Assert the long-lived store, a fresh store and the oracle agree.
    fn agree(store: &FlowRecordStore, dir: &Path, cutoff: &str, step: &str) {
        let kept = flat(&store.read(dir, cutoff));
        let fresh = flat(&FlowRecordStore::new(store.keep).read(dir, cutoff));
        let want = oracle(dir, cutoff, store.keep);
        assert_eq!(fresh, want, "{step}: a from-scratch read differs from the pre-#2891 walk");
        assert_eq!(kept, want, "{step}: the incremental read differs from a from-scratch walk");
    }

    #[test]
    fn an_incremental_read_equals_a_scratch_walk_through_appends_rotation_and_truncation() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path();
        let store = FlowRecordStore::new(keep_sessions);
        append(dir, "2026-01-01", b"{\"_type\":\"schema\",\"session_id\":\"x\"}\n");
        for n in 0..5 {
            append(dir, "2026-01-01", rec(n).as_bytes());
        }
        append(dir, "2026-01-01", b"\n   \nnot json\n{\"ts\":\"t\",\"action\":\"machine.telemetry\"}\n");
        agree(&store, dir, "2026-01-01", "first read");

        append(dir, "2026-01-01", rec(5).as_bytes());
        agree(&store, dir, "2026-01-01", "append");

        // A record still being written: no newline yet, then completed.
        let half = rec(6);
        append(dir, "2026-01-01", &half.as_bytes()[..half.len() - 1]);
        agree(&store, dir, "2026-01-01", "unterminated last line");
        append(dir, "2026-01-01", b"\n");
        agree(&store, dir, "2026-01-01", "last line completed");
        append(dir, "2026-01-01", &half.as_bytes()[..10]);
        agree(&store, dir, "2026-01-01", "torn tail");
        append(dir, "2026-01-01", &half.as_bytes()[10..]);
        agree(&store, dir, "2026-01-01", "torn tail completed");

        append(dir, "2026-01-02", rec(7).as_bytes());
        agree(&store, dir, "2026-01-01", "a new day file");

        // Rotation: the name now holds a different file.
        let day1 = dir.join("2026-01-01.jsonl");
        std::fs::rename(&day1, dir.join("rotated.old")).unwrap();
        append(dir, "2026-01-01", format!("{}{}{}{}", rec(20), rec(21), rec(22), rec(23)).as_bytes());
        agree(&store, dir, "2026-01-01", "rotation");

        // Truncation to fewer bytes than were parsed.
        std::fs::write(&day1, rec(30)).unwrap();
        agree(&store, dir, "2026-01-01", "truncation");

        // Rewritten in place, same inode, longer, same first bytes.
        std::fs::write(&day1, format!("{}{}{}", rec(30), rec(31), rec(32))).unwrap();
        agree(&store, dir, "2026-01-01", "rewrite that kept the head");

        // Rewritten in place, same inode, longer, its last parsed line changed.
        std::fs::write(&day1, format!("{}{}{}{}", rec(30), rec(31), rec(33), rec(34))).unwrap();
        agree(&store, dir, "2026-01-01", "rewrite that changed the last parsed line");

        // A non-UTF-8 line ends the file's walk, appends after it included.
        append(dir, "2026-01-02", b"\xff\xfe\n");
        append(dir, "2026-01-02", rec(8).as_bytes());
        agree(&store, dir, "2026-01-01", "non-UTF-8 line");
        append(dir, "2026-01-02", rec(9).as_bytes());
        agree(&store, dir, "2026-01-01", "append after a non-UTF-8 line");

        std::fs::remove_file(&day1).unwrap();
        agree(&store, dir, "2026-01-01", "a deleted day file");
        append(dir, "2026-01-01", rec(40).as_bytes());
        agree(&store, dir, "2026-01-01", "a day file recreated");
        agree(&store, dir, "2026-01-02", "a later cutoff");
        agree(&store, dir, "2026-01-01", "the earlier cutoff again");
    }

    /// A replacement file whose first and last parsed bytes match and that
    /// is longer: only its identity tells it from an append.
    #[test]
    fn a_replaced_file_is_read_again_even_when_its_ends_match() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path();
        let store = FlowRecordStore::new(keep_all);
        append(dir, "2026-01-01", (1..=5).map(rec).collect::<String>().as_bytes());
        agree(&store, dir, "2026-01-01", "first read");
        let day = dir.join("2026-01-01.jsonl");
        std::fs::rename(&day, dir.join("rotated.old")).unwrap();
        append(dir, "2026-01-01", [1, 9, 3, 4, 5, 6].map(rec).concat().as_bytes());
        agree(&store, dir, "2026-01-01", "replaced");
    }

    #[test]
    fn a_repeat_read_parses_only_the_appended_lines() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path();
        let store = FlowRecordStore::new(keep_all);
        for n in 0..10 {
            append(dir, "2026-01-01", rec(n).as_bytes());
            append(dir, "2026-01-02", rec(n).as_bytes());
        }
        store.read(dir, "2026-01-01");
        assert_eq!(store.lines_parsed(), 20, "the first read parses every line");
        store.read(dir, "2026-01-01");
        assert_eq!(store.lines_parsed(), 20, "a repeat read with nothing appended parses nothing");
        append(dir, "2026-01-02", rec(10).as_bytes());
        let read = store.read(dir, "2026-01-01");
        assert_eq!(store.lines_parsed(), 21, "a read after one append parses one line");
        assert_eq!(flat(&read).len(), 21);
    }

    #[test]
    fn a_file_that_leaves_the_window_is_forgotten() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path();
        let store = FlowRecordStore::new(keep_all);
        append(dir, "2026-01-01", rec(1).as_bytes());
        append(dir, "2026-01-02", rec(2).as_bytes());
        store.read(dir, "2026-01-01");
        assert_eq!(flat(&store.read(dir, "2026-01-02")).len(), 1);
        store.read(dir, "2026-01-01");
        assert_eq!(store.lines_parsed(), 3, "the file the narrower read left out was read again");
    }

    #[test]
    fn a_read_of_another_directory_starts_over() {
        let (a, b) = (TempDir::new().unwrap(), TempDir::new().unwrap());
        append(a.path(), "2026-01-01", rec(1).as_bytes());
        append(b.path(), "2026-01-01", rec(2).as_bytes());
        let store = FlowRecordStore::new(keep_all);
        store.read(a.path(), "2026-01-01");
        assert_eq!(flat(&store.read(b.path(), "2026-01-01")), oracle(b.path(), "2026-01-01", keep_all));
    }
}
