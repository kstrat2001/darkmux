//! (#2924, moved here by #2916 stage 2) What this machine knows about fleet
//! identity: which machine goes by which name, from this machine's own
//! resolution, live presence beats and a bounded window of flow history.
//!
//! This lived in the CLI binary (`fleet_cli`), where only `darkmux doctor`
//! could use it. `profile@machine` addresses resolve through the one
//! canonical machine name, and the daemon and routing will need the same
//! knowledge, so it lives in the fleet library now. Pure: callers gather
//! the live inputs (presence needs Redis) and pass them in.

/// Whether live presence was read for this doctor run. Without it, a peer
/// that is merely off and a name nothing ever used look the same.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum PresenceState {
    /// No Redis is configured, so there is no presence to read.
    #[default]
    NotConfigured,
    /// Redis is configured but the read failed.
    Unreadable,
    /// Presence beats were read (possibly none).
    Read,
}

/// What this machine knows about fleet identity, gathered by the caller from
/// its own resolution, presence beats, and a bounded window of local flow
/// history.
#[derive(Debug, Clone, Default)]
pub struct FleetIdentityKnowledge {
    /// Each known hardware uid -> the machine_id that machine goes by NOW:
    /// this machine's own resolution, a live presence beat's `display_name`,
    /// else the most recent name in flow history.
    pub current_name_by_uid: std::collections::BTreeMap<String, String>,
    /// Every name seen -> EVERY uid seen under it. A set, not a last-writer
    /// map: one machine collects throwaway names (a `DARKMUX_MACHINE_ID` set
    /// for one session), and two machines can once have shared a
    /// hostname-derived id. A name traces to a machine only when it maps to
    /// exactly one uid.
    pub uids_by_name: std::collections::BTreeMap<String, std::collections::BTreeSet<String>>,
    /// This machine's own resolved machine_id, even when its hardware uid
    /// could not be read (non-macOS, `ioreg` failing).
    pub local_name: Option<String>,
    /// Whether live presence was read.
    pub presence: PresenceState,
    /// This machine's hardware uid, when readable. A history-only trace to
    /// it is weak: throwaway session names collect here.
    pub local_uid: Option<String>,
    /// uids whose current name came from a LIVE source (this machine, a
    /// presence beat), not from history.
    pub live_uids: std::collections::BTreeSet<String>,
    /// True when `local_name` came from the `DARKMUX_MACHINE_ID` env tier
    /// (a per-shell override), which is not evidence of the machine's name.
    pub local_name_from_env: bool,
    /// `Some(n)` when older flow files exist beyond the last `n` read.
    pub history_truncated_to: Option<usize>,
}

impl FleetIdentityKnowledge {
    /// True when some machine goes by `name` right now.
    pub fn is_current_name(&self, name: &str) -> bool {
        self.local_name.as_deref() == Some(name) || self.current_name_by_uid.values().any(|n| n == name)
    }
}

/// What this doctor run learned without reading flow history.
pub struct LiveIdentity {
    /// Presence beats as `(uid, display_name)`.
    pub beats: Vec<(String, String)>,
    /// This machine's hardware uid, when readable.
    pub local_uid: Option<String>,
    /// This machine's resolved machine_id.
    pub local_name: Option<String>,
    /// True when `local_name` came from the `DARKMUX_MACHINE_ID` env tier.
    pub local_name_from_env: bool,
    pub presence: PresenceState,
}

/// (#2924 C-5) How many of the most recent flow FILES the roster identity
/// check reads (one file per day in practice, but it counts files: a day
/// with no records has none). Bounds doctor's cost as history is retained without
/// limit (the laptop holds ~130 days, ~300 MB). 120 covers the live rename
/// this check was written for; a rename older than the window is reported as
/// a note ("matches no machine_id this machine can see"), never a warning.
pub const ROSTER_HISTORY_FILES: usize = 120;

/// True when some roster entry cannot be settled from live knowledge alone:
/// it declares no uid and is not a current name, or declares a uid nobody
/// live answers to. History can only change the verdict for those.
pub fn roster_needs_history(roster: &crate::FleetRoster, live: &FleetIdentityKnowledge) -> bool {
    roster.machines.values().any(|m| match &m.machine_uid {
        Some(uid) => !live.current_name_by_uid.contains_key(uid),
        None => !live.is_current_name(&m.id),
    })
}

/// A flow record's top-level `(machine_id, machine_uid)`, or `None` when it
/// carries no machine_id.
///
/// Measured cost is why this is not a plain `serde_json` parse: a full
/// history on the laptop is ~300 MB, and parsing every line added ~4 s to a
/// debug `darkmux doctor`. darkmux writes both fields as flat strings among
/// the record's leading scalar fields, so the fast path reads them by key,
/// accepting a match only when no `{` precedes it (i.e. it is not inside a
/// nested object). Any other shape falls back to a real parse.
pub fn record_identity(line: &str) -> Option<(String, Option<String>)> {
    fn flat_field<'a>(line: &'a str, key: &str) -> Option<Option<&'a str>> {
        let pat = format!("\"{key}\":\"");
        let Some(at) = line.find(&pat) else {
            return if line.contains(&format!("\"{key}\"")) { None } else { Some(None) };
        };
        if line.get(1..at).is_some_and(|pre| pre.contains('{')) {
            return None;
        }
        let rest = &line[at + pat.len()..];
        let end = rest.find('"')?;
        let v = &rest[..end];
        if v.contains('\\') {
            return None;
        }
        Some(Some(v))
    }
    let fast = flat_field(line, "machine_id").zip(flat_field(line, "machine_uid"));
    let (id, uid) = match fast {
        Some((id, uid)) => (id.map(str::to_string), uid.map(str::to_string)),
        None => {
            #[derive(serde::Deserialize)]
            struct Ids {
                machine_id: Option<String>,
                machine_uid: Option<String>,
            }
            let ids: Ids = serde_json::from_str(line).ok()?;
            (ids.machine_id, ids.machine_uid)
        }
    };
    let id = id.filter(|n| !n.is_empty())?;
    Some((id, uid.filter(|u| !u.is_empty())))
}

/// Build [`FleetIdentityKnowledge`] from the last
/// [`ROSTER_HISTORY_FILES`] flow day-files, presence beats, and this
/// machine's own resolution. Later sources override earlier ones for a uid's
/// CURRENT name. Every uid a name was ever seen under is kept (a set, never
/// last-writer-wins), because one machine collects throwaway session names
/// and two machines can once have shared a hostname-derived id.
///
/// Flow files are read in name (date) order so the last name a uid wrote is
/// its current one by history.
pub fn gather_identity_knowledge(
    flows_dir: Option<&std::path::Path>,
    live: &LiveIdentity,
) -> FleetIdentityKnowledge {
    use std::io::BufRead;
    let mut known = FleetIdentityKnowledge::default();
    let mut files: Vec<std::path::PathBuf> = flows_dir
        .and_then(|d| std::fs::read_dir(d).ok())
        .map(|rd| {
            rd.flatten()
                .map(|e| e.path())
                .filter(|p| p.extension().is_some_and(|x| x == "jsonl"))
                .collect()
        })
        .unwrap_or_default();
    files.sort();
    let skip = files.len().saturating_sub(ROSTER_HISTORY_FILES);
    if skip > 0 {
        known.history_truncated_to = Some(ROSTER_HISTORY_FILES);
    }
    for path in files.into_iter().skip(skip) {
        let Ok(file) = std::fs::File::open(&path) else { continue };
        for line in std::io::BufReader::new(file).lines().map_while(Result::ok) {
            let Some((name, uid)) = record_identity(&line) else { continue };
            // A record without a uid names no machine. The field is optional
            // (a machine whose `machine_uid()` fails writes records without
            // it), so such a record is skipped, not matched by name.
            let Some(uid) = uid else { continue };
            known.uids_by_name.entry(name.clone()).or_default().insert(uid.clone());
            known.current_name_by_uid.insert(uid, name);
        }
    }
    // (#2924 C-b) A `DARKMUX_MACHINE_ID` override is a per-shell name, not
    // this machine's name for the fleet, so it is not overlaid onto the local
    // uid; that uid keeps whatever presence or history says.
    let local_overlay = if live.local_name_from_env {
        None
    } else {
        live.local_uid.as_deref().zip(live.local_name.as_deref())
    };
    let overlays = live
        .beats
        .iter()
        .map(|(u, n)| (u.as_str(), n.as_str()))
        .chain(local_overlay);
    for (uid, name) in overlays {
        known.uids_by_name.entry(name.to_string()).or_default().insert(uid.to_string());
        known.current_name_by_uid.insert(uid.to_string(), name.to_string());
        known.live_uids.insert(uid.to_string());
    }
    known.local_name = live.local_name.clone();
    known.local_name_from_env = live.local_name_from_env;
    known.local_uid = live.local_uid.clone();
    known.presence = live.presence;
    known
}

