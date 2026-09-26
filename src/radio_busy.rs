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
//! 1. **LM Studio's own report.** `lms ps --json`'s `status` on the row for
//!    the instance radio would send to — the same field
//!    `darkmux_profiles::lms::list_loaded` has always parsed
//!    (`LoadedModel::status`). LM Studio's vocabulary, read off its CLI's
//!    own `modelProcessingStatusSchema`: `idle`, `processingPrompt`,
//!    `generating`, `computingEmbedding`. Only the three non-idle words
//!    claim busy ([`LM_STUDIO_BUSY_STATUSES`]); an absent or unrecognized
//!    status is NOT a busy claim — an older `lms` with no field, or a word
//!    this reader does not know, says nothing about the instance.
//! 2. **darkmux's own knowledge.** The residency-lease registry
//!    (`darkmux_types::residency_lease`): another live darkmux process that
//!    has CONFIRMED the identifier resident and in use is dispatching to it
//!    — a role execution is many turns, and LM Studio reports the instance
//!    idle between them, so this source catches the gaps the first misses.
//!
//! When either says busy, the OCCUPANT is named from what darkmux knows: a
//! live run on this machine's runs board whose model is that instance
//! (`mission <id> (<role>)`), else the leasing darkmux process by pid, else
//! — LM Studio busy with nothing of darkmux's in flight on it — work
//! darkmux did not start.
//!
//! # Two seats, two policies
//!
//! - The **answering seat** never queues: [`BusyReport::answering_seat_message`]
//!   is the answer, on both surfaces (`radio_cli.rs` prints it; `acp.rs`
//!   sends it as the reply chunk).
//! - The **router** waits, by the operator's decision on #2914 (option A:
//!   a routing call behind a compaction on the one utility instance is
//!   accepted). What this module adds there is the WORD: after
//!   `radio::ROUTER_SLOW_NOTICE_AFTER` the surfaces say what is happening
//!   ([`router_wait_notice_live`]) and keep waiting to the ceiling.
//!
//! # Cost on the idle path
//!
//! One bounded `lms ps --json` (the same call the residency preflight makes
//! a moment later) plus a directory scan of `<darkmux-home>/residency/`.
//! The runs board (a flow-file parse) is read ONLY once busy is
//! established, never on the idle path. Measured before/after on the PR.

use crate::radio_answer::AnswererOverrides;
use darkmux_types::LoadedModel;
use std::time::Duration;

pub use crate::crew::dispatch::LocalTarget;

/// LM Studio's non-idle processing statuses, verbatim from its CLI
/// (`modelProcessingStatusSchema`, minus `idle`). Matched case-insensitively
/// in case a future `lms` changes the casing; anything else is not a claim.
pub const LM_STUDIO_BUSY_STATUSES: [&str; 3] = ["processingPrompt", "generating", "computingEmbedding"];

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
    /// Nothing of darkmux's is on the instance: the work is not darkmux's.
    NotDarkmux,
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
        .filter(|s| LM_STUDIO_BUSY_STATUSES.iter().any(|busy| busy.eq_ignore_ascii_case(s)));
    let lease_pid = leases
        .iter()
        .find(|(_, models)| models.iter().any(|m| m == &target.identifier))
        .map(|(pid, _)| *pid);
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
        .unwrap_or(Occupant::NotDarkmux);
    Some(BusyReport { identifier: target.identifier.clone(), source, occupant })
}

impl BusyReport {
    /// "LM Studio reports it generating" / "darkmux process 123 is
    /// dispatching to it".
    fn fact_clause(&self) -> String {
        match &self.source {
            BusySource::LmStudioStatus(status) => format!("LM Studio reports it {status}"),
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
            Occupant::NotDarkmux => ", on work darkmux did not start".to_string(),
        }
    }

    /// The answering seat's reply — user-facing copy, the same on the CLI
    /// and in the editor panel. States the instance, the fact, the
    /// occupant, that radio did not queue, and the two ways out.
    pub fn answering_seat_message(&self) -> String {
        format!(
            "the model radio would answer with, `{}`, is busy: {}{}. Radio did not queue behind it. \
             Ask again when that finishes, or point `radio.answerer_profile` at a model that is free.",
            self.identifier,
            self.fact_clause(),
            self.occupant_clause()
        )
    }
}

/// The router's "still waiting" notice, after `waited` with no decision
/// yet. Says what is happening from the facts in hand — busy (and with
/// what), idle (so the delay is the model itself, not a queue), or no
/// utility instance resolved to check — and that routing keeps waiting, up
/// to `ceiling`. Pure over its inputs; [`router_wait_notice_live`] gathers
/// them.
pub fn router_wait_notice(
    target: Option<&LocalTarget>,
    busy: Option<&BusyReport>,
    waited: Duration,
    ceiling: Duration,
) -> String {
    let waited = waited.as_secs();
    let ceiling = ceiling.as_secs();
    match (target, busy) {
        (_, Some(busy)) => format!(
            "still routing after {waited}s: the utility model `{}` is busy: {}{}. Routing waits behind it, \
             up to {ceiling}s.",
            busy.identifier,
            busy.fact_clause(),
            busy.occupant_clause()
        ),
        (Some(target), None) => format!(
            "still routing after {waited}s. The utility model `{}` reports idle, so nothing is queued ahead \
             of this call; the model itself is slow. Waiting up to {ceiling}s.",
            target.identifier
        ),
        (None, None) => format!(
            "still routing after {waited}s (no utility model resolved to check). Waiting up to {ceiling}s."
        ),
    }
}

// ── Live gathering ────────────────────────────────────────────────────────

/// The facts, gathered live, for `target`. `lms ps` failing is NOT a busy
/// claim (the dispatch that follows surfaces that error itself); the lease
/// registry is read the way every other reader reads it (lenient,
/// crash-orphans swept).
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

/// The router's notice with live facts about the utility instance.
pub fn router_wait_notice_live(waited: Duration) -> String {
    let target = crate::crew::dispatch::utility_local_target(None);
    let busy = target.as_ref().and_then(busy_report_live);
    router_wait_notice(
        target.as_ref(),
        busy.as_ref(),
        waited,
        Duration::from_secs(u64::from(crate::radio::ROUTER_CALL_CEILING_SECONDS)),
    )
}

/// This machine's live runs, from the SAME local records `darkmux run list`
/// reads (`darkmux_serve::build_runs` over the flows + lab dirs), local
/// only — no fleet read, because the instance in question is on this
/// machine. Called only once busy is established.
fn live_runs_from_local_records() -> Vec<LiveRun> {
    let flows_dir = darkmux_types::config_access::flows_dir();
    let lab_dir = darkmux_types::config_access::lab_dir();
    darkmux_serve::build_runs(&flows_dir, Some(&lab_dir), &[])
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

    fn resident(identifier: &str, status: &str) -> LoadedModel {
        LoadedModel {
            identifier: identifier.to_string(),
            model: identifier.trim_start_matches("darkmux:").to_string(),
            status: status.to_string(),
            size: "20.00 GB".to_string(),
            context: 100_000,
        }
    }

    fn no_runs() -> Vec<LiveRun> {
        Vec::new()
    }

    fn runs_must_not_be_read() -> Vec<LiveRun> {
        panic!("the runs board is the expensive read: it must not be touched on the idle path (#2917)")
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
            assert_eq!(report.occupant, Occupant::NotDarkmux);
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
        assert!(msg.contains("`darkmux:qwen3.6-35b-a3b`, is busy: LM Studio reports it generating for mission `pepper-refresh-rotation` (coder)."), "{msg}");
        assert!(msg.contains("Radio did not queue behind it."), "{msg}");
    }

    #[test]
    fn a_run_recorded_under_the_bare_model_key_still_matches_the_instance() {
        // Older archives record the bare key (#2240 put the identifier on
        // the wire); the recorded side is namespace-insensitive.
        let loaded = [resident("darkmux:qwen3.6-35b-a3b", "generating")];
        let runs = || vec![LiveRun { kind: "dispatch", id: "d-1".to_string(), role: None, model: Some("qwen3.6-35b-a3b".to_string()) }];
        let report = busy_report(&target(), &loaded, &[], &runs).expect("busy");
        assert_eq!(report.occupant, Occupant::Run { kind: "dispatch", id: "d-1".to_string(), role: None });
        assert!(report.answering_seat_message().contains("for dispatch `d-1`."), "{}", report.answering_seat_message());
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
            report.answering_seat_message().contains("LM Studio reports it processingPrompt for darkmux process 77."),
            "{}",
            report.answering_seat_message()
        );
    }

    #[test]
    fn the_busy_message_says_the_work_is_not_darkmuxs_when_nothing_of_ours_is_on_it() {
        let loaded = [resident("darkmux:qwen3.6-35b-a3b", "generating")];
        let msg = busy_report(&target(), &loaded, &[], &no_runs).expect("busy").answering_seat_message();
        assert!(msg.contains("LM Studio reports it generating, on work darkmux did not start."), "{msg}");
        assert!(msg.contains("`radio.answerer_profile`"), "names a way out: {msg}");
    }

    #[test]
    fn the_router_notice_names_the_fact_the_occupant_and_the_ceiling() {
        let util = LocalTarget { identifier: "darkmux:qwen3-4b".to_string(), model_key: "qwen3-4b".to_string() };
        let busy = BusyReport {
            identifier: util.identifier.clone(),
            source: BusySource::LmStudioStatus("generating".to_string()),
            occupant: Occupant::Run { kind: "mission", id: "m-1".to_string(), role: Some("coder".to_string()) },
        };
        let notice = router_wait_notice(Some(&util), Some(&busy), Duration::from_secs(10), Duration::from_secs(300));
        assert_eq!(
            notice,
            "still routing after 10s: the utility model `darkmux:qwen3-4b` is busy: LM Studio reports it \
             generating for mission `m-1` (coder). Routing waits behind it, up to 300s."
        );
        let idle = router_wait_notice(Some(&util), None, Duration::from_secs(10), Duration::from_secs(300));
        assert!(idle.contains("reports idle, so nothing is queued ahead of this call"), "{idle}");
        let none = router_wait_notice(None, None, Duration::from_secs(10), Duration::from_secs(300));
        assert!(none.contains("no utility model resolved to check"), "{none}");
    }
}
