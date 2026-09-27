//! (#2902 step 2b) The Rust twin of the viewer's ONE token sum.
//!
//! Every model call darkmux makes emits exactly one `telemetry.tokens`
//! usage record (writer: `darkmux_crew::usage`), and a token total anywhere
//! is a PLAIN SUM of those records: no run keying, no complete-vs-telemetry
//! precedence, no dedup, no local/cloud classification, no estimates. This
//! module is that sum on the server side, shared by `darkmux run list`
//! (`src/run_list.rs`, the TOKENS column and `--usage`) and the daemon's
//! `GET /runs` (each row's `tokens`), the same way `runs::build_runs` is
//! shared by both. `ui/src/lib/usageRecords.ts::sumUsage` is the viewer's
//! half; the shared golden fixture `tests/usage-golden/` pins both to one
//! answer (`shared_golden_matches_the_viewer` below, and the TS side's own
//! test over the same files).
//!
//! THE ONE EXCEPTION is legacy data, isolated in [`UsageFold`]'s finish: a
//! run key `(session_id, mission_id)` with ZERO usage records (written
//! before flow schema 1.57.0, or by a fleet peer on an older darkmux)
//! counts each of its token-bearing `dispatch complete` records once. A
//! run with any usage record, even a count-less `token_source: "absent"`
//! one, never reads its complete.
//!
//! What this module reports is what darkmux INVOKED: the endpoint string
//! the record carries, the model darkmux requested, the model the reply
//! named when it differs, and the provider's own counts with their source.
//! Nothing here labels an endpoint local, cloud or metered, and nothing
//! here costs anything.

use std::collections::{HashMap, HashSet};

use darkmux_crew::usage::UsagePurpose;

/// The sum of a set of records: the Rust twin of `usageRecords.ts`'s
/// `UsageSum`, field for field.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize)]
pub struct UsageSum {
    /// Sum of each record's total (the provider's own, else prompt +
    /// completion, else a legacy complete's `remote_tokens`).
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
    /// Counted entries (usage records or legacy completes) that reported a
    /// count: `0` means nothing measured, which a cell shows as `-`, never 0.
    pub reported: u64,
    /// Legacy `dispatch complete` records counted.
    pub legacy_completes: u64,
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
    }

    /// Fold another sum into this one (the overall over every run key).
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
        self.legacy_completes += o.legacy_completes;
    }
}

// (#2902 step 5) The per-record half of the sum (what ONE usage record
// contributes, and the value domain it reads counts in) lives in
// `darkmux_crew::usage`, so the endpoint budget's rolling-window sum (in
// `darkmux_crew::budget`, below this crate in the dependency graph) and this
// fold read a record the same way. Re-exported here unchanged.
pub use darkmux_crew::usage::{is_usage_record, usage_contribution, usage_purpose, UsageAmount, MAX_COUNT};
use darkmux_crew::usage::{amount_of, has_any_token_counts, payload_of};

/// The identity of ONE RUN, `(session_id, mission_id)`: the twin of
/// `runKey`. A bare session id is not one (`session_id::task` is
/// deterministic, so the same id recurs across unrelated runs, #2690/#2709).
/// A sessionless record gets a composite of its own; `\0` cannot occur
/// inside either id.
fn run_key(v: &serde_json::Value) -> String {
    let s = |k: &str| v.get(k).and_then(|x| x.as_str()).unwrap_or("");
    let sid = s("session_id");
    let mid = s("mission_id");
    if sid.is_empty() {
        format!("ts:{}:{}:{}\0{mid}", s("ts"), s("handle"), s("machine_uid"))
    } else {
        format!("{sid}\0{mid}")
    }
}

/// The records the legacy fallback counts (the twin of
/// `legacyCompleteCounts`): every token-bearing `dispatch complete` whose
/// run key holds no usage record.
pub fn legacy_complete_counts<'a>(records: &[&'a serde_json::Value]) -> Vec<&'a serde_json::Value> {
    let with_usage: HashSet<String> =
        records.iter().filter(|r| is_usage_record(r)).map(|r| run_key(r)).collect();
    records
        .iter()
        .copied()
        .filter(|r| is_legacy_fallback_complete(r, &with_usage))
        .collect()
}

fn is_dispatch_complete(v: &serde_json::Value) -> bool {
    v.get("action").and_then(|a| a.as_str()).is_some_and(darkmux_flow::is_dispatch_complete)
}

fn is_legacy_fallback_complete(v: &serde_json::Value, with_usage: &HashSet<String>) -> bool {
    is_dispatch_complete(v) && has_any_token_counts(payload_of(v)) && !with_usage.contains(&run_key(v))
}

/// THE sum over a slice of records: [`UsageFold`] driven to completion.
pub fn sum_usage<'a>(records: impl IntoIterator<Item = &'a serde_json::Value>) -> UsageSum {
    let mut fold = UsageFold::new(None);
    for r in records {
        fold.add(r);
    }
    fold.finish().breakdown.overall
}

/// One line of the breakdown: everything darkmux invoked one way. The key
/// is the endpoint it called, the model it requested, and the model the
/// reply named when it carried one; each is a fact off the record, absent
/// when the record did not carry it (a legacy complete, a pre-1.57 turn).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct UsageGroup {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub endpoint: Option<String>,
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
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize)]
pub struct UsageSplit {
    /// Records counted (usage records or legacy completes).
    pub calls: u64,
    pub total: u64,
    pub input: u64,
    /// Only when at least one record reported `cached_tokens`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cached: Option<u64>,
    pub generated: u64,
}

impl UsageSplit {
    fn add(&mut self, a: &UsageAmount) {
        self.calls += 1;
        self.total = self.total.saturating_add(a.total);
        self.input = self.input.saturating_add(a.prompt);
        self.generated = self.generated.saturating_add(a.completion);
        if let Some(c) = a.cached {
            self.cached = Some(self.cached.unwrap_or(0).saturating_add(c));
        }
    }
}

/// The breakdown `run list --usage` prints and `--json` emits: the overall
/// sum plus one [`UsageGroup`] per (endpoint, requested model, reported
/// model), largest first.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize)]
pub struct UsageBreakdown {
    pub overall: UsageSum,
    pub groups: Vec<UsageGroup>,
}

type GroupKey = (Option<String>, Option<String>, Option<String>);

fn group_key(v: &serde_json::Value) -> GroupKey {
    let p = payload_of(v);
    let s = |k: &str| p.get(k).and_then(|x| x.as_str()).filter(|x| !x.is_empty()).map(str::to_string);
    let requested = s("requested_model");
    let reported = s("reported_model").filter(|r| Some(r) != requested.as_ref());
    (s("endpoint"), requested, reported)
}

/// A pending legacy candidate: held until the fold knows whether its run
/// key ever saw a usage record.
struct PendingComplete {
    run: usize,
    group: GroupKey,
    amount: UsageAmount,
}

/// The fold: feed it every record in a window (any order), then `finish`.
/// Linear in the records, one small allocation per NEW run key or group.
/// `since` (an ISO `YYYY-MM-DDTHH:MM:SSZ` bound, inclusive) keeps records
/// stamped before it out of the SUMS only; the flow schema's timestamps
/// sort as plain strings, so this is a lexical compare, the same one
/// `runs.rs` uses everywhere. Every usage record the pass visits still
/// marks its run key as one that HAS usage records, whatever its stamp:
/// the legacy rule asks whether the run ever wrote one, and a bound that
/// cut a modern run's turns off must not turn its complete into a legacy
/// count (review MUST FIX 1).
pub struct UsageFold {
    since: Option<String>,
    runs: Vec<RunEntry>,
    run_index: HashMap<String, usize>,
    groups: HashMap<GroupKey, UsageGroup>,
    completes: Vec<PendingComplete>,
}

/// The two halves of a run key, kept so a caller can look a run up by
/// either.
#[derive(Debug, Clone, Default)]
struct RunKeyParts {
    session_id: String,
    mission_id: String,
}

/// One run key's state inside the fold.
#[derive(Debug, Default)]
struct RunEntry {
    parts: RunKeyParts,
    /// True once ANY usage record for this key was visited, in the window
    /// or not — the legacy rule's question. `sum.usage_records` counts
    /// only the ones in the window.
    has_usage: bool,
    sum: UsageSum,
}

impl UsageFold {
    pub fn new(since: Option<String>) -> Self {
        Self { since, runs: Vec::new(), run_index: HashMap::new(), groups: HashMap::new(), completes: Vec::new() }
    }

    fn run_slot(&mut self, v: &serde_json::Value) -> usize {
        let key = run_key(v);
        if let Some(&i) = self.run_index.get(&key) {
            return i;
        }
        let s = |k: &str| v.get(k).and_then(|x| x.as_str()).unwrap_or("").to_string();
        let parts = RunKeyParts { session_id: s("session_id"), mission_id: s("mission_id") };
        self.runs.push(RunEntry { parts, ..Default::default() });
        let i = self.runs.len() - 1;
        self.run_index.insert(key, i);
        i
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

    /// Fold one flow record. Anything that is neither a usage record nor a
    /// `dispatch complete` is ignored at no cost beyond the action read.
    pub fn add(&mut self, v: &serde_json::Value) {
        if is_usage_record(v) {
            let i = self.run_slot(v);
            self.runs[i].has_usage = true;
            if !self.in_window(v) {
                return;
            }
            let amount = amount_of(payload_of(v));
            let sum = &mut self.runs[i].sum;
            sum.add(&amount);
            sum.usage_records += 1;
            self.group(group_key(v), &amount);
        } else if is_dispatch_complete(v) && self.in_window(v) && has_any_token_counts(payload_of(v)) {
            let run = self.run_slot(v);
            self.completes.push(PendingComplete { run, group: group_key(v), amount: amount_of(payload_of(v)) });
        }
    }

    fn group(&mut self, key: GroupKey, amount: &UsageAmount) {
        let g = self.groups.entry(key.clone()).or_insert_with(|| UsageGroup {
            endpoint: key.0,
            requested_model: key.1,
            reported_model: key.2,
            work: UsageSplit::default(),
            utility: UsageSplit::default(),
        });
        match amount.purpose {
            UsagePurpose::Work => g.work.add(amount),
            UsagePurpose::Utility => g.utility.add(amount),
        }
    }

    /// Apply the legacy rule and produce the index.
    pub fn finish(mut self) -> UsageIndex {
        let completes = std::mem::take(&mut self.completes);
        for c in completes {
            let entry = &mut self.runs[c.run];
            if entry.has_usage {
                continue;
            }
            entry.sum.add(&c.amount);
            entry.sum.legacy_completes += 1;
            self.group(c.group, &c.amount);
        }
        let mut overall = UsageSum::default();
        let mut by_session: HashMap<String, Vec<usize>> = HashMap::new();
        let mut by_mission: HashMap<String, Vec<usize>> = HashMap::new();
        for (i, entry) in self.runs.iter().enumerate() {
            overall.merge(&entry.sum);
            if !entry.parts.session_id.is_empty() {
                by_session.entry(entry.parts.session_id.clone()).or_default().push(i);
            }
            if !entry.parts.mission_id.is_empty() {
                by_mission.entry(entry.parts.mission_id.clone()).or_default().push(i);
            }
        }
        let mut groups: Vec<UsageGroup> = self.groups.into_values().collect();
        // Largest first; ties by key so the order is stable across runs.
        groups.sort_by(|a, b| {
            b.total()
                .cmp(&a.total())
                .then_with(|| a.endpoint.cmp(&b.endpoint))
                .then_with(|| a.requested_model.cmp(&b.requested_model))
                .then_with(|| a.reported_model.cmp(&b.reported_model))
        });
        let runs = self.runs.into_iter().map(|e| (e.parts, e.sum)).collect();
        UsageIndex { runs, by_session, by_mission, breakdown: UsageBreakdown { overall, groups } }
    }
}

/// The finished fold: per-run sums a `Run` row reads its `tokens` from,
/// plus the breakdown.
#[derive(Debug, Default)]
pub struct UsageIndex {
    runs: Vec<(RunKeyParts, UsageSum)>,
    by_session: HashMap<String, Vec<usize>>,
    by_mission: HashMap<String, Vec<usize>>,
    pub breakdown: UsageBreakdown,
}

impl UsageIndex {
    /// ALL tokens (utility included) of the run whose records carry
    /// `mission_id` OR one of `session_ids`, each run key counted once.
    /// `None` when nothing was measured: no run key matched, or none of
    /// the matched keys' records reported a count.
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
        let mut take = |i: usize, runs: &[(RunKeyParts, UsageSum)]| {
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
                let owner = self.runs[i].0.mission_id.as_str();
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
    /// else as 0, and judge "reported" (usage record or legacy complete
    /// alike) by that same reading.
    #[test]
    fn shared_domain_goldens_match_the_viewer() {
        for (records_file, expected_file) in [("domain.jsonl", "domain-expected.json"), ("clamp.jsonl", "clamp-expected.json")] {
            let (records, expected) = golden_named(records_file, expected_file);
            let idx = fold_all(&records, None);
            let s = &idx.breakdown.overall;
            assert_eq!(shape(s.total, s.prompt, s.completion, s.cached), expected["overall"], "{records_file}: overall");
            assert_eq!(s.usage_records, expected["usage_records"].as_u64().unwrap(), "{records_file}");
            assert_eq!(s.legacy_completes, expected["legacy_completes_counted"].as_u64().unwrap(), "{records_file}");
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
        let huge = UsageAmount { total: u64::MAX, prompt: u64::MAX, completion: u64::MAX, cached: Some(u64::MAX), purpose: UsagePurpose::Utility, reported: true };
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
    /// usage record's contribution plus the legacy completes, keyed on one
    /// payload field, `(none)` when absent.
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
        for r in legacy_complete_counts(&refs) {
            add(r, &amount_of(payload_of(r)));
        }
        serde_json::to_value(out).unwrap()
    }

    /// The shared golden: this fold and the viewer's `sumUsage` must agree
    /// on every figure `tests/usage-golden/expected.json` pins, the legacy
    /// rule included.
    #[test]
    fn shared_golden_matches_the_viewer() {
        let (records, expected) = golden();
        let idx = fold_all(&records, None);
        let s = &idx.breakdown.overall;
        assert_eq!(shape(s.total, s.prompt, s.completion, s.cached), expected["overall"], "overall");
        assert_eq!(s.usage_records, expected["usage_records"].as_u64().unwrap());
        assert_eq!(s.legacy_completes, expected["legacy_completes_counted"].as_u64().unwrap());
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
        assert_eq!(work.calls + util.calls, s.usage_records + s.legacy_completes, "every counted entry lands in one split");

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

    /// The golden's legacy complete (`crew-dispatch-old-1`, 1000 tokens)
    /// is counted; every complete whose run key holds a usage record is
    /// not, including the count-less `absent` probe step's.
    #[test]
    fn legacy_rule_counts_only_completes_of_runs_with_no_usage_record() {
        let (records, _) = golden();
        let refs: Vec<&serde_json::Value> = records.iter().collect();
        let counted: Vec<&str> =
            legacy_complete_counts(&refs).iter().map(|r| r["session_id"].as_str().unwrap()).collect();
        assert_eq!(counted, vec!["crew-dispatch-old-1"]);
        let idx = fold_all(&records, None);
        assert_eq!(idx.tokens_for_session("crew-dispatch-old-1"), Some(1000));
        // A run with only an `absent` record (task-probe has counted items
        // too) — build one: usage record with no counts, then a complete.
        let only_absent = vec![
            serde_json::json!({"action":"telemetry.tokens","category":"telemetry","source":"tokens","session_id":"s-abs","payload":{"token_source":"absent","call_kind":"turn"}}),
            serde_json::json!({"action":"dispatch complete","session_id":"s-abs","payload":{"total_tokens":500}}),
        ];
        let idx = fold_all(&only_absent, None);
        assert_eq!(idx.breakdown.overall.total, 0, "an absent record silences the complete");
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
        assert_eq!(idx.breakdown.overall.total, 2228);
    }

    /// (review MUST FIX 1) The reviewer's repro: a modern run whose usage
    /// records fall before `since` and whose complete falls after it. The
    /// complete must not be counted as legacy; the run's in-window sum is
    /// simply zero, and it holds no `(none)` group.
    #[test]
    fn since_never_turns_a_modern_run_into_a_legacy_one() {
        let usage = |ts: &str, total: u64| serde_json::json!({"action":"telemetry.tokens","category":"telemetry","source":"tokens","session_id":"S1","ts":ts,"payload":{"call_kind":"turn","purpose":"work","requested_model":"m","endpoint":"http://h/v1","token_source":"provider","total_tokens":total}});
        let records = vec![
            serde_json::json!({"action":"dispatch start","session_id":"S1","ts":"2026-09-26T08:00:00Z"}),
            usage("2026-09-26T08:01:00Z", 120),
            usage("2026-09-26T08:02:00Z", 90),
            usage("2026-09-26T08:03:00Z", 180),
            serde_json::json!({"action":"dispatch complete","session_id":"S1","ts":"2026-09-26T09:30:00Z","payload":{"total_tokens":300}}),
        ];
        let whole = fold_all(&records, None);
        assert_eq!((whole.tokens_for_session("S1"), whole.breakdown.overall.legacy_completes), (Some(390), 0));
        let bounded = fold_all(&records, Some("2026-09-26T09:00:00Z"));
        assert_eq!(bounded.breakdown.overall.total, 0);
        assert_eq!(bounded.breakdown.overall.legacy_completes, 0);
        assert_eq!(bounded.tokens_for_session("S1"), None, "nothing measured in the window");
        assert!(bounded.breakdown.groups.is_empty(), "no (none) group: {:?}", bounded.breakdown.groups);
        // A genuinely legacy run in the same window is still counted.
        let mut with_legacy = records.clone();
        with_legacy.push(serde_json::json!({"action":"dispatch complete","session_id":"S2","ts":"2026-09-26T09:40:00Z","payload":{"total_tokens":50}}));
        let bounded = fold_all(&with_legacy, Some("2026-09-26T09:00:00Z"));
        assert_eq!((bounded.breakdown.overall.total, bounded.breakdown.overall.legacy_completes), (50, 1));
    }

    /// (review CONSIDER 2) Two missions whose steps share a session id (the
    /// scheduler stamps `task-<id>`, which carries no per-run identity):
    /// each mission reads only the keys carrying its own `mission_id` or
    /// no mission at all, never the other mission's.
    #[test]
    fn tokens_for_a_mission_never_reads_another_missions_share_of_a_common_session() {
        let usage = |mid: &str, total: u64| serde_json::json!({"action":"telemetry.tokens","category":"telemetry","source":"tokens","session_id":"task-t1","mission_id":mid,"payload":{"token_source":"provider","total_tokens":total}});
        let records = vec![
            usage("M-A", 100),
            usage("M-B", 1000),
            // A record on the same session naming no mission: reachable by either.
            serde_json::json!({"action":"telemetry.tokens","category":"telemetry","source":"tokens","session_id":"task-t1","payload":{"token_source":"provider","total_tokens":7}}),
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
    /// a legacy record without `purpose` follows the legacy rule.
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
        // 20 (compaction with NO purpose: the legacy rule) = 145; no work.
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

    /// `since` drops records stamped before the bound, inclusive at it, and
    /// the legacy rule is judged on what is INSIDE the window. Fixed
    /// timestamps on both sides: no clock.
    #[test]
    fn since_bounds_the_fold_lexically_and_inclusively() {
        let (records, _) = golden();
        // Everything from the radio dispatch (10:11) on: radio 35 +
        // old-1 complete 1000 + old-2 turn 40 + fixer 80 = 1155.
        let idx = fold_all(&records, Some("2026-09-26T10:11:00Z"));
        assert_eq!(idx.breakdown.overall.total, 1155);
        assert_eq!(idx.breakdown.overall.legacy_completes, 1);
        // One second later the radio usage record (10:11:00) is out, and so
        // is its complete (same second) — the window holds 1120.
        let idx = fold_all(&records, Some("2026-09-26T10:11:01Z"));
        assert_eq!(idx.breakdown.overall.total, 1120);
        // A bound after the old-2 turn but before its complete: the turn is
        // outside the SUM, but the run still HAS a usage record, so its
        // complete (10:16) is never read — a modern run must not turn into
        // a legacy one because the window cut its turns off (review MUST
        // FIX 1). Only the fixer's 80 remain.
        let idx = fold_all(&records, Some("2026-09-26T10:16:00Z"));
        assert_eq!((idx.breakdown.overall.total, idx.breakdown.overall.legacy_completes), (80, 0));
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
