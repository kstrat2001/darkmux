//! (#2902 step 2b) The Rust twin of the viewer's ONE token sum.
//!
//! Every model call darkmux makes emits exactly one `telemetry.tokens`
//! usage record (writer: `darkmux_crew::usage`), and a token total anywhere
//! is a PLAIN SUM of those records: no execution keying, no complete-vs-telemetry
//! precedence, no dedup, no local/cloud classification, no estimates, and no
//! fallback to a `dispatch complete` (a record written before 5.0 without a
//! usage record carries no tokens here). This module is that sum on the server
//! side, shared by `darkmux run list` (`src/run_list.rs`, the TOKENS column and
//! `--usage`) and the daemon's `GET /runs` (each row's `tokens`), the same way
//! `runs::build_runs` is shared by both. `ui/src/lib/usageRecords.ts::sumUsage`
//! is the viewer's half; the shared golden fixture `tests/usage-golden/` pins
//! both to one answer (`shared_golden_matches_the_viewer` below, and the TS
//! side's own test over the same files).
//!
//! A run's tokens are the records that name its session or mission, each
//! record counted under ITS OWN session and mission (#3067: a resumed
//! dispatch reuses its execution id under a new session, so the execution is
//! not the unit of attribution; the record is). A record naming neither
//! (radio routing, a `doctor --probe`) belongs to no run: it is in the
//! overall and in [`UsageBreakdown::no_run`], so the run rows plus `no_run`
//! equal the overall.
//!
//! What this module reports is what darkmux INVOKED, by the machine that
//! executed it (the record's own `machine_uid`/`machine_id`; a relayed run's
//! asker writes no usage, so its tokens count once, on the executor): the
//! endpoint (its registry id when the record carries one, else the string it
//! carries), the model darkmux requested, the model the reply named when it
//! differs, and the provider's own counts with their source. Nothing here
//! labels an endpoint local, cloud or metered, and nothing here costs anything.

use std::collections::{HashMap, HashSet};

use darkmux_crew::usage::UsagePurpose;

/// The sum of a set of records: the Rust twin of `usageRecords.ts`'s
/// `UsageSum`, field for field.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, schemars::JsonSchema)]
pub struct UsageSum {
    /// Sum of each record's total (the provider's own, else the halves it
    /// reported).
    pub total: u64,
    /// Sum of `prompt_tokens`.
    pub prompt: u64,
    /// Sum of `completion_tokens`.
    pub completion: u64,
    /// Sum of `cached_tokens` over the records that report it; `None` when
    /// none does (never assumed zero).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cached: Option<u64>,
    /// Sum of `total` over `purpose: utility` records.
    pub utility: u64,
    /// Usage records counted, `absent` ones included.
    pub usage_records: u64,
    /// Usage records that reported a count: `0` means nothing measured, which
    /// a cell shows as `-`, never 0.
    pub reported: u64,
}

impl UsageSum {
    fn add(&mut self, a: &UsageAmount) {
        self.total = self.total.saturating_add(a.total);
        self.prompt = self.prompt.saturating_add(a.prompt);
        self.completion = self.completion.saturating_add(a.completion);
        if let Some(c) = a.cached {
            self.cached = Some(self.cached.unwrap_or(0).saturating_add(c));
        }
        if a.purpose == UsagePurpose::Utility {
            self.utility = self.utility.saturating_add(a.total);
        }
        if a.reported {
            self.reported += 1;
        }
        self.usage_records += 1;
    }

    /// Fold another sum into this one (the overall over every run).
    fn merge(&mut self, o: &UsageSum) {
        self.total = self.total.saturating_add(o.total);
        self.prompt = self.prompt.saturating_add(o.prompt);
        self.completion = self.completion.saturating_add(o.completion);
        if let Some(c) = o.cached {
            self.cached = Some(self.cached.unwrap_or(0).saturating_add(c));
        }
        self.utility = self.utility.saturating_add(o.utility);
        self.usage_records += o.usage_records;
        self.reported += o.reported;
    }
}

// (#2902 step 5) The per-record half of the sum (what ONE usage record
// contributes, and the value domain it reads counts in) lives in
// `darkmux_crew::usage`, so the endpoint budget's rolling-window sum (in
// `darkmux_crew::budget`, below this crate in the dependency graph) and this
// fold read a record the same way. Re-exported here unchanged.
pub use darkmux_crew::usage::{is_usage_record, usage_contribution, usage_purpose, UsageAmount, MAX_COUNT};
use darkmux_crew::usage::{amount_of, payload_of};

/// THE sum over a slice of records: [`UsageFold`] driven to completion.
pub fn sum_usage<'a>(records: impl IntoIterator<Item = &'a serde_json::Value>) -> UsageSum {
    let mut fold = UsageFold::new(None);
    for r in records {
        fold.add(r);
    }
    fold.finish().breakdown.overall
}

/// One line of the breakdown: everything darkmux invoked one way. The key
/// is the MACHINE that executed the call (#3067: `localhost` is a different
/// machine to whoever made the call, so the same URL on two machines is two
/// lines), the endpoint it called (its registry id when the record carries
/// one, else the endpoint string), the model it requested, and the model the
/// reply named when it carried one; each is a fact off the record, absent
/// when the record did not carry it (a pre-5.0 record).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, schemars::JsonSchema)]
pub struct UsageGroup {
    /// The executing machine's `machine_id` (its newest name in the window,
    /// the group being keyed on its hardware uid so a rename does not split
    /// it). Absent on a record that names no machine.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub machine: Option<String>,
    /// The endpoint string darkmux called. For a named endpoint it is a
    /// display label (the smallest seen), not part of the key.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub endpoint: Option<String>,
    /// The profile registry's `endpoints` id the call went through, when it
    /// had one; it, not `endpoint`, identifies a named endpoint.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub endpoint_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub requested_model: Option<String>,
    /// The reply's own `model`, kept only when it DIFFERS from the request:
    /// a served model that matches the request is not a fact worth a line.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reported_model: Option<String>,
    /// The operator's work calls.
    pub work: UsageSplit,
    /// darkmux's own utility calls (compaction, radio routing) at this
    /// endpoint/model, split out by each record's `purpose`.
    pub utility: UsageSplit,
}

impl UsageGroup {
    /// Work + utility, all tokens.
    pub fn total(&self) -> u64 {
        self.work.total + self.utility.total
    }
}

/// One purpose's share of a group.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, schemars::JsonSchema)]
pub struct UsageSplit {
    /// Usage records counted.
    pub calls: u64,
    pub total: u64,
    pub input: u64,
    /// Only when at least one record reported `cached_tokens`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cached: Option<u64>,
    pub generated: u64,
    /// Calls of `calls` whose reply reported no usage: counted as calls,
    /// adding 0 tokens (the endpoint window ledger reads them the same way;
    /// the conservative charge belongs to a dispatch's own cap only).
    pub unreported: u64,
}

impl UsageSplit {
    fn add(&mut self, a: &UsageAmount) {
        self.calls += 1;
        if !a.reported {
            self.unreported += 1;
        }
        self.total = self.total.saturating_add(a.total);
        self.input = self.input.saturating_add(a.prompt);
        self.generated = self.generated.saturating_add(a.completion);
        if let Some(c) = a.cached {
            self.cached = Some(self.cached.unwrap_or(0).saturating_add(c));
        }
    }
}

/// The breakdown `run list --usage` prints and `--json` emits: the overall
/// sum plus one [`UsageGroup`] per (executing machine, endpoint, requested
/// model, reported model), largest first, plus the calls that belong to no run.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, schemars::JsonSchema)]
pub struct UsageBreakdown {
    pub overall: UsageSum,
    pub groups: Vec<UsageGroup>,
    /// The calls whose record names no session and no mission (radio routing,
    /// a `doctor --probe`): in `overall`, on no run's TOKENS cell. The run rows
    /// plus this equal the overall.
    pub no_run: UsageSplit,
}

/// What makes two usage records one line of the breakdown. The endpoint
/// string is part of the key only for an UNNAMED endpoint: a named one is
/// its `endpoint_id` (the label can differ per model or deployment).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct GroupKey {
    /// The executing machine: its hardware uid (case-folded), else its
    /// `machine_id`, else none.
    machine: Option<String>,
    endpoint: Option<String>,
    endpoint_id: Option<String>,
    requested: Option<String>,
    reported: Option<String>,
}

/// The text a record carries under `k`, empty read as absent.
fn text_of(v: &serde_json::Value, k: &str) -> Option<String> {
    v.get(k).and_then(|x| x.as_str()).filter(|x| !x.is_empty()).map(str::to_string)
}

/// The machine a record names: the record's own `machine_uid`/`machine_id`,
/// stamped by the machine that wrote it, which is the one that executed the
/// call (a relayed run's asker writes no usage). Returns the key part and
/// the display name.
fn machine_of(v: &serde_json::Value) -> (Option<String>, Option<String>) {
    let name = text_of(v, "machine_id");
    let key = text_of(v, "machine_uid").map(|u| u.to_ascii_uppercase()).or_else(|| name.clone());
    (key, name)
}

fn group_key(v: &serde_json::Value) -> GroupKey {
    let p = payload_of(v);
    let requested = text_of(p, "requested_model");
    let reported = text_of(p, "reported_model").filter(|r| Some(r) != requested.as_ref());
    let endpoint_id = text_of(p, "endpoint_id");
    let endpoint = if endpoint_id.is_some() { None } else { text_of(p, "endpoint") };
    GroupKey { machine: machine_of(v).0, endpoint, endpoint_id, requested, reported }
}

/// The display facts of one record that are NOT part of its group's key:
/// the machine's name, the endpoint string, and when the record was written
/// (the newest name wins, the smallest endpoint string wins, so the line
/// reads the same whatever order the records arrive in).
struct GroupLabel {
    machine: Option<String>,
    endpoint: Option<String>,
    ts: String,
}

fn group_label(v: &serde_json::Value) -> GroupLabel {
    GroupLabel { machine: machine_of(v).1, endpoint: text_of(payload_of(v), "endpoint"), ts: text_of(v, "ts").unwrap_or_default() }
}

/// The fold: feed it every record in a window (any order), then `finish`.
/// Linear in the records, one small allocation per NEW run key or group.
/// `since` (an ISO `YYYY-MM-DDTHH:MM:SSZ` bound, inclusive) keeps records
/// stamped before it out of the sums; the flow schema's timestamps sort as
/// plain strings, so this is a lexical compare, the same one `runs.rs` uses
/// everywhere.
pub struct UsageFold {
    since: Option<String>,
    runs: Vec<RunEntry>,
    run_index: HashMap<(String, String), usize>,
    groups: HashMap<GroupKey, UsageGroup>,
    /// The timestamp of the record whose machine name each group shows.
    machine_ts: HashMap<GroupKey, String>,
    no_run: UsageSplit,
}

/// One (session, mission) pair's sum inside the fold: the unit a run row
/// reads its tokens from. Either id may be empty (a record names what it
/// names; neither means the record belongs to no run).
#[derive(Debug, Default)]
struct RunEntry {
    session_id: String,
    mission_id: String,
    sum: UsageSum,
}

impl UsageFold {
    pub fn new(since: Option<String>) -> Self {
        Self { since, runs: Vec::new(), run_index: HashMap::new(), groups: HashMap::new(), machine_ts: HashMap::new(), no_run: UsageSplit::default() }
    }

    /// The entry of the (session, mission) `v` names, created on first sight.
    fn run_slot(&mut self, v: &serde_json::Value) -> usize {
        let s = |k: &str| text_of(v, k).unwrap_or_default();
        let key = (s("session_id"), s("mission_id"));
        if let Some(&i) = self.run_index.get(&key) {
            return i;
        }
        self.runs.push(RunEntry { session_id: key.0.clone(), mission_id: key.1.clone(), sum: UsageSum::default() });
        self.run_index.insert(key, self.runs.len() - 1);
        self.runs.len() - 1
    }

    /// True when `v` is stamped inside the window (or carries no stamp:
    /// kept, the same posture the runs scan takes for an unattributable
    /// record).
    fn in_window(&self, v: &serde_json::Value) -> bool {
        match &self.since {
            Some(since) => {
                let ts = v.get("ts").and_then(|t| t.as_str()).unwrap_or("");
                ts.is_empty() || ts >= since.as_str()
            }
            None => true,
        }
    }

    /// Fold one flow record. Anything that is not a usage record is ignored
    /// at no cost beyond the action read.
    pub fn add(&mut self, v: &serde_json::Value) {
        if !is_usage_record(v) || !self.in_window(v) {
            return;
        }
        let amount = amount_of(payload_of(v));
        let i = self.run_slot(v);
        let entry = &mut self.runs[i];
        entry.sum.add(&amount);
        if entry.session_id.is_empty() && entry.mission_id.is_empty() {
            self.no_run.add(&amount);
        }
        self.group(group_key(v), group_label(v), &amount);
    }

    fn group(&mut self, key: GroupKey, label: GroupLabel, amount: &UsageAmount) {
        let g = self.groups.entry(key.clone()).or_insert_with(|| UsageGroup {
            machine: None,
            endpoint: None,
            endpoint_id: key.endpoint_id.clone(),
            requested_model: key.requested.clone(),
            reported_model: key.reported.clone(),
            work: UsageSplit::default(),
            utility: UsageSplit::default(),
        });
        if let Some(seen) = label.endpoint {
            if g.endpoint.as_ref().is_none_or(|kept| seen < *kept) {
                g.endpoint = Some(seen);
            }
        }
        let newest = self.machine_ts.get(&key).is_none_or(|seen| label.ts >= *seen);
        if label.machine.is_some() && newest {
            g.machine = label.machine;
            self.machine_ts.insert(key, label.ts);
        }
        match amount.purpose {
            // A purpose this build does not name is not known to be a utility job, and `Work` is
            // "every call that is not a utility job": its tokens are counted, never dropped.
            UsagePurpose::Work | UsagePurpose::Unknown => g.work.add(amount),
            UsagePurpose::Utility => g.utility.add(amount),
        }
    }

    /// Produce the index.
    pub fn finish(self) -> UsageIndex {
        let mut overall = UsageSum::default();
        let mut by_session: HashMap<String, Vec<usize>> = HashMap::new();
        let mut by_mission: HashMap<String, Vec<usize>> = HashMap::new();
        for (i, entry) in self.runs.iter().enumerate() {
            overall.merge(&entry.sum);
            if !entry.session_id.is_empty() {
                by_session.entry(entry.session_id.clone()).or_default().push(i);
            }
            if !entry.mission_id.is_empty() {
                by_mission.entry(entry.mission_id.clone()).or_default().push(i);
            }
        }
        let mut groups: Vec<UsageGroup> = self.groups.into_values().collect();
        // Largest first; ties by key so the order is stable across runs.
        groups.sort_by(|a, b| {
            b.total()
                .cmp(&a.total())
                .then_with(|| a.machine.cmp(&b.machine))
                .then_with(|| a.endpoint_id.cmp(&b.endpoint_id))
                .then_with(|| a.endpoint.cmp(&b.endpoint))
                .then_with(|| a.requested_model.cmp(&b.requested_model))
                .then_with(|| a.reported_model.cmp(&b.reported_model))
        });
        let runs = self.runs.into_iter().map(|e| (e.mission_id, e.sum)).collect();
        UsageIndex { runs, by_session, by_mission, breakdown: UsageBreakdown { overall, groups, no_run: self.no_run } }
    }
}

/// The finished fold: per-run sums a `Run` row reads its `tokens` from,
/// plus the breakdown.
#[derive(Debug, Default)]
pub struct UsageIndex {
    /// Each (session, mission) entry's mission id and sum.
    runs: Vec<(String, UsageSum)>,
    by_session: HashMap<String, Vec<usize>>,
    by_mission: HashMap<String, Vec<usize>>,
    pub breakdown: UsageBreakdown,
}

impl UsageIndex {
    /// ALL tokens (utility included) of the run whose records carry
    /// `mission_id` OR one of `session_ids`, each entry counted once.
    /// `None` when nothing was measured: no entry matched, or none of
    /// the matched entries' records reported a count.
    ///
    /// With a `mission_id`, a session hit counts only when its records
    /// name that mission or none: the scheduler stamps `session_id` from
    /// the TASK id (#1918), so two missions' steps can share one session,
    /// and a mission must never read the other's share of it (review
    /// CONSIDER 2). Without one (a ghost, a lab run) the session is read
    /// whole.
    pub fn tokens_for<'a>(
        &self,
        mission_id: Option<&str>,
        session_ids: impl IntoIterator<Item = &'a str>,
    ) -> Option<u64> {
        let mission_id = mission_id.filter(|m| !m.is_empty());
        let mut seen: HashSet<usize> = HashSet::new();
        let mut total = 0u64;
        let mut reported = 0u64;
        let mut take = |i: usize, runs: &[(String, UsageSum)]| {
            if seen.insert(i) {
                total = total.saturating_add(runs[i].1.total);
                reported += runs[i].1.reported;
            }
        };
        if let Some(mid) = mission_id {
            for &i in self.by_mission.get(mid).into_iter().flatten() {
                take(i, &self.runs);
            }
        }
        for sid in session_ids {
            for &i in self.by_session.get(sid).into_iter().flatten() {
                let owner = self.runs[i].0.as_str();
                if owner.is_empty() || mission_id.is_none() || mission_id == Some(owner) {
                    take(i, &self.runs);
                }
            }
        }
        (reported > 0).then_some(total)
    }

    /// The run keyed on exactly one session (a ghost, a lab run).
    pub fn tokens_for_session(&self, session_id: &str) -> Option<u64> {
        self.tokens_for(None, std::iter::once(session_id))
    }
}

/// (#2902) `<n><unit>` (`s`/`m`/`h`/`d`/`w`, the same units `run list`'s
/// own STARTED column speaks) as an epoch bound `n` units before `now`, or
/// a `YYYY-MM-DD` UTC date (the spelling `finding sync --since` already
/// takes) as that day's midnight. Anything else is an error naming both
/// forms. `now` is passed in so the parse is testable against a frozen
/// clock.
pub fn parse_since(spec: &str, now_secs: u64) -> Result<u64, String> {
    let spec = spec.trim();
    if let Some(secs) = crate::runs::parse_flow_ts(&format!("{spec}T00:00:00Z")) {
        return Ok(secs);
    }
    let unit_at = spec.find(|c: char| !c.is_ascii_digit()).unwrap_or(spec.len());
    let (digits, unit) = spec.split_at(unit_at);
    let per_unit: u64 = match unit {
        "s" => 1,
        "m" => 60,
        "h" => 3_600,
        "d" => 86_400,
        "w" => 604_800,
        _ => return Err(since_error(spec)),
    };
    let n: u64 = digits.parse().map_err(|_| since_error(spec))?;
    if n == 0 {
        return Err(since_error(spec));
    }
    Ok(now_secs.saturating_sub(n.saturating_mul(per_unit)))
}

fn since_error(spec: &str) -> String {
    format!("--since {spec:?}: expected a duration like 24h or 7d (units s, m, h, d, w) or a date YYYY-MM-DD")
}

/// The flow schema's own timestamp spelling for an epoch second, so a
/// `since` bound can be compared lexically against record `ts` values.
pub fn iso_from_epoch(secs: u64) -> String {
    let days = (secs / 86_400) as i64;
    let rem = secs % 86_400;
    let (y, m, d) = crate::runs::civil_from_days(days);
    format!("{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z", rem / 3600, (rem % 3600) / 60, rem % 60)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn golden_named(records: &str, expected: &str) -> (Vec<serde_json::Value>, serde_json::Value) {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/usage-golden");
        let records = std::fs::read_to_string(dir.join(records))
            .unwrap()
            .lines()
            .filter(|l| !l.trim().is_empty())
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        let expected = serde_json::from_str(&std::fs::read_to_string(dir.join(expected)).unwrap()).unwrap();
        (records, expected)
    }

    fn golden() -> (Vec<serde_json::Value>, serde_json::Value) {
        golden_named("records.jsonl", "expected.json")
    }

    /// The shared value domain (`domain.jsonl` + `clamp.jsonl`): both sums
    /// floor a finite number to an integer in `[0, 2^53]`, read anything
    /// else as 0, and judge "reported" by that same reading.
    #[test]
    fn shared_domain_goldens_match_the_viewer() {
        for (records_file, expected_file) in [("domain.jsonl", "domain-expected.json"), ("clamp.jsonl", "clamp-expected.json")] {
            let (records, expected) = golden_named(records_file, expected_file);
            let idx = fold_all(&records, None);
            let s = &idx.breakdown.overall;
            assert_eq!(shape(s.total, s.prompt, s.completion, s.cached), expected["overall"], "{records_file}: overall");
            assert_eq!(s.usage_records, expected["usage_records"].as_u64().unwrap(), "{records_file}");
            assert_eq!(s.reported, expected["reported_entries"].as_u64().unwrap(), "{records_file}: reported");
            if let Some(by_model) = expected.get("by_requested_model") {
                assert_eq!(breakdown_by(&records, "requested_model"), *by_model, "{records_file}");
            }
        }
    }

    /// The sums themselves never overflow: a run of clamped maxima
    /// saturates rather than wrapping (release) or panicking (debug).
    #[test]
    fn sums_saturate_instead_of_overflowing() {
        let huge = UsageAmount { total: u64::MAX, prompt: u64::MAX, completion: u64::MAX, cached: Some(u64::MAX), purpose: UsagePurpose::Utility, reported: true, spend: Some(u64::MAX) };
        let mut sum = UsageSum::default();
        sum.add(&huge);
        sum.add(&huge);
        assert_eq!((sum.total, sum.prompt, sum.completion, sum.cached, sum.utility), (u64::MAX, u64::MAX, u64::MAX, Some(u64::MAX), u64::MAX));
        let mut split = UsageSplit::default();
        split.add(&huge);
        split.add(&huge);
        assert_eq!((split.total, split.input, split.generated, split.cached), (u64::MAX, u64::MAX, u64::MAX, Some(u64::MAX)));
        // And the fold's overall, across two runs at the maximum.
        let rec = |sid: &str| serde_json::json!({"action":"telemetry.tokens","category":"telemetry","source":"tokens","session_id":sid,"payload":{"total_tokens":1e300,"prompt_tokens":1e300}});
        let mut fold = UsageFold::new(None);
        for _ in 0..3 {
            fold.add(&rec("a"));
            fold.add(&rec("b"));
        }
        let o = fold.finish().breakdown.overall;
        assert_eq!(o.total, 6 * (1u64 << 53), "six clamped maxima, exactly");
    }

    /// (#3067) A resumed dispatch reuses its execution id under a NEW session,
    /// so the execution is not the unit of attribution: each record counts
    /// under its OWN session. The resumed pair's two sessions read their own
    /// tokens, neither the other's.
    #[test]
    fn a_resumed_executions_records_count_under_their_own_sessions() {
        let usage = |sid: &str, total: u64| serde_json::json!({"action":"telemetry.tokens","category":"telemetry","source":"tokens","session_id":sid,"mission_id":"m","execution_id":"exec-shared","payload":{"token_source":"provider","total_tokens":total}});
        let idx = fold_all(&[usage("sess-first", 100), usage("sess-resumed", 40), usage("sess-resumed", 2)], None);
        assert_eq!(idx.tokens_for_session("sess-first"), Some(100), "the first session reads only its own");
        assert_eq!(idx.tokens_for_session("sess-resumed"), Some(42), "and the resumed one has its own, not `-`");
        assert_eq!(idx.breakdown.overall.total, 142);
    }

    /// The same session under another mission is another run (the scheduler
    /// stamps one session id from a TASK id, which two missions can share).
    #[test]
    fn the_same_session_under_another_mission_is_another_run() {
        let usage = |mission: &str, total: u64| serde_json::json!({"action":"telemetry.tokens","category":"telemetry","source":"tokens","session_id":"s","mission_id":mission,"payload":{"token_source":"provider","total_tokens":total}});
        let idx = fold_all(&[usage("m", 10), usage("other", 3)], None);
        assert_eq!(idx.tokens_for(Some("m"), ["s"]), Some(10));
        assert_eq!(idx.tokens_for(Some("other"), ["s"]), Some(3));
    }

    /// A call that names no session and no mission (radio routing, a
    /// `doctor --probe`) is in the overall and in `no_run`, on no run: the
    /// runs' tokens plus `no_run` are the overall.
    #[test]
    fn a_sessionless_call_is_counted_in_no_run_so_rows_plus_it_equal_the_total() {
        let run = serde_json::json!({"action":"telemetry.tokens","category":"telemetry","source":"tokens","session_id":"s1","payload":{"token_source":"provider","total_tokens":100}});
        let routing = serde_json::json!({"action":"telemetry.tokens","category":"telemetry","source":"tokens","handle":"radio-router","payload":{"purpose":"utility","call_kind":"single_shot","token_source":"provider","total_tokens":9}});
        let idx = fold_all(&[run, routing.clone(), routing], None);
        assert_eq!(idx.breakdown.overall.total, 118);
        assert_eq!((idx.breakdown.no_run.calls, idx.breakdown.no_run.total), (2, 18));
        assert_eq!(idx.tokens_for_session("s1").unwrap() + idx.breakdown.no_run.total, idx.breakdown.overall.total);
    }

    /// One usage record as the machine that EXECUTED the call wrote it.
    fn usage_on(machine: &str, uid: &str, endpoint_id: Option<&str>, purpose: &str, model: &str, total: Option<u64>) -> serde_json::Value {
        let mut p = serde_json::json!({"call_kind":"single_shot","purpose":purpose,"requested_model":model,"endpoint":"http://localhost:1234/v1"});
        match total {
            Some(t) => {
                p["token_source"] = "provider".into();
                p["prompt_tokens"] = (t - 1).into();
                p["completion_tokens"] = 1.into();
                p["total_tokens"] = t.into();
            }
            None => p["token_source"] = "absent".into(),
        }
        if let Some(id) = endpoint_id {
            p["endpoint_id"] = id.into();
        }
        serde_json::json!({"ts":"2026-10-02T06:00:00Z","action":"telemetry.tokens","category":"telemetry","source":"tokens","session_id":format!("s-{machine}"),"execution_id":format!("exec-{machine}-{model}-{purpose}-{total:?}"),"machine_id":machine,"machine_uid":uid,"payload":p})
    }

    /// (#3067) `localhost` means a different machine to whoever made the
    /// call: the same URL and model on two machines are two rows, and the
    /// overall still sums both.
    #[test]
    fn one_localhost_on_two_machines_is_two_groups() {
        let records = [
            usage_on("MacBook-Pro", "UID-A", None, "work", "darkmux:phi-4", Some(100)),
            usage_on("studio", "UID-B", None, "work", "darkmux:phi-4", Some(30)),
        ];
        let b = fold_all(&records, None).breakdown;
        assert_eq!(b.groups.len(), 2, "{:#?}", b.groups);
        let by: std::collections::BTreeMap<_, _> = b.groups.iter().map(|g| (g.machine.clone().unwrap(), g.total())).collect();
        assert_eq!(by, [("MacBook-Pro".to_string(), 100), ("studio".to_string(), 30)].into());
        assert_eq!(b.overall.total, 130);
    }

    /// A machine renamed mid-window is still one machine: the hardware uid
    /// is the key, the newest `machine_id` the label.
    #[test]
    fn a_renamed_machine_stays_one_group_labeled_by_its_newest_name() {
        let mut old = usage_on("studio", "UID-B", None, "work", "m", Some(10));
        old["ts"] = "2026-10-01T06:00:00Z".into();
        let mut new = usage_on("studio-renamed", "uid-b", None, "work", "m", Some(20));
        new["ts"] = "2026-10-02T06:00:00Z".into();
        for order in [[old.clone(), new.clone()], [new, old]] {
            let g = fold_all(&order, None).breakdown.groups;
            assert_eq!(g.len(), 1, "{g:#?}");
            assert_eq!((g[0].machine.as_deref(), g[0].total()), (Some("studio-renamed"), 30));
        }
    }

    /// A named endpoint is identified by its registry id: its rows carry the
    /// id, and one hosted endpoint used from two machines is two rows
    /// (each machine's spend is enforced locally), summed by the overall.
    #[test]
    fn a_hosted_endpoint_used_from_two_machines_is_two_rows_keyed_by_its_id() {
        let mut other_label = usage_on("MacBook-Pro", "UID-A", Some("azure"), "work", "gpt", Some(50));
        other_label["payload"]["endpoint"] = "https://x.example/gpt".into();
        let records = [
            usage_on("MacBook-Pro", "UID-A", Some("azure"), "work", "gpt", Some(100)),
            other_label,
            usage_on("studio", "UID-B", Some("azure"), "work", "gpt", Some(7)),
        ];
        let b = fold_all(&records, None).breakdown;
        assert_eq!(b.groups.len(), 2, "one id on one machine is one row whatever its label: {:#?}", b.groups);
        assert!(b.groups.iter().all(|g| g.endpoint_id.as_deref() == Some("azure")));
        assert_eq!(b.groups[0].total(), 150);
        assert_eq!(b.overall.total, 157);
    }

    /// A relayed run executes on the peer, which writes the usage; the asker
    /// writes only bookends with no counts. Tokens count once, on the
    /// executor.
    #[test]
    fn a_relayed_run_counts_once_on_the_executor() {
        let sid = "radio.solo.relay.MacBook-Pro.radio.solo.adhoc.radio-host.1";
        let mut exec = usage_on("studio", "UID-B", None, "work", "darkmux:phi-4", Some(60));
        exec["session_id"] = sid.into();
        let asker_start = serde_json::json!({"ts":"2026-10-02T06:00:00Z","action":"dispatch.start","session_id":"radio-ask","machine_id":"MacBook-Pro","machine_uid":"UID-A"});
        let asker_end = serde_json::json!({"ts":"2026-10-02T06:00:05Z","action":"dispatch.complete","session_id":"radio-ask","machine_id":"MacBook-Pro","machine_uid":"UID-A","payload":{}});
        let idx = fold_all(&[asker_start, exec, asker_end], None);
        let g = &idx.breakdown.groups;
        assert_eq!(g.len(), 1, "{g:#?}");
        assert_eq!((g[0].machine.as_deref(), g[0].work.calls, g[0].total()), (Some("studio"), 1, 60));
        assert_eq!(idx.breakdown.overall.total, 60);
    }

    /// A utility call is a utility row of ITS machine, never specialist work.
    #[test]
    fn a_utility_call_lands_in_the_utility_split_of_its_machines_row() {
        let records = [
            usage_on("MacBook-Pro", "UID-A", None, "utility", "darkmux:qwen3-4b", Some(9)),
            usage_on("MacBook-Pro", "UID-A", None, "work", "darkmux:coder", Some(90)),
        ];
        let g = fold_all(&records, None).breakdown.groups;
        let util = g.iter().find(|g| g.requested_model.as_deref() == Some("darkmux:qwen3-4b")).unwrap();
        assert_eq!((util.utility.total, util.work.calls), (9, 0));
        let work = g.iter().find(|g| g.requested_model.as_deref() == Some("darkmux:coder")).unwrap();
        assert_eq!((work.work.total, work.utility.calls), (90, 0));
    }

    /// A call whose reply reported no usage is a counted call that adds 0
    /// tokens and says so (`unreported`), the same reading the endpoint
    /// window ledger takes (`known: 0, metered: false`); the conservative
    /// charge applies only to a dispatch's own cap.
    #[test]
    fn a_usage_less_call_counts_as_a_call_of_zero_tokens_and_is_flagged() {
        let records = [
            usage_on("MacBook-Pro", "UID-A", None, "work", "m", None),
            usage_on("MacBook-Pro", "UID-A", None, "work", "m", Some(40)),
        ];
        let g = fold_all(&records, None).breakdown.groups;
        assert_eq!(g.len(), 1);
        assert_eq!((g[0].work.calls, g[0].work.total, g[0].work.unreported), (2, 40, 1));
        assert_eq!(usage_contribution(&records[0]).unwrap().spend, None, "the window ledger reads it as unmetered");
    }

    fn fold_all(records: &[serde_json::Value], since: Option<&str>) -> UsageIndex {
        let mut fold = UsageFold::new(since.map(str::to_string));
        for r in records {
            fold.add(r);
        }
        fold.finish()
    }

    fn shape(total: u64, input: u64, generated: u64, cached: Option<u64>) -> serde_json::Value {
        serde_json::json!({ "total": total, "input": input, "generated": generated, "cached": cached })
    }

    /// The per-field breakdown the TS test computes the same way: every
    /// usage record's contribution, keyed on one payload field, `(none)`
    /// when absent.
    fn breakdown_by(records: &[serde_json::Value], field: &str) -> serde_json::Value {
        let refs: Vec<&serde_json::Value> = records.iter().collect();
        let mut out: std::collections::BTreeMap<String, u64> = Default::default();
        let mut add = |r: &serde_json::Value, a: &UsageAmount| {
            let k = payload_of(r).get(field).and_then(|v| v.as_str()).unwrap_or("(none)").to_string();
            *out.entry(k).or_default() += a.total;
        };
        for r in &refs {
            if let Some(a) = usage_contribution(r) {
                add(r, &a);
            }
        }
        serde_json::to_value(out).unwrap()
    }

    /// The shared golden: this fold and the viewer's `sumUsage` must agree
    /// on every figure `tests/usage-golden/expected.json` pins.
    #[test]
    fn shared_golden_matches_the_viewer() {
        let (records, expected) = golden();
        let idx = fold_all(&records, None);
        let s = &idx.breakdown.overall;
        assert_eq!(shape(s.total, s.prompt, s.completion, s.cached), expected["overall"], "overall");
        assert_eq!(s.usage_records, expected["usage_records"].as_u64().unwrap());
        assert_eq!(s.utility, expected["by_purpose"]["utility"]["total"].as_u64().unwrap());

        // The groups, re-summed by purpose, are the golden's by_purpose.
        let (mut work, mut util) = (UsageSplit::default(), UsageSplit::default());
        for g in &idx.breakdown.groups {
            for (into, from) in [(&mut work, &g.work), (&mut util, &g.utility)] {
                into.calls += from.calls;
                into.total += from.total;
                into.input += from.input;
                into.generated += from.generated;
                if let Some(c) = from.cached {
                    into.cached = Some(into.cached.unwrap_or(0) + c);
                }
            }
        }
        let purpose_shape = |s: &UsageSplit| serde_json::json!({ "total": s.total, "input": s.input, "generated": s.generated });
        assert_eq!(purpose_shape(&work), expected["by_purpose"]["work"], "work");
        assert_eq!(purpose_shape(&util), expected["by_purpose"]["utility"], "utility");
        assert_eq!(shape(work.total, work.input, work.generated, work.cached), expected["excluding_utility"]);
        assert_eq!(work.calls + util.calls, s.usage_records, "every counted record lands in one split");

        // The groups, re-keyed on one field, are the golden's per-field maps.
        let by = |field: &str| {
            let mut out: std::collections::BTreeMap<String, u64> = Default::default();
            for g in &idx.breakdown.groups {
                let k = match field {
                    "endpoint" => g.endpoint.clone(),
                    "requested_model" => g.requested_model.clone(),
                    _ => unreachable!(),
                };
                *out.entry(k.unwrap_or_else(|| "(none)".into())).or_default() += g.total();
            }
            serde_json::to_value(out).unwrap()
        };
        assert_eq!(by("endpoint"), expected["by_endpoint"]);
        assert_eq!(by("requested_model"), expected["by_requested_model"]);
        assert_eq!(breakdown_by(&records, "call_kind"), expected["by_call_kind"]);
        assert_eq!(breakdown_by(&records, "endpoint"), expected["by_endpoint"]);
        assert_eq!(breakdown_by(&records, "requested_model"), expected["by_requested_model"]);

        // `sum_usage` is the same fold.
        assert_eq!(&sum_usage(records.iter()), s);
    }

    /// A `dispatch complete` carries no tokens here, whatever its payload
    /// says: the golden's old runs that wrote only a complete (and a run
    /// whose usage record is count-less) read as unmeasured, never as their
    /// complete's total.
    #[test]
    fn a_dispatch_complete_is_never_read_for_tokens() {
        let (records, _) = golden();
        let idx = fold_all(&records, None);
        assert_eq!(idx.tokens_for_session("crew-dispatch-old-1"), None, "its 1,000 live only on a complete");
        let only_absent = vec![
            serde_json::json!({"action":"telemetry.tokens","category":"telemetry","source":"tokens","session_id":"s-abs","payload":{"token_source":"absent","call_kind":"turn"}}),
            serde_json::json!({"action":"dispatch.complete","session_id":"s-abs","payload":{"total_tokens":500}}),
        ];
        let idx = fold_all(&only_absent, None);
        assert_eq!(idx.breakdown.overall.total, 0);
        assert_eq!(idx.tokens_for_session("s-abs"), None, "nothing measured reads as absent, never 0");
    }

    /// Per-run attribution: a mission's tokens are the keys carrying its
    /// `mission_id` plus its step sessions, each key once.
    #[test]
    fn tokens_for_a_mission_unions_mission_id_and_step_sessions_without_double_counting() {
        let (records, _) = golden();
        let idx = fold_all(&records, None);
        // m-golden's keyed records: step-coder (300 = 120+90+180... see
        // golden: coder turns 120+180 + compaction 90 = 390), analyst 500,
        // task-probe 183, step-fixer 80 => 1153.
        assert_eq!(idx.tokens_for(Some("m-golden"), std::iter::empty()), Some(1153));
        // Naming a step session the mission_id already covers adds nothing.
        assert_eq!(idx.tokens_for(Some("m-golden"), ["step-coder-m-golden"]), Some(1153));
        // A session alone.
        assert_eq!(idx.tokens_for_session("step-coder-m-golden"), Some(390));
        // The radio session has no mission_id: reachable by session only.
        assert_eq!(idx.tokens_for_session("crew-dispatch-radio-router-1"), Some(35));
        assert_eq!(idx.tokens_for(Some("no-such-mission"), std::iter::empty()), None);
        // Whole-window total is the plain sum of every key.
        assert_eq!(idx.breakdown.overall.total, 1228);
    }

    /// `since` bounds a run's tokens to the window: records stamped before
    /// it are out of the sums (and out of the groups), those after it in.
    #[test]
    fn since_keeps_only_the_windows_share_of_a_run() {
        let usage = |ts: &str, total: u64| serde_json::json!({"action":"telemetry.tokens","category":"telemetry","source":"tokens","session_id":"S1","ts":ts,"payload":{"call_kind":"turn","purpose":"work","requested_model":"m","endpoint":"http://h/v1","token_source":"provider","total_tokens":total}});
        let records = vec![usage("2026-09-26T08:01:00Z", 120), usage("2026-09-26T08:02:00Z", 90), usage("2026-09-26T09:30:00Z", 180)];
        let whole = fold_all(&records, None);
        assert_eq!(whole.tokens_for_session("S1"), Some(390));
        let bounded = fold_all(&records, Some("2026-09-26T09:00:00Z"));
        assert_eq!((bounded.breakdown.overall.total, bounded.tokens_for_session("S1")), (180, Some(180)));
        let none = fold_all(&records, Some("2026-09-26T10:00:00Z"));
        assert_eq!(none.tokens_for_session("S1"), None, "nothing measured in the window");
        assert!(none.breakdown.groups.is_empty(), "no (none) group: {:?}", none.breakdown.groups);
    }

    /// (review CONSIDER 2) Two missions whose steps share a session id (the
    /// scheduler stamps `task-<id>`, which carries no per-run identity):
    /// each mission reads only the keys carrying its own `mission_id` or
    /// no mission at all, never the other mission's.
    #[test]
    fn tokens_for_a_mission_never_reads_another_missions_share_of_a_common_session() {
        let usage = |mid: &str, total: u64| serde_json::json!({"action":"telemetry.tokens","category":"telemetry","source":"tokens","session_id":"task-t1","mission_id":mid,"execution_id":format!("exec-{mid}"),"payload":{"token_source":"provider","total_tokens":total}});
        let records = vec![
            usage("M-A", 100),
            usage("M-B", 1000),
            // A record on the same session naming no mission: reachable by either.
            serde_json::json!({"action":"telemetry.tokens","category":"telemetry","source":"tokens","session_id":"task-t1","execution_id":"exec-none","payload":{"token_source":"provider","total_tokens":7}}),
        ];
        let idx = fold_all(&records, None);
        assert_eq!(idx.tokens_for(Some("M-A"), ["task-t1"]), Some(107));
        assert_eq!(idx.tokens_for(Some("M-B"), ["task-t1"]), Some(1007));
        assert_eq!(idx.tokens_for(Some("M-A"), std::iter::empty()), Some(100));
        // With no mission to filter on (a ghost), the session is read whole.
        assert_eq!(idx.tokens_for_session("task-t1"), Some(1107));
    }

    /// `reported_model` is a group key only when it differs from the
    /// request; a matching served model is folded into the request's line.
    #[test]
    fn reported_model_splits_a_group_only_when_it_differs() {
        let rec = |reported: Option<&str>, total: u64| {
            let mut p = serde_json::json!({"call_kind":"turn","purpose":"work","requested_model":"qwen","endpoint":"http://h/v1","token_source":"provider","total_tokens":total});
            if let Some(r) = reported {
                p["reported_model"] = serde_json::json!(r);
            }
            serde_json::json!({"action":"telemetry.tokens","category":"telemetry","source":"tokens","session_id":"s","payload":p})
        };
        let idx = fold_all(&[rec(None, 10), rec(Some("qwen"), 20), rec(Some("qwen-served"), 40)], None);
        let g = &idx.breakdown.groups;
        assert_eq!(g.len(), 2, "{g:#?}");
        assert_eq!((g[0].reported_model.as_deref(), g[0].total()), (Some("qwen-served"), 40));
        assert_eq!((g[1].reported_model.as_deref(), g[1].total()), (None, 30));
    }

    /// Utility is split out by each record's purpose inside a group, and
    /// a record without `purpose` (before flow schema 1.59.0) reads a
    /// compaction as utility and anything else as work.
    #[test]
    fn utility_is_split_by_purpose_within_a_group() {
        let (records, _) = golden();
        let idx = fold_all(&records, None);
        let lm = idx
            .breakdown
            .groups
            .iter()
            .find(|g| g.requested_model.as_deref() == Some("util-4b"))
            .expect("util-4b group");
        // 90 (compaction, purpose utility) + 35 (radio, purpose utility) +
        // 20 (compaction with NO purpose: read as utility) = 145; no work.
        assert_eq!((lm.utility.total, lm.utility.calls, lm.work.total), (145, 3, 0));
        let qa = idx.breakdown.groups.iter().find(|g| g.requested_model.as_deref() == Some("qwen-a")).unwrap();
        assert_eq!((qa.work.total, qa.work.cached, qa.utility.total), (360, Some(40), 0));
    }

    /// `cached` is absent, never 0, until a record reports it.
    #[test]
    fn cached_is_absent_until_reported() {
        let (records, _) = golden();
        let idx = fold_all(&records, None);
        let qb = idx.breakdown.groups.iter().find(|g| g.requested_model.as_deref() == Some("qwen-b")).unwrap();
        assert_eq!(qb.work.cached, None);
        assert_eq!(idx.breakdown.overall.cached, Some(140));
        let none = fold_all(&records[..2], None);
        assert_eq!(none.breakdown.overall.cached, None);
        assert_eq!(serde_json::to_value(&none.breakdown.overall).unwrap().get("cached"), None, "absent on the wire");
    }

    /// `since` drops records stamped before the bound, inclusive at it.
    /// Fixed timestamps on both sides: no clock.
    #[test]
    fn since_bounds_the_fold_lexically_and_inclusively() {
        let (records, _) = golden();
        // Everything from the radio dispatch (10:11) on: radio 35 + old-2
        // turn 40 + fixer 80 = 155.
        let idx = fold_all(&records, Some("2026-09-26T10:11:00Z"));
        assert_eq!(idx.breakdown.overall.total, 155);
        // One second later the radio usage record (10:11:00) is out: 120.
        let idx = fold_all(&records, Some("2026-09-26T10:11:01Z"));
        assert_eq!(idx.breakdown.overall.total, 120);
        // A bound after the old-2 turn: only the fixer's 80 remain.
        let idx = fold_all(&records, Some("2026-09-26T10:16:00Z"));
        assert_eq!(idx.breakdown.overall.total, 80);
        assert_eq!(idx.tokens_for_session("crew-dispatch-old-2"), None, "nothing of it is in the window");
        // A record with no ts is kept.
        let idx = fold_all(
            &[serde_json::json!({"action":"telemetry.tokens","category":"telemetry","source":"tokens","session_id":"s","payload":{"total_tokens":7}})],
            Some("2030-01-01T00:00:00Z"),
        );
        assert_eq!(idx.breakdown.overall.total, 7);
    }

    // ── parse_since: frozen clock ──────────────────────────────────────

    const NOW: u64 = 1_790_000_000; // 2026-09-21T14:13:20Z

    #[test]
    fn parse_since_durations_count_back_from_the_given_now() {
        assert_eq!(parse_since("30s", NOW), Ok(NOW - 30));
        assert_eq!(parse_since("15m", NOW), Ok(NOW - 900));
        assert_eq!(parse_since("24h", NOW), Ok(NOW - 86_400));
        assert_eq!(parse_since("7d", NOW), Ok(NOW - 7 * 86_400));
        assert_eq!(parse_since("2w", NOW), Ok(NOW - 14 * 86_400));
        assert_eq!(parse_since(" 1d ", NOW), Ok(NOW - 86_400), "whitespace tolerated");
    }

    #[test]
    fn parse_since_date_is_that_utc_midnight_and_ignores_now() {
        assert_eq!(parse_since("2026-09-20", NOW), Ok(1_789_862_400));
        assert_eq!(parse_since("2026-09-20", 0), Ok(1_789_862_400));
        assert_eq!(iso_from_epoch(1_789_862_400), "2026-09-20T00:00:00Z");
        assert_eq!(iso_from_epoch(NOW), "2026-09-21T14:13:20Z");
    }

    #[test]
    fn parse_since_rejects_what_it_cannot_read_by_naming_both_forms() {
        for bad in ["", "abc", "7", "7x", "0d", "-1d", "2026-13-01", "2026/09/20", "1.5h"] {
            let err = parse_since(bad, NOW).unwrap_err();
            assert!(err.contains("24h") && err.contains("YYYY-MM-DD"), "{bad:?}: {err}");
        }
    }
}