//! A summary of every flow day file, read once and kept.
//!
//! Three daemon routes answered a question about the whole flow history by
//! walking all of it on every request: `/flow-days` (the day catalog),
//! `/flow-mission/:id` and `/flow-dispatch/:id` (which days hold an id), and
//! the roster's uid backfill (which uid each machine name carried). The
//! viewer polls the replay routes every 5 s, so an open replay tab parsed the
//! whole history every 5 s: about 0.6 s and 740k records a request on a
//! 100-day history.
//!
//! A [`DayIndex`] keeps, per day file, the few facts those routes need (the
//! record count, the mission and session ids it holds, the dispatches that
//! started in it, and the machine uids it names) and how far the file has
//! been read. A later read stats each file and reads only bytes appended
//! since, so after the first read a request costs a stat per day file plus
//! today's new lines. A replay then parses only the day files that hold its
//! id.
//!
//! What a read reports is what a walk of the same bytes would find, the same
//! way `flow_window` keeps lines: blank, torn and schema-header lines are
//! skipped; a non-UTF-8 line ends that file; the bytes after the last
//! newline are read as one more line every time and never kept; a file whose
//! identity changed, that got shorter, or whose last read bytes differ is
//! read again from its start; a file that is gone is forgotten.

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use serde_json::Value;

/// What the daemon's routes need to know about one day file.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct DaySummary {
    /// Flow records (the schema header not counted).
    pub records: u64,
    /// Every `mission_id` a record carries.
    pub missions: BTreeSet<String>,
    /// Every `session_id` a record carries.
    pub sessions: HashSet<String>,
    /// The execution ids of the dispatches that started here.
    pub dispatches: BTreeSet<String>,
    /// Each `machine_id` paired with the first `machine_uid` it carried here.
    pub uids: BTreeMap<String, String>,
}

impl DaySummary {
    fn add(&mut self, v: &Value) {
        self.records += 1;
        let field = |key: &str| v.get(key).and_then(|x| x.as_str());
        if let Some(m) = field("mission_id") {
            if !self.missions.contains(m) {
                self.missions.insert(m.to_string());
            }
        }
        if let Some(s) = field("session_id") {
            if !self.sessions.contains(s) {
                self.sessions.insert(s.to_string());
            }
        }
        if darkmux_flow::reader::action_of(v) == Some(darkmux_flow::FlowAction::DispatchStart) {
            self.dispatches.extend(darkmux_flow::reader::execution_id_of(v).map(|id| id.to_string()));
        }
        if let (Some(id), Some(uid)) = (field("machine_id"), field("machine_uid")) {
            if !self.uids.contains_key(id) {
                self.uids.insert(id.to_string(), uid.to_string());
            }
        }
    }

    /// Whether a record here carries `id` in `field` (`mission_id` or
    /// `session_id`); `None` for a field this summary does not index.
    pub(crate) fn holds(&self, field: &str, id: &str) -> Option<bool> {
        match field {
            "mission_id" => Some(self.missions.contains(id)),
            "session_id" => Some(self.sessions.contains(id)),
            _ => None,
        }
    }
}

/// One day file in the index.
pub(crate) struct IndexedDay {
    pub date: String,
    pub path: PathBuf,
    pub summary: Arc<DaySummary>,
}

/// The kept summaries of one flows directory. One slot, keyed by directory:
/// a read of another directory starts over. The lock is held across a read,
/// so concurrent callers read new bytes once.
pub(crate) struct DayIndex {
    slot: Mutex<Option<DirIndex>>,
}

/// The daemon's index of its flows directory.
pub(crate) static DAY_INDEX: DayIndex = DayIndex::new();

struct DirIndex {
    dir: PathBuf,
    days: BTreeMap<String, DayFile>,
    /// Lines parsed off disk over this index's life: how a test proves a
    /// repeat read read nothing.
    #[cfg(test)]
    lines_parsed: u64,
}

/// What a stat says about a file: when it is unchanged, nothing is opened.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct Stamp {
    id: Option<crate::flow_window::FileId>,
    len: u64,
    modified: Option<std::time::SystemTime>,
}

#[derive(Default)]
struct DayFile {
    stamp: Option<Stamp>,
    /// Bytes read and summarized: always the end of a newline.
    committed: u64,
    last: Vec<u8>,
    /// A non-UTF-8 line ended this file's walk; nothing after it is read.
    stopped: bool,
    /// The whole lines read so far.
    kept: Arc<DaySummary>,
    /// `kept` plus the unterminated tail as of the last read.
    current: Arc<DaySummary>,
}

impl DayIndex {
    pub(crate) const fn new() -> Self {
        Self { slot: Mutex::new(None) }
    }

    /// Every `YYYY-MM-DD.jsonl` day file in `flows_dir`, oldest first, each
    /// with its summary as of now.
    pub(crate) fn days(&self, flows_dir: &Path) -> Vec<IndexedDay> {
        let mut slot = self.slot.lock().unwrap_or_else(|p| p.into_inner());
        if slot.as_ref().map(|s| s.dir.as_path()) != Some(flows_dir) {
            *slot = Some(DirIndex {
                dir: flows_dir.to_path_buf(),
                days: BTreeMap::new(),
                #[cfg(test)]
                lines_parsed: 0,
            });
        }
        slot.as_mut().expect("the slot was just filled").read()
    }

    /// Lines parsed off disk so far for the directory last read.
    #[cfg(test)]
    pub(crate) fn lines_parsed(&self) -> u64 {
        self.slot.lock().unwrap_or_else(|p| p.into_inner()).as_ref().map_or(0, |s| s.lines_parsed)
    }
}

impl DirIndex {
    fn read(&mut self) -> Vec<IndexedDay> {
        let on_disk = crate::flow_day_files(&self.dir);
        let names: HashSet<&str> = on_disk.iter().map(|(d, _)| d.as_str()).collect();
        self.days.retain(|date, _| names.contains(date.as_str()));
        let mut out = Vec::with_capacity(on_disk.len());
        for (date, path) in on_disk {
            let file = self.days.entry(date.clone()).or_default();
            #[cfg(test)]
            let parsed = &mut self.lines_parsed;
            #[cfg(not(test))]
            let parsed = &mut 0u64;
            if file.refresh(&path, parsed) {
                out.push(IndexedDay { date, path, summary: file.current.clone() });
            } else {
                self.days.remove(&date);
            }
        }
        out
    }
}

impl DayFile {
    /// Bring this file's summary up to date with the bytes on disk. `false`
    /// when the file cannot be read (a walk skips it).
    fn refresh(&mut self, path: &Path, parsed: &mut u64) -> bool {
        let Ok(meta) = std::fs::metadata(path) else { return false };
        let stamp = Stamp { id: crate::flow_window::file_id(&meta), len: meta.len(), modified: meta.modified().ok() };
        if self.stamp == Some(stamp) {
            return true;
        }
        let Ok(mut file) = std::fs::File::open(path) else { return false };
        self.restart_unless_appended(&mut file, stamp);
        self.stamp = Some(stamp);
        if self.stopped || stamp.len == self.committed {
            self.current = self.kept.clone();
            return true;
        }
        let start = self.committed;
        let mut buf = Vec::with_capacity((stamp.len - start) as usize);
        if file.seek(SeekFrom::Start(start)).is_err() || (&mut file).take(stamp.len - start).read_to_end(&mut buf).is_err() {
            self.stamp = None;
            return false;
        }
        // Just past the last newline: the bytes before it are whole lines.
        let complete = buf.iter().rposition(|b| *b == b'\n').map_or(0, |p| p + 1);
        self.keep_lines(&buf[..complete], parsed);
        self.committed = start + complete as u64;
        self.last = crate::flow_window::last_bytes(&mut file, self.committed);
        self.current = self.with_tail(&buf[complete..], parsed);
        true
    }

    /// Start this file over unless it is the one read last time, only
    /// longer: another identity (a rotation), shorter than what was read (a
    /// truncation), or different last bytes (a rewrite in place).
    fn restart_unless_appended(&mut self, file: &mut std::fs::File, stamp: Stamp) {
        let same_file = self.stamp.is_some_and(|s| s.id == stamp.id);
        if !same_file || stamp.len < self.committed || crate::flow_window::last_bytes(file, self.committed) != self.last {
            *self = DayFile::default();
        }
    }

    /// Summarize `whole` (newline-terminated lines), stopping the file at a
    /// non-UTF-8 one.
    fn keep_lines(&mut self, whole: &[u8], parsed: &mut u64) {
        let Some((_, lines)) = whole.split_last() else { return };
        let kept = Arc::make_mut(&mut self.kept);
        for raw in lines.split(|b| *b == b'\n') {
            match parse_line(raw, parsed) {
                Line::Record(v) => kept.add(&v),
                Line::Skip => {}
                Line::Stop => {
                    self.stopped = true;
                    return;
                }
            }
        }
    }

    /// The kept summary plus the unterminated `tail` read as one more line,
    /// which is never kept.
    fn with_tail(&self, tail: &[u8], parsed: &mut u64) -> Arc<DaySummary> {
        match (self.stopped, parse_line(tail, parsed)) {
            (false, Line::Record(v)) => {
                let mut with_tail = (*self.kept).clone();
                with_tail.add(&v);
                Arc::new(with_tail)
            }
            _ => self.kept.clone(),
        }
    }
}

enum Line {
    Record(Value),
    Skip,
    Stop,
}

fn parse_line(raw: &[u8], parsed: &mut u64) -> Line {
    if raw.is_empty() {
        return Line::Skip;
    }
    *parsed += 1;
    let Ok(line) = std::str::from_utf8(raw) else {
        return Line::Stop;
    };
    let line = line.trim();
    if line.is_empty() {
        return Line::Skip;
    }
    match darkmux_flow::reader::parse_value(line) {
        Some(v) if v.get("_type").and_then(|t| t.as_str()) != Some("schema") => Line::Record(v),
        _ => Line::Skip,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    /// The summary a from-scratch walk of `path` gives: the oracle every
    /// incremental read must equal.
    fn walked(path: &Path) -> DaySummary {
        let mut s = DaySummary::default();
        crate::for_each_record_in_day_file(path, |v| s.add(v));
        s
    }

    fn append(path: &Path, text: &str) {
        std::fs::OpenOptions::new().create(true).append(true).open(path).unwrap().write_all(text.as_bytes()).unwrap();
    }

    fn summary_of(index: &DayIndex, dir: &Path, date: &str) -> DaySummary {
        index.days(dir).into_iter().find(|d| d.date == date).map(|d| (*d.summary).clone()).unwrap()
    }

    const START: &str = r#"{"action":"dispatch.start","session_id":"s1","mission_id":"m1","execution_id":"e1","machine_id":"laptop","machine_uid":"U1"}"#;
    const STEP: &str = r#"{"action":"tool.call","session_id":"s1","mission_id":"m1","machine_id":"laptop","machine_uid":"U2"}"#;
    const OTHER: &str = r#"{"action":"dispatch.start","session_id":"s2","execution_id":"e2"}"#;

    #[test]
    fn an_appended_file_is_read_only_past_what_was_read_and_equals_a_walk() {
        let dir = tempfile::tempdir().unwrap();
        let day = dir.path().join("2026-11-10.jsonl");
        append(&day, &format!("{{\"_type\":\"schema\"}}\n{START}\n\n{STEP}\n"));
        let index = DayIndex::new();
        assert_eq!(summary_of(&index, dir.path(), "2026-11-10"), walked(&day));
        let first = index.lines_parsed();
        assert_eq!(summary_of(&index, dir.path(), "2026-11-10"), walked(&day));
        assert_eq!(index.lines_parsed(), first, "an unchanged file is not read again");
        append(&day, &format!("{OTHER}\n"));
        let s = summary_of(&index, dir.path(), "2026-11-10");
        assert_eq!(s, walked(&day));
        assert_eq!(index.lines_parsed(), first + 1, "only the appended line is parsed");
        assert_eq!(s.records, 3);
        assert_eq!(s.dispatches.iter().collect::<Vec<_>>(), ["e1", "e2"]);
        assert_eq!(s.uids.get("laptop").map(String::as_str), Some("U1"), "the first uid a name carried wins");
    }

    #[test]
    fn an_unterminated_last_line_counts_but_is_never_kept() {
        let dir = tempfile::tempdir().unwrap();
        let day = dir.path().join("2026-11-10.jsonl");
        append(&day, &format!("{START}\n{OTHER}"));
        let index = DayIndex::new();
        let s = summary_of(&index, dir.path(), "2026-11-10");
        assert_eq!(s, walked(&day));
        assert!(s.sessions.contains("s2"));
        append(&day, &format!("\n{STEP}\n"));
        assert_eq!(summary_of(&index, dir.path(), "2026-11-10"), walked(&day), "the tail is read whole once ended");
    }

    #[test]
    fn a_rewritten_truncated_or_replaced_file_is_read_again_from_its_start() {
        let dir = tempfile::tempdir().unwrap();
        let day = dir.path().join("2026-11-10.jsonl");
        append(&day, &format!("{START}\n{STEP}\n"));
        let index = DayIndex::new();
        index.days(dir.path());
        // Truncated.
        std::fs::write(&day, format!("{OTHER}\n")).unwrap();
        assert_eq!(summary_of(&index, dir.path(), "2026-11-10"), walked(&day));
        // Rewritten in place, same length.
        let same_len = OTHER.replace("s2", "s9");
        std::fs::write(&day, format!("{same_len}\n")).unwrap();
        assert_eq!(summary_of(&index, dir.path(), "2026-11-10"), walked(&day));
        // Replaced by another file.
        let fresh = dir.path().join("fresh");
        std::fs::write(&fresh, format!("{START}\n")).unwrap();
        std::fs::rename(&fresh, &day).unwrap();
        assert_eq!(summary_of(&index, dir.path(), "2026-11-10"), walked(&day));
        // Gone.
        std::fs::remove_file(&day).unwrap();
        assert!(index.days(dir.path()).is_empty());
    }

    #[test]
    fn a_non_utf8_line_ends_the_file_as_a_walk_does() {
        let dir = tempfile::tempdir().unwrap();
        let day = dir.path().join("2026-11-10.jsonl");
        let mut bytes = format!("{START}\n").into_bytes();
        bytes.extend_from_slice(b"\xff\xfe\n");
        bytes.extend_from_slice(format!("{OTHER}\n").as_bytes());
        std::fs::write(&day, &bytes).unwrap();
        let index = DayIndex::new();
        let s = summary_of(&index, dir.path(), "2026-11-10");
        assert_eq!(s, walked(&day));
        assert!(!s.sessions.contains("s2"));
    }

    #[test]
    fn only_dated_day_files_are_indexed_oldest_first() {
        let dir = tempfile::tempdir().unwrap();
        for name in ["2026-11-10.jsonl", "2026-11-09.jsonl", "notes.jsonl", "2026-11.jsonl", "2026-11-08.json"] {
            append(&dir.path().join(name), &format!("{START}\n"));
        }
        let dates: Vec<String> = DayIndex::new().days(dir.path()).into_iter().map(|d| d.date).collect();
        assert_eq!(dates, ["2026-11-09", "2026-11-10"]);
    }
}
