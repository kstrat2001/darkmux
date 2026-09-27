//! (#2917) Is the instance radio would send to busy? Answered from facts,
//! BEFORE the request is sent, so radio answers at once instead of queueing
//! inside LM Studio until the routing/answering call's 300s ceiling.
//!
//! One LM Studio instance serves one request at a time (measured on the
//! issue at `--parallel 1` and `2`). darkmux caps concurrency only within
//! one process (#2772, `config_access::local_dispatch_concurrency`), so a
//! request from ANOTHER process — `darkmux radio` fired while a coder runs
//! — queues inside LM Studio with no visibility. Live, 2026-09-26: the
//! answering seat (`radio-host`) resolved to `default_profile`, the SAME
//! 35B instance a coder was using, queued behind the coder's turn, and
//! failed at 300s with `curl: (28)`. Five minutes of silence on an
//! interactive surface.
//!
//! # Facts, never guesses
//!
//! Busy is claimed from exactly two sources, each a fact something else
//! already holds. Nothing here is inferred from timing.
//!
//! 1. **LM Studio's own report.** `lms ps --json`'s row for the instance
//!    radio would send to: `status` (`LoadedModel::status`) and `queued`
//!    (`LoadedModel::queued`, the number of requests WAITING on it). LM
//!    Studio's vocabulary, read off its CLI's own
//!    `modelProcessingStateSchema = {status, queued}`: `idle`,
//!    `processingPrompt`, `generating`, `computingEmbedding`. Only the three
//!    non-idle words claim busy ([`LM_STUDIO_BUSY_STATUSES`]); an absent or
//!    unrecognized status is NOT a busy claim.
//! 2. **darkmux's own knowledge.** The residency-lease registry
//!    (`darkmux_types::residency_lease`): another live darkmux process that
//!    has CONFIRMED the identifier resident and in use is dispatching to it
//!    — a role execution is many turns, and LM Studio reports the instance
//!    idle between them, so this source catches the gaps the first misses.
//!    Only a lease whose pid is verified to still be its writer counts
//!    (`live_loaded_models_by_process`), so a crash-orphaned lease under a
//!    reused pid never names the wrong process.
//!
//! When either says busy, the OCCUPANT is named from what darkmux knows: a
//! live run on this machine's runs board whose model is that instance
//! (`mission <id> (<role>)`), else the leasing darkmux process by pid, else
//! nothing — the copy then says what was checked and found empty: no live
//! run on it in the last day of darkmux's records, and no darkmux process
//! darkmux can verify holds it. That is a fact about what was read, not a
//! claim about whose work it is (a compaction, for one, is darkmux's work
//! that no ledger records), nor that no darkmux run is using it: a run live
//! longer than the window can have its model outside it.
//!
//! # Two seats, two policies
//!
//! - The **answering seat** never queues silently: [`BusyReport::answering_seat_message`]
//!   is the answer, on both surfaces (`radio_cli.rs` prints it; `acp.rs`
//!   sends it as the reply chunk). The check runs just before the send, so
//!   work that starts in between still queues it (a check-then-send race,
//!   stated in the guide).
//! - The **router** waits, by the operator's decision on #2914 (option A:
//!   a routing call behind a compaction on the one utility instance is
//!   accepted). What this module adds there is the WORD: after
//!   `radio::router_slow_notice_after()` the surfaces say what LM Studio
//!   reports ([`router_wait_notice_live`]) and keep waiting to the ceiling.
//!   The notice is read WHILE radio's own routing call is in flight, so a
//!   non-idle utility instance is expected to include that call: it is not
//!   by itself "something ahead of you". `queued > 0` (a request waiting on
//!   the instance; `queued` excludes the one being served) says the call is
//!   sharing the instance with other work. A verified foreign lease on it
//!   says another darkmux process has it LOADED, not that a request of its
//!   is in flight: it is named as context, and says "may be sharing" only
//!   when LM Studio's queue count is not available.
//!
//! # Cost
//!
//! The idle path: one bounded `lms ps --json` plus a directory scan of
//! `<darkmux-home>/residency/`. The runs board is read ONLY once the
//! answering seat is established busy, and then narrowed to the most recent
//! day files ([`LIVE_RUN_WINDOW_DAYS`]): a run that is live now has records
//! from today. The router notice never reads it.

use crate::radio_answer::AnswererOverrides;
use darkmux_types::LoadedModel;
use std::time::Duration;

pub use crate::crew::dispatch::LocalTarget;

/// LM Studio's non-idle processing statuses, verbatim from its CLI
/// (`modelProcessingStateSchema`, minus `idle`). Matched case-insensitively
/// in case a future `lms` changes the casing; anything else is not a claim.
pub const LM_STUDIO_BUSY_STATUSES: [&str; 3] = ["processingPrompt", "generating", "computingEmbedding"];

/// How many day files back the busy path reads the runs board: today and
/// yesterday (UTC), so a run that crossed midnight is still seen. A run
/// that is live NOW writes records now; an older window only costs time
/// (the full 14-day board read measured seconds on a real archive). A run
/// whose identifying records fell outside the window is not named, which
/// says less, never something wrong.
pub const LIVE_RUN_WINDOW_DAYS: i64 = 1;

/// LM Studio's status word in plain English, or `None` for a word this
/// reader does not know.
pub fn plain_status(status: &str) -> Option<&'static str> {
    const WORDS: [(&str, &str); 4] = [
        ("idle", "idle"),
        ("processingPrompt", "reading a prompt"),
        ("generating", "generating a reply"),
        ("computingEmbedding", "computing embeddings"),
    ];
    WORDS.iter().find(|(raw, _)| raw.eq_ignore_ascii_case(status)).map(|(_, plain)| *plain)
}

fn is_busy_status(status: &str) -> bool {
    LM_STUDIO_BUSY_STATUSES.iter().any(|busy| busy.eq_ignore_ascii_case(status))
}

/// Which fact says the instance is busy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BusySource {
    /// `lms ps` reports the instance in this non-idle status (verbatim).
    LmStudioStatus(String),
    /// A live darkmux process (`pid`) holds the instance loaded and in use;
    /// LM Studio reported it idle at this instant (between turns).
    DarkmuxLease { pid: u32 },
}

/// What is occupying the instance, as far as darkmux knows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Occupant {
    /// A live run on this machine's runs board whose model is the instance.
    Run { kind: &'static str, id: String, role: Option<String> },
    /// A darkmux process holds the instance but no live run names it (its
    /// records may not have reached the flow file yet).
    DarkmuxProcess { pid: u32 },
    /// No live darkmux run in the last [`LIVE_RUN_WINDOW_DAYS`] of records
    /// and no verified darkmux lease names the instance. A fact about what
    /// was read only: it is NOT a claim that the work is not darkmux's (a
    /// compaction is darkmux work that no ledger records), nor that no
    /// darkmux run uses it (a run live longer than the window can have its
    /// model outside it).
    NoneOnRecord,
}

/// The busy verdict for one instance: the instance, the fact that says
/// busy, and who occupies it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BusyReport {
    pub identifier: String,
    pub source: BusySource,
    pub occupant: Occupant,
}

/// A live run as the busy check reads it — the four fields of
/// `darkmux_serve::Run` it needs, so the pure core is buildable in a test
/// without a flow file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LiveRun {
    pub kind: &'static str,
    pub id: String,
    pub role: Option<String>,
    pub model: Option<String>,
}

/// What LM Studio's listing says about ONE instance. Three different
/// states that an earlier revision collapsed into "idle": the instance
/// listed (with whatever status and queue it reports), not listed at all
/// (unloaded, or still loading), and the listing itself unavailable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InstanceReading {
    Listed { status: String, queued: Option<u64> },
    NotListed,
    ReadFailed,
}

/// The listing's row for `target`, as an [`InstanceReading`]. `loaded` is
/// `None` when `lms ps` could not be read.
pub fn read_instance(target: &LocalTarget, loaded: Option<&[LoadedModel]>) -> InstanceReading {
    let Some(loaded) = loaded else {
        return InstanceReading::ReadFailed;
    };
    match loaded.iter().find(|m| m.identifier == target.identifier) {
        Some(m) => InstanceReading::Listed { status: m.status.clone(), queued: m.queued },
        None => InstanceReading::NotListed,
    }
}

/// The pure core. `loaded` is `lms ps`; `leases` is
/// `residency_lease::live_loaded_models_by_process` (other processes' pid +
/// the identifiers each has confirmed in use); `live_runs` is called ONLY
/// when busy is already established (it is the expensive read).
pub fn busy_report(
    target: &LocalTarget,
    loaded: &[LoadedModel],
    leases: &[(u32, Vec<String>)],
    live_runs: &dyn Fn() -> Vec<LiveRun>,
) -> Option<BusyReport> {
    let lms_status = loaded
        .iter()
        .find(|m| m.identifier == target.identifier)
        .map(|m| m.status.as_str())
        .filter(|s| is_busy_status(s));
    let lease_pid = lease_holder(target, leases);
    let source = match (lms_status, lease_pid) {
        (Some(status), _) => BusySource::LmStudioStatus(status.to_string()),
        (None, Some(pid)) => BusySource::DarkmuxLease { pid },
        (None, None) => return None,
    };
    let occupant = live_runs()
        .into_iter()
        .find(|r| r.model.as_deref().is_some_and(|m| target.matches_recorded_model(m)))
        .map(|r| Occupant::Run { kind: r.kind, id: r.id, role: r.role })
        .or(lease_pid.map(|pid| Occupant::DarkmuxProcess { pid }))
        .unwrap_or(Occupant::NoneOnRecord);
    Some(BusyReport { identifier: target.identifier.clone(), source, occupant })
}

/// The first other process whose verified lease has `target` loaded.
fn lease_holder(target: &LocalTarget, leases: &[(u32, Vec<String>)]) -> Option<u32> {
    leases.iter().find(|(_, models)| models.iter().any(|m| m == &target.identifier)).map(|(pid, _)| *pid)
}

impl BusyReport {
    /// "LM Studio reports it generating a reply" / "darkmux process 123 is
    /// dispatching to it".
    fn fact_clause(&self) -> String {
        match &self.source {
            BusySource::LmStudioStatus(status) => match plain_status(status) {
                Some(plain) => format!("LM Studio reports it {plain}"),
                None => format!("LM Studio reports it `{status}`"),
            },
            BusySource::DarkmuxLease { pid } => format!("darkmux process {pid} is dispatching to it"),
        }
    }

    /// Who, as a clause that follows [`Self::fact_clause`].
    fn occupant_clause(&self) -> String {
        match &self.occupant {
            Occupant::Run { kind, id, role } => match role {
                Some(role) => format!(" for {kind} `{id}` ({role})"),
                None => format!(" for {kind} `{id}`"),
            },
            Occupant::DarkmuxProcess { pid } => match &self.source {
                // The pid is already in the fact clause; do not say it twice.
                BusySource::DarkmuxLease { .. } => String::new(),
                BusySource::LmStudioStatus(_) => format!(" for darkmux process {pid}"),
            },
            // (#2917 re-review C-3) Say what was checked: the runs board is
            // read over the last day only (`LIVE_RUN_WINDOW_DAYS`), so a run
            // live longer than that can be missing its model there, and
            // only a VERIFIED lease names a process.
            Occupant::NoneOnRecord => "; darkmux found no live run on it in the last day of its records, and no \
                                       darkmux process it can verify holds it"
                .to_string(),
        }
    }

    /// The answering seat's reply — user-facing copy, the same on the CLI
    /// and in the editor panel. States the instance, the fact, the
    /// occupant, that radio did not queue, and the two ways out.
    pub fn answering_seat_message(&self) -> String {
        format!(
            "the model radio would answer with, `{}`, is busy: {}{}. Radio did not queue behind it. \
             Ask again when that finishes, or point `radio.answerer_profile` at a profile whose model is free.",
            self.identifier,
            self.fact_clause(),
            self.occupant_clause()
        )
    }
}

/// The facts the router notice speaks from, for the utility instance.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UtilityFacts {
    pub target: LocalTarget,
    pub reading: InstanceReading,
    /// Another darkmux process whose VERIFIED lease has the utility
    /// instance loaded and in use, if any.
    pub foreign_holder: Option<u32>,
}

/// The router's "still waiting" notice, after `waited` with no decision
/// yet. Pure over its inputs; [`router_wait_notice_live`] gathers them.
///
/// Read while radio's OWN routing call is in flight on the utility
/// instance, so a non-idle status there is expected to include that call.
/// "Sharing it with other work" is said only from `queued > 0`, or "may be
/// sharing" from a verified foreign lease when the queue count is unknown;
/// every other case says exactly what LM Studio reported, and what that
/// does and does not tell, with a foreign holder added as context.
pub fn router_wait_notice(facts: Option<&UtilityFacts>, waited: Duration, ceiling: Duration) -> String {
    let waited = waited.as_secs();
    let ceiling = ceiling.as_secs();
    let tail = format!("Routing keeps waiting, up to {ceiling}s.");
    let Some(facts) = facts else {
        return format!("still routing after {waited}s (no utility model resolved to check). {tail}");
    };
    let id = &facts.target.identifier;
    // What the queue says about other work: `Some(true)` requests are
    // waiting, `Some(false)` none are (or the instance is idle), `None`
    // darkmux cannot tell.
    let (body, others_waiting): (String, Option<bool>) = match &facts.reading {
        InstanceReading::ReadFailed => (
            "darkmux could not read LM Studio's model list (`lms ps`), so it cannot say what is ahead of this call"
                .to_string(),
            None,
        ),
        InstanceReading::NotListed => (
            format!("LM Studio does not list the utility model `{id}` as loaded, so darkmux cannot read what it is doing"),
            None,
        ),
        // `queued > 0` is the rule for "sharing", and `queued` EXCLUDES the
        // request being served: measured 2026-09-26 against a real LM Studio
        // (operator-attended), one request in flight reads `queued: 0` and
        // two read `queued: 1` while the second waits. So radio's own routing
        // call, the one being served, never counts itself as waiting, and
        // `queued: 0` with a busy status is that call alone.
        InstanceReading::Listed { status, queued } => match (plain_status(status), queued) {
            (Some(plain), Some(n)) if *n > 0 => (
                format!(
                    "LM Studio reports the utility model `{id}` {plain} with {n} {} waiting, so this call is sharing \
                     it with other work (for example a compaction)",
                    if *n == 1 { "request" } else { "requests" }
                ),
                Some(true),
            ),
            (Some(plain), queued) if is_busy_status(status) => match queued {
                Some(_) => (
                    format!(
                        "LM Studio reports the utility model `{id}` {plain} with nothing waiting, so no other request \
                         is ahead of this call"
                    ),
                    Some(false),
                ),
                None => (
                    format!(
                        "LM Studio reports the utility model `{id}` {plain}, but not how many requests are waiting, \
                         so darkmux cannot tell whether that is this call or other work (for example a compaction)"
                    ),
                    None,
                ),
            },
            (Some(plain), _) => (
                format!(
                    "LM Studio reports the utility model `{id}` {plain}, so no other request is being served ahead \
                     of this call"
                ),
                Some(false),
            ),
            (None, _) if status.is_empty() => (
                format!(
                    "LM Studio lists the utility model `{id}` without a status, so darkmux cannot say what is ahead \
                     of this call"
                ),
                None,
            ),
            (None, _) => (
                format!(
                    "LM Studio reports the utility model `{id}` as `{status}`, which darkmux does not recognize, so it \
                     cannot say what is ahead of this call"
                ),
                None,
            ),
        },
    };
    // (#2917 re-review C-2) A verified foreign lease means that process has
    // the model LOADED for a role execution, not that a request of its is
    // in flight. It never overrides LM Studio's queue: it is context when
    // the queue is known, and the only fact about other work when it is not.
    let body = match (facts.foreign_holder, others_waiting) {
        (None, _) => body,
        (Some(pid), Some(true)) => format!("{body}; darkmux process {pid} also has it in use"),
        (Some(pid), Some(false)) => {
            format!("{body}; darkmux process {pid} also has it loaded, which by itself puts no request ahead of this call")
        }
        (Some(pid), None) => {
            format!("{body}; darkmux process {pid} also has it in use, so this call may be sharing it with that process's work")
        }
    };
    format!("still routing after {waited}s: {body}. {tail}")
}

// ── Live gathering ────────────────────────────────────────────────────────

/// The facts, gathered live, for `target`. `lms ps` failing is NOT a busy
/// claim (the dispatch that follows surfaces that error itself); the lease
/// registry is read the way every other reader reads it (lenient,
/// crash-orphans swept) and only verified leases count.
pub fn busy_report_live(target: &LocalTarget) -> Option<BusyReport> {
    let loaded = crate::lms::list_loaded().unwrap_or_default();
    let leases = darkmux_types::residency_lease::live_loaded_models_by_process(std::process::id());
    busy_report(target, &loaded, &leases, &live_runs_from_local_records)
}

/// The answering seat's busy verdict under `overrides` — `None` when the
/// seat targets no local instance (a hosted endpoint queues on the
/// provider's side, not on this machine's one instance) or is free.
pub fn answering_seat_busy(overrides: &AnswererOverrides) -> Option<BusyReport> {
    crate::radio_answer::answering_seat_target(overrides).and_then(|target| busy_report_live(&target))
}

/// The utility instance's facts, gathered live: `lms ps` (a failure is
/// carried as [`InstanceReading::ReadFailed`], never read as idle) and the
/// verified leases of OTHER processes (this process's own pid is excluded:
/// radio's routing call is its own work). `None` when no utility model
/// resolves.
pub fn utility_facts_live() -> Option<UtilityFacts> {
    let target = crate::crew::dispatch::utility_local_target(None)?;
    let loaded = crate::lms::list_loaded().ok();
    let reading = read_instance(&target, loaded.as_deref());
    let leases = darkmux_types::residency_lease::live_loaded_models_by_process(std::process::id());
    let foreign_holder = lease_holder(&target, &leases);
    Some(UtilityFacts { target, reading, foreign_holder })
}

/// The router's notice with live facts about the utility instance.
pub fn router_wait_notice_live(waited: Duration) -> String {
    router_wait_notice(
        utility_facts_live().as_ref(),
        waited,
        Duration::from_secs(u64::from(crate::radio::ROUTER_CALL_CEILING_SECONDS)),
    )
}

/// This machine's live runs, from the SAME local records `darkmux run list`
/// reads (`darkmux_serve::build_runs_within` over the flows + lab dirs),
/// narrowed to [`LIVE_RUN_WINDOW_DAYS`] and local only — no fleet read,
/// because the instance in question is on this machine. Called only once
/// busy is established.
fn live_runs_from_local_records() -> Vec<LiveRun> {
    let flows_dir = darkmux_types::config_access::flows_dir();
    let lab_dir = darkmux_types::config_access::lab_dir();
    darkmux_serve::build_runs_within(&flows_dir, Some(&lab_dir), &[], LIVE_RUN_WINDOW_DAYS)
        .into_iter()
        .filter(|r| r.status == darkmux_serve::RunStatus::Running)
        .map(|r| LiveRun {
            kind: match r.kind {
                darkmux_serve::RunKind::Mission => "mission",
                darkmux_serve::RunKind::Dispatch => "dispatch",
                darkmux_serve::RunKind::Lab => "lab run",
            },
            id: r.id,
            role: r.role,
            model: r.model,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn target() -> LocalTarget {
        LocalTarget { identifier: "darkmux:qwen3.6-35b-a3b".to_string(), model_key: "qwen3.6-35b-a3b".to_string() }
    }

    fn util() -> LocalTarget {
        LocalTarget { identifier: "darkmux:qwen3-4b".to_string(), model_key: "qwen3-4b".to_string() }
    }

    fn resident(identifier: &str, status: &str) -> LoadedModel {
        resident_q(identifier, status, None)
    }

    fn resident_q(identifier: &str, status: &str, queued: Option<u64>) -> LoadedModel {
        LoadedModel {
            identifier: identifier.to_string(),
            model: identifier.trim_start_matches("darkmux:").to_string(),
            status: status.to_string(),
            size: "20.00 GB".to_string(),
            context: 100_000,
            queued,
        }
    }

    fn no_runs() -> Vec<LiveRun> {
        Vec::new()
    }

    fn runs_must_not_be_read() -> Vec<LiveRun> {
        panic!("the runs board is the expensive read: it must not be touched on the idle path (#2917)")
    }

    const W: Duration = Duration::from_secs(10);
    const C: Duration = Duration::from_secs(300);

    fn notice(reading: InstanceReading, foreign_holder: Option<u32>) -> String {
        router_wait_notice(Some(&UtilityFacts { target: util(), reading, foreign_holder }), W, C)
    }

    fn listed(status: &str, queued: Option<u64>) -> InstanceReading {
        InstanceReading::Listed { status: status.to_string(), queued }
    }

    #[test]
    fn an_idle_instance_is_not_busy_and_the_runs_board_is_never_read() {
        let loaded = [resident("darkmux:qwen3.6-35b-a3b", "idle")];
        assert_eq!(busy_report(&target(), &loaded, &[], &runs_must_not_be_read), None);
    }

    #[test]
    fn an_absent_or_unknown_status_is_not_a_busy_claim() {
        // An older `lms` with no field, and a word this reader does not
        // know (`loaded` is the shape one fixture in tests/cli.rs uses):
        // neither says anything about the instance.
        for status in ["", "loaded", "ready"] {
            let loaded = [resident("darkmux:qwen3.6-35b-a3b", status)];
            assert_eq!(busy_report(&target(), &loaded, &[], &no_runs), None, "status {status:?}");
        }
    }

    #[test]
    fn a_status_on_some_other_instance_says_nothing_about_this_one() {
        let loaded = [resident("darkmux:qwen3-4b", "generating"), resident("qwen3.6-35b-a3b", "generating")];
        // The user-loaded copy of the SAME model key is a different
        // instance (#2240): radio sends to the namespaced one.
        assert_eq!(busy_report(&target(), &loaded, &[], &no_runs), None);
    }

    #[test]
    fn every_non_idle_lm_studio_status_is_busy_case_insensitively() {
        for status in ["generating", "processingPrompt", "computingEmbedding", "GENERATING"] {
            let loaded = [resident("darkmux:qwen3.6-35b-a3b", status)];
            let report = busy_report(&target(), &loaded, &[], &no_runs).unwrap_or_else(|| panic!("{status}"));
            assert_eq!(report.source, BusySource::LmStudioStatus(status.to_string()));
            assert_eq!(report.occupant, Occupant::NoneOnRecord);
        }
    }

    /// (#2917 review C6) The user reads plain words, not LM Studio's
    /// camelCase schema values.
    #[test]
    fn lm_studio_status_words_render_as_plain_english() {
        for (raw, plain) in [
            ("processingPrompt", "LM Studio reports it reading a prompt"),
            ("generating", "LM Studio reports it generating a reply"),
            ("computingEmbedding", "LM Studio reports it computing embeddings"),
        ] {
            let loaded = [resident("darkmux:qwen3.6-35b-a3b", raw)];
            let msg = busy_report(&target(), &loaded, &[], &no_runs).expect("busy").answering_seat_message();
            assert!(msg.contains(plain), "{msg}");
            if raw != "generating" {
                assert!(!msg.contains(raw), "the raw schema word must not reach the user: {msg}");
            }
        }
    }

    #[test]
    fn lm_studio_busy_with_a_live_run_on_the_instance_names_the_run() {
        let loaded = [resident("darkmux:qwen3.6-35b-a3b", "generating")];
        let runs = || {
            vec![
                LiveRun {
                    kind: "mission",
                    id: "other-mission".to_string(),
                    role: Some("reviewer".to_string()),
                    model: Some("darkmux:qwen3-4b".to_string()),
                },
                LiveRun {
                    kind: "mission",
                    id: "pepper-refresh-rotation".to_string(),
                    role: Some("coder".to_string()),
                    model: Some("darkmux:qwen3.6-35b-a3b".to_string()),
                },
            ]
        };
        let report = busy_report(&target(), &loaded, &[], &runs).expect("busy");
        assert_eq!(
            report.occupant,
            Occupant::Run { kind: "mission", id: "pepper-refresh-rotation".to_string(), role: Some("coder".to_string()) }
        );
        let msg = report.answering_seat_message();
        assert!(msg.contains("`darkmux:qwen3.6-35b-a3b`, is busy: LM Studio reports it generating a reply for mission `pepper-refresh-rotation` (coder)."), "{msg}");
        assert!(msg.contains("Radio did not queue behind it."), "{msg}");
    }

    #[test]
    fn a_run_recorded_under_the_bare_model_key_still_matches_the_instance() {
        // Older archives record the bare key (#2240 put the identifier on
        // the wire); a bare key names the namespaced instance.
        let loaded = [resident("darkmux:qwen3.6-35b-a3b", "generating")];
        let runs = || vec![LiveRun { kind: "dispatch", id: "d-1".to_string(), role: None, model: Some("qwen3.6-35b-a3b".to_string()) }];
        let report = busy_report(&target(), &loaded, &[], &runs).expect("busy");
        assert_eq!(report.occupant, Occupant::Run { kind: "dispatch", id: "d-1".to_string(), role: None });
        assert!(report.answering_seat_message().contains("for dispatch `d-1`."), "{}", report.answering_seat_message());
    }

    /// (#2917 review C7) A seat whose profile sets a custom `identifier`
    /// is a different instance from `darkmux:<key>`: a run recorded on the
    /// namespaced copy must not be named as its occupant.
    #[test]
    fn a_run_on_the_namespaced_copy_is_not_named_for_a_custom_identifier_seat() {
        let aliased = LocalTarget { identifier: "my-alias".to_string(), model_key: "qwen3.6-35b-a3b".to_string() };
        let loaded = [resident("my-alias", "generating")];
        let runs = || {
            vec![LiveRun {
                kind: "mission",
                id: "on-the-other-copy".to_string(),
                role: None,
                model: Some("darkmux:qwen3.6-35b-a3b".to_string()),
            }]
        };
        let report = busy_report(&aliased, &loaded, &[], &runs).expect("busy");
        assert_eq!(report.occupant, Occupant::NoneOnRecord);
    }

    #[test]
    fn lm_studio_idle_but_a_live_darkmux_lease_holds_the_instance_is_busy_by_lease() {
        // Between turns LM Studio reports idle; the lease says a darkmux
        // process is mid role-execution on it.
        let loaded = [resident("darkmux:qwen3.6-35b-a3b", "idle")];
        let leases = [(4242u32, vec!["darkmux:qwen3.6-35b-a3b".to_string()])];
        let report = busy_report(&target(), &loaded, &leases, &no_runs).expect("busy by lease");
        assert_eq!(report.source, BusySource::DarkmuxLease { pid: 4242 });
        assert_eq!(report.occupant, Occupant::DarkmuxProcess { pid: 4242 });
        let msg = report.answering_seat_message();
        assert!(msg.contains("is busy: darkmux process 4242 is dispatching to it."), "the pid is said once: {msg}");
    }

    #[test]
    fn a_lease_on_a_different_identifier_does_not_make_this_instance_busy() {
        let loaded = [resident("darkmux:qwen3.6-35b-a3b", "idle")];
        let leases = [(4242u32, vec!["darkmux:qwen3-4b".to_string()])];
        assert_eq!(busy_report(&target(), &loaded, &leases, &runs_must_not_be_read), None);
    }

    #[test]
    fn lm_studio_busy_and_a_lease_with_no_run_names_the_leasing_process() {
        let loaded = [resident("darkmux:qwen3.6-35b-a3b", "processingPrompt")];
        let leases = [(77u32, vec!["darkmux:qwen3.6-35b-a3b".to_string()])];
        let report = busy_report(&target(), &loaded, &leases, &no_runs).expect("busy");
        assert_eq!(report.source, BusySource::LmStudioStatus("processingPrompt".to_string()));
        assert_eq!(report.occupant, Occupant::DarkmuxProcess { pid: 77 });
        assert!(
            report.answering_seat_message().contains("LM Studio reports it reading a prompt for darkmux process 77."),
            "{}",
            report.answering_seat_message()
        );
    }

    /// Nothing of darkmux's on record is a fact about darkmux's RECORDS:
    /// the copy says exactly that, and never that the work is not
    /// darkmux's (a compaction is darkmux work no ledger records).
    #[test]
    fn with_nothing_on_record_the_busy_message_says_so_without_claiming_whose_work_it_is() {
        let loaded = [resident("darkmux:qwen3.6-35b-a3b", "generating")];
        let msg = busy_report(&target(), &loaded, &[], &no_runs).expect("busy").answering_seat_message();
        assert!(
            msg.contains(
                "LM Studio reports it generating a reply; darkmux found no live run on it in the last day of its \
                 records, and no darkmux process it can verify holds it."
            ),
            "{msg}"
        );
        assert!(!msg.contains("did not start"), "{msg}");
        assert!(msg.contains("point `radio.answerer_profile` at a profile"), "names a way out, at a PROFILE: {msg}");
    }

    // ── read_instance (#2917 review M2) ──────────────────────────────────

    #[test]
    fn read_instance_keeps_listed_not_listed_and_read_failed_apart() {
        let loaded = [resident_q("darkmux:qwen3-4b", "generating", Some(1))];
        assert_eq!(read_instance(&util(), Some(&loaded)), listed("generating", Some(1)));
        assert_eq!(read_instance(&util(), Some(&[])), InstanceReading::NotListed);
        assert_eq!(
            read_instance(&util(), Some(&[resident("qwen3-4b", "idle")])),
            InstanceReading::NotListed,
            "a user-loaded copy of the same key is not the instance"
        );
        assert_eq!(read_instance(&util(), None), InstanceReading::ReadFailed);
    }

    // ── router_wait_notice (#2917 review M1, M2) ─────────────────────────

    /// M1, cases A and G of the review: only radio's OWN routing call on
    /// the utility instance (it reports `generating`, nothing queued) —
    /// with or without a live lease on some OTHER model — must not read
    /// as "something ahead of you", and never as work darkmux did not start.
    #[test]
    fn the_router_notice_does_not_call_its_own_routing_call_other_work() {
        let n = notice(listed("generating", Some(0)), None);
        assert_eq!(
            n,
            "still routing after 10s: LM Studio reports the utility model `darkmux:qwen3-4b` generating a reply with \
             nothing waiting, so no other request is ahead of this call. Routing keeps waiting, up to 300s."
        );
        assert!(!n.contains("did not start") && !n.contains("sharing"), "{n}");
    }

    /// M1: a request WAITING on the utility instance (`queued > 0`) is the
    /// fact that says the call shares it with other work — worded as
    /// darkmux work it cannot attribute (a compaction), never as someone
    /// else's.
    #[test]
    fn a_queue_on_the_utility_instance_says_the_call_is_sharing_it() {
        let n = notice(listed("generating", Some(1)), None);
        assert_eq!(
            n,
            "still routing after 10s: LM Studio reports the utility model `darkmux:qwen3-4b` generating a reply with \
             1 request waiting, so this call is sharing it with other work (for example a compaction). Routing keeps \
             waiting, up to 300s."
        );
        assert!(notice(listed("processingPrompt", Some(3)), None).contains("with 3 requests waiting"));
        assert!(!n.contains("did not start"), "{n}");
    }

    #[test]
    fn an_older_lms_with_no_queue_field_says_it_cannot_tell() {
        let n = notice(listed("generating", None), None);
        assert!(n.contains("but not how many requests are waiting, so darkmux cannot tell"), "{n}");
        assert!(!n.contains("sharing") && !n.contains("no other request is ahead"), "{n}");
    }

    /// (#2917 re-review C-2) A verified foreign lease means that process
    /// has the model LOADED for a role execution, not that a request of
    /// its is in flight. With LM Studio reporting nothing waiting, its
    /// reading stands and the holder is context, never "sharing".
    #[test]
    fn a_foreign_holder_with_nothing_waiting_keeps_lm_studios_reading() {
        let n = notice(listed("generating", Some(0)), Some(4242));
        assert_eq!(
            n,
            "still routing after 10s: LM Studio reports the utility model `darkmux:qwen3-4b` generating a reply with \
             nothing waiting, so no other request is ahead of this call; darkmux process 4242 also has it loaded, \
             which by itself puts no request ahead of this call. Routing keeps waiting, up to 300s."
        );
        let idle = notice(listed("idle", Some(0)), Some(4242));
        assert!(idle.contains("idle, so no other request is being served ahead of this call; darkmux process 4242 also has it loaded"), "{idle}");
        for n in [n, idle] {
            assert!(!n.contains("sharing"), "{n}");
        }
    }

    /// C-2: `queued > 0` is what says sharing; the holder is named too.
    #[test]
    fn a_foreign_holder_with_requests_waiting_says_sharing_and_names_the_holder() {
        let n = notice(listed("generating", Some(2)), Some(4242));
        assert!(n.contains("with 2 requests waiting, so this call is sharing it with other work"), "{n}");
        assert!(n.contains("darkmux process 4242 also has it in use"), "{n}");
    }

    /// C-2: with the queue unknown (an older `lms`, the model not listed,
    /// the listing unreadable, a status darkmux does not know), a foreign
    /// holder is the only fact about other work, and it says the call may
    /// be sharing the instance.
    #[test]
    fn a_foreign_holder_with_the_queue_unknown_says_the_call_may_be_sharing_it() {
        for reading in [
            listed("generating", None),
            InstanceReading::NotListed,
            InstanceReading::ReadFailed,
            listed("warming", Some(0)),
            listed("", None),
        ] {
            let n = notice(reading.clone(), Some(4242));
            assert!(
                n.contains("; darkmux process 4242 also has it in use, so this call may be sharing it with that process's work."),
                "{reading:?}: {n}"
            );
        }
    }

    #[test]
    fn an_idle_utility_instance_says_nothing_is_being_served_ahead() {
        for q in [Some(0), None] {
            let n = notice(listed("idle", q), None);
            assert!(n.contains("reports the utility model `darkmux:qwen3-4b` idle, so no other request"), "{n}");
        }
    }

    /// M2: not listed, unreadable listing, unknown or missing status each
    /// get their own honest wording — never "reports idle".
    #[test]
    fn not_listed_read_failed_and_unknown_status_never_read_as_idle() {
        let cases = [
            (InstanceReading::NotListed, "does not list the utility model `darkmux:qwen3-4b` as loaded"),
            (InstanceReading::ReadFailed, "could not read LM Studio's model list"),
            (listed("warming", Some(0)), "as `warming`, which darkmux does not recognize"),
            (listed("", None), "without a status"),
        ];
        for (reading, expect) in cases {
            let n = notice(reading.clone(), None);
            assert!(n.contains(expect), "{reading:?}: {n}");
            assert!(!n.contains("idle") && !n.contains("nothing is queued") && !n.contains("no other request"), "{reading:?}: {n}");
        }
        let none = router_wait_notice(None, W, C);
        assert!(none.contains("no utility model resolved to check"), "{none}");
    }

    // ── live gathering: this process's own lease (#2917 review C3) ───────

    struct EnvGuard {
        key: &'static str,
        prev: Option<String>,
    }

    impl EnvGuard {
        fn set(key: &'static str, value: &str) -> Self {
            let prev = std::env::var(key).ok();
            // SAFETY: caller's test is #[serial_test::serial].
            unsafe { std::env::set_var(key, value) };
            Self { key, prev }
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            // SAFETY: caller's test is #[serial_test::serial].
            unsafe {
                match &self.prev {
                    Some(v) => std::env::set_var(self.key, v),
                    None => std::env::remove_var(self.key),
                }
            }
        }
    }

    /// Radio's own work is never "something else": a lease THIS process
    /// holds (the `darkmux acp` host dispatching on the same instance, or
    /// the routing call's own residency) must not make either seat's
    /// instance read as held by another darkmux process. `lms` is pointed
    /// at a path that does not exist, so the listing is unreadable and the
    /// lease is the only fact in play.
    #[test]
    #[serial_test::serial]
    fn this_processs_own_lease_never_reads_as_a_foreign_holder() {
        let home = tempfile::TempDir::new().unwrap();
        let profiles = home.path().join("profiles.json");
        std::fs::write(
            &profiles,
            r#"{"profiles":{"work":{"models":[{"id":"stub-worker","n_ctx":8000}]}},
                "default_profile":"work","internal":{"utility":{"id":"stub-util","n_ctx":8000}}}"#,
        )
        .unwrap();
        let _home = EnvGuard::set("DARKMUX_HOME", home.path().to_str().unwrap());
        let _profiles = EnvGuard::set("DARKMUX_PROFILES", profiles.to_str().unwrap());
        let _lms = EnvGuard::set("DARKMUX_LMS_BIN", home.path().join("no-such-lms").to_str().unwrap());

        let guard = darkmux_types::residency_lease::LeaseGuard::acquire();
        let held = vec!["darkmux:stub-util".to_string(), "darkmux:stub-worker".to_string()];
        guard.write(&held).unwrap();
        guard.mark_loaded(&held).unwrap();

        let facts = utility_facts_live().expect("the utility binding resolves");
        assert_eq!(facts.target.identifier, "darkmux:stub-util");
        assert_eq!(facts.reading, InstanceReading::ReadFailed, "an unreadable listing is carried as such");
        assert_eq!(facts.foreign_holder, None, "radio's own process is not a foreign holder");

        let worker = LocalTarget { identifier: "darkmux:stub-worker".to_string(), model_key: "stub-worker".to_string() };
        assert_eq!(busy_report_live(&worker), None, "radio's own lease does not make its answering seat busy");
    }
}
