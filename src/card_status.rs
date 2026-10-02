//! A machine's status as its fleet card words it, for `darkmux machine list`.
//!
//! The fleet lens words each machine card's status in
//! `ui/src/lenses/fleet/cardStatus.ts` (`STATUS_WORD`, `secondLineOf`,
//! `statusReason`): idle, dispatch in flight, online (with "not streaming" on
//! the count line), not streaming, offline. A phone has no tooltips, so the
//! console reaches the same words and reasons through `machine list`. This is
//! the Rust twin of that derivation, over the inputs the fleet view itself
//! carries: the row's liveness, its card outcome and whether this row is this
//! machine. `tests/fixtures/card-status-rows.json` holds rows with the visible
//! words both sides must give; this module's test and `cards.test.ts` read the
//! same file, so the two cannot drift apart unnoticed.
//!
//! What the view cannot supply is the flow window, which the lens uses to say
//! whether records from a machine reach the hub. A live presence beat (or this
//! machine itself) stands in for "records reach this hub". "Checking" is the
//! lens's word for sources that have not answered yet; one gather has always
//! answered, so it is never said here. A model working on a card the view read
//! stands in for the lens's `/runs` row: a loaded model in a busy state means
//! running.

use darkmux_serve::fleet_view::{CardOutcome, FleetMachine, Liveness, UnavailableWhy, UnreachableReason};
use darkmux_serve::wire::UtilityModel;

/// The status, as the card's status line has it. The page-only states
/// (`checking…`, `disconnected`) have no twin here: one gather has always
/// answered, and a CLI has no daemon connection to lose.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum CardStatus {
    Idle,
    Running,
    /// The card was read, so the machine is up, but its flow stream does not
    /// reach this hub.
    OnlineNotStreaming,
    /// Nothing reaches this hub and the card could not be read either.
    NotStreaming,
    Offline,
}

impl CardStatus {
    /// The status line's word.
    pub(crate) fn word(self) -> &'static str {
        match self {
            CardStatus::Idle => "idle",
            CardStatus::Running => "dispatch in flight",
            CardStatus::OnlineNotStreaming => "online",
            CardStatus::NotStreaming => "not streaming",
            CardStatus::Offline => "offline",
        }
    }

    /// What the count line says in place of a count.
    pub(crate) fn second_line(self) -> Option<&'static str> {
        (self == CardStatus::OnlineNotStreaming).then_some("not streaming")
    }
}

/// The status and, when the word needs one, why it says it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct StatusReading {
    pub status: CardStatus,
    pub reason: Option<String>,
}

/// The card's fixed phrase for a card the view could not read, `None` for one
/// it read. The twin of `outcomeLine` in `viewRows.ts`.
fn outcome_line(outcome: &CardOutcome) -> Option<&'static str> {
    Some(match outcome {
        CardOutcome::Available { .. } => return None,
        CardOutcome::Unavailable { why, .. } => unavailable_line(*why),
        CardOutcome::Mismatch { .. } => "another machine answered",
        CardOutcome::Unreachable { reason, .. } => unreachable_line(*reason),
        CardOutcome::Unknown => "card state unknown",
    })
}

fn unavailable_line(why: UnavailableWhy) -> &'static str {
    match why {
        UnavailableWhy::NoCardRoute => "no card (older peer)",
        UnavailableWhy::OtherSchemaMajor => "card schema differs",
        UnavailableWhy::Unparseable => "card unreadable",
        UnavailableWhy::Unknown => "card unavailable",
    }
}

fn unreachable_line(reason: UnreachableReason) -> &'static str {
    match reason {
        UnreachableReason::BadAddress => "bad address",
        UnreachableReason::DnsFailed => "address not found",
        UnreachableReason::IdentityUnavailable => "identity unavailable",
        UnreachableReason::NotOnOverlay => "not on the overlay network",
        UnreachableReason::PinNotSaved => "pin not saved",
        UnreachableReason::PinMismatch => "identity mismatch",
        UnreachableReason::ListenerOff => "not listening",
        UnreachableReason::AuthRequired => "auth required",
        UnreachableReason::RefusedByPeer => "refused by peer",
        UnreachableReason::ListenerUnavailable => "listener unavailable",
        UnreachableReason::BadAnswer => "bad answer",
        UnreachableReason::Unknown => "unreachable",
    }
}

/// Whether a loaded model of the card is working right now.
fn a_model_is_working(m: &FleetMachine) -> bool {
    let CardOutcome::Available { card, .. } = &m.card else { return false };
    card.specs.loaded_models.iter().any(|x| crate::radio_busy::is_busy_status(&x.status))
}

const NOT_STREAMING_READ: &str = "online · not streaming: its flow stream doesn't reach this hub, so its activity can't be shown here.";

/// The status of one machine row. Same precedence as `statusOf` in
/// `cards.ts`: offline, then work, then the not-streaming forms, then idle.
pub(crate) fn card_status(m: &FleetMachine) -> StatusReading {
    let card_read = matches!(m.card, CardOutcome::Available { .. });
    // `rowStanding`: a card the view read proves the machine is up.
    let online = m.is_this_machine || card_read || m.liveness == Liveness::Live;
    let offline = !online && m.liveness == Liveness::NoBeat;
    let seen = m.is_this_machine || m.liveness == Liveness::Live;
    // `machineAvailability`: a machine that is down is not a stream problem.
    let streams_here = m.is_this_machine || (!offline && seen);
    let note = outcome_line(&m.card);
    if offline {
        let why = note.unwrap_or("its presence beat stopped");
        return StatusReading { status: CardStatus::Offline, reason: Some(format!("offline: {why}")) };
    }
    if a_model_is_working(m) {
        return StatusReading { status: CardStatus::Running, reason: None };
    }
    if !streams_here {
        return if card_read {
            StatusReading { status: CardStatus::OnlineNotStreaming, reason: Some(NOT_STREAMING_READ.to_string()) }
        } else {
            // A card that was not read always has its reason (`outcome_line`).
            StatusReading { status: CardStatus::NotStreaming, reason: Some(format!("not streaming: {}", note.unwrap_or("its card couldn't be read"))) }
        };
    }
    StatusReading { status: CardStatus::Idle, reason: None }
}

/// The utility model line: its name, whether it is resident, and its job.
/// A utility job is a flow record, which neither the machine card nor a
/// one-shot CLI read carries, so the job is said to be unshown, never guessed.
pub(crate) fn utility_line(utility: Option<&UtilityModel>) -> String {
    match utility {
        Some(u) => format!(
            "utility model: {}, {}; job: not shown here",
            u.id,
            if u.loaded { "resident" } else { "not resident" }
        ),
        None => "utility model: none bound; job: not shown here".to_string(),
    }
}

/// The lines `machine list` prints under a machine's row: its status word, the
/// count line's second line and the tooltip's reason when it has them, and, for
/// a card that was read, its utility model.
pub(crate) fn status_lines(m: &FleetMachine) -> Vec<String> {
    let reading = card_status(m);
    let mut lines = vec![format!("  status: {}", reading.status.word())];
    lines.extend(reading.status.second_line().map(|line| format!("  {line}")));
    lines.extend(reading.reason.map(|why| format!("  why: {why}")));
    if let CardOutcome::Available { card, .. } = &m.card {
        lines.push(format!("  {}", utility_line(card.specs.utility_model.as_ref())));
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(serde::Deserialize)]
    struct Expect {
        status: CardStatus,
        word: String,
        second_line: Option<String>,
        reason: Option<String>,
    }
    #[derive(serde::Deserialize)]
    struct Case {
        name: String,
        row: FleetMachine,
        expect: Expect,
    }

    /// The rows `cards.test.ts` reads too: both derivations give the same
    /// VISIBLE words (status word, count line's second line, tooltip).
    #[test]
    fn the_rust_status_gives_the_words_the_fleet_card_gives_for_the_shared_rows() {
        let raw = include_str!("../tests/fixtures/card-status-rows.json");
        let cases: Vec<Case> = serde_json::from_str(raw).expect("the shared rows parse as fleet rows");
        assert!(cases.len() >= 10, "the fixture must keep covering every status");
        for c in cases {
            let got = card_status(&c.row);
            assert_eq!(got.status, c.expect.status, "{}", c.name);
            assert_eq!(got.status.word(), c.expect.word, "{}", c.name);
            assert_eq!(got.status.second_line().map(str::to_string), c.expect.second_line, "{}", c.name);
            assert_eq!(got.reason, c.expect.reason, "{}", c.name);
        }
    }

    #[test]
    fn the_utility_line_says_resident_or_not_and_never_guesses_the_job() {
        let u = |loaded| UtilityModel { id: "qwen3-4b".into(), loaded, n_ctx: None };
        assert_eq!(utility_line(Some(&u(true))), "utility model: qwen3-4b, resident; job: not shown here");
        assert_eq!(utility_line(Some(&u(false))), "utility model: qwen3-4b, not resident; job: not shown here");
        assert_eq!(utility_line(None), "utility model: none bound; job: not shown here");
    }

    #[test]
    fn status_lines_carry_the_reason_and_the_cards_utility_model() {
        let raw = include_str!("../tests/fixtures/card-status-rows.json");
        let cases: Vec<Case> = serde_json::from_str(raw).unwrap();
        let by = |name: &str| cases.iter().find(|c| c.name == name).unwrap_or_else(|| panic!("{name}"));
        let off = status_lines(&by("peer, no beat, listener off").row);
        assert_eq!(off, vec!["  status: offline".to_string(), "  why: offline: not listening".to_string()]);
        let online = status_lines(&by("peer, card read, no beat").row);
        assert_eq!(&online[..2], ["  status: online", "  not streaming"]);
        let idle = status_lines(&by("peer, beating, idle").row);
        assert_eq!(idle[0], "  status: idle");
        assert_eq!(idle[1], "  utility model: qwen3-4b, resident; job: not shown here");
    }
}
