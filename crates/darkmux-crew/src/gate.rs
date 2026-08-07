//! Operator sign-off gate for mission steps (#1684 Packet 2).
//!
//! Packet 1 (#1695) built the panel-advertised, registry-driven command
//! surface (`darkmux acp`'s slash commands, `src/acp_panel.rs`) and its
//! ephemeral in-process runner. This packet is the mechanism BEHIND a
//! gated panel verb — `pr-merge`, `pr-approve`, or any future config that
//! declares `"gate": "operator"` on a step (`mission_config::StepConfig::
//! gate`, schema 2.2) — the generic operator-confirmation checkpoint the
//! `#1685` verb spec calls "the gate mechanism".
//!
//! # The seam
//!
//! `scheduler::run_step_graph` already takes a caller-supplied `persist:
//! &mut dyn FnMut(&Step)` closure — a durability seam every launcher wires
//! to its own storage. [`GateHandler`] mirrors that shape: a caller-
//! supplied `FnMut(&Step, &BTreeMap<String, String>) -> GateDecision`,
//! invoked by the scheduler ONLY for a step whose `gate` field names a
//! recognized gate kind. `facts` is the step's composed upstream input map
//! — the SAME map `scheduler::gather_inputs` would hand the step at run
//! time — so a surface rendering a confirmation dialog shows the operator
//! the real facts the step is about to run with, not a re-derived summary.
//!
//! Three production handlers exist, one per surface (the #1685 spec,
//! verbatim: "ACP → native session/request_permission dialog; interactive
//! CLI → prompt; non-interactive → blocks pending sign-off"):
//!
//!   - ACP: `src/acp_panel.rs`'s ephemeral runner wires a handler that
//!     raises `session/request_permission` to the connected client and
//!     blocks its worker thread on the response (the ephemeral run already
//!     executes under `spawn_blocking` — see that module's doc). Lives in
//!     the `darkmux` binary crate, not here, because it depends on the
//!     `agent-client-protocol` connection.
//!   - Interactive CLI: [`tty_prompt_handler`] — a y/N stdin prompt.
//!   - Non-interactive: [`refusal_handler`] — the DEFAULT when no better
//!     surface exists, and what a `None` gate argument to
//!     `scheduler::run_step_graph` falls back to internally (see
//!     [`resolve_gate`]).
//!
//! # Fail-closed, not fail-open
//!
//! An operator sign-off gate exists to make a consequential action
//! (merging a PR, approving a review) require a live human decision. The
//! ONLY way [`resolve_gate`] returns [`GateDecision::Approved`] is a
//! handler explicitly saying so for the recognized `"operator"` gate kind.
//! Every other path — no handler supplied, an unrecognized gate value, a
//! handler that errors — refuses the step. There is no code path in this
//! module that lets an unattended run complete a gated step.

use crate::types::Step;
use std::collections::BTreeMap;

/// The only gate kind this binary recognizes today. A step's `gate` field
/// (`mission_config::StepConfig::gate` / `crew::types::Step::gate`), when
/// present, is compared against this constant — see [`resolve_gate`] for
/// what happens on a mismatch.
pub const GATE_KIND_OPERATOR: &str = "operator";

/// The result of evaluating an operator sign-off gate for one step.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GateDecision {
    /// The step may run.
    Approved,
    /// The step must NOT run. `reason` is recorded in the step's own
    /// terminal `output` (mirroring any other step-level failure — see
    /// `scheduler::apply_step_terminal`) and surfaces wherever a failed
    /// step's output surfaces (the mission board, the panel message, the
    /// envelope).
    Declined { reason: String },
}

/// Caller-supplied gate handler — see the module doc's "The seam" section.
/// Invoked ONLY for a step whose `gate` field is `Some(GATE_KIND_OPERATOR)`;
/// an ungated step (`gate: None`) never reaches this closure, and neither
/// does a step declaring an unrecognized gate kind (see [`resolve_gate`]).
pub type GateHandler<'a> = dyn FnMut(&Step, &BTreeMap<String, String>) -> GateDecision + 'a;

/// Evaluate a step's `gate` field against `facts` (its composed upstream
/// input map), deciding whether [`GateHandler`] gets invoked at all, and
/// with what fallback when the caller supplied none.
///
/// - `gate: None` (the step is ungated) → `None` — the caller runs the
///   step normally. `handler` is NEVER invoked for an ungated step,
///   regardless of whether one was supplied.
/// - `gate: Some("operator")` and a `handler` was supplied → `Some(handler
///   (step, facts))` — the caller's decision, verbatim.
/// - `gate: Some("operator")` and NO `handler` was supplied (a caller,
///   like the review driver or a bare scheduler unit test, that never
///   expects a gated step in its own graphs) → `Some(Declined { .. })` via
///   [`refusal_handler`] — fails closed rather than silently approving.
/// - `gate: Some(other)` (an unrecognized gate kind) → `Some(Declined {
///   .. })`, WITHOUT ever invoking `handler` — an unrecognized kind is not
///   the handler's problem to interpret; a surface built for `"operator"`
///   dialogs has no idea how to render a kind it's never heard of, and
///   guessing is exactly the silent-fallback failure mode this gate exists
///   to prevent. See the module doc's "Fail-closed, not fail-open".
pub fn resolve_gate(
    step: &Step,
    facts: &BTreeMap<String, String>,
    handler: Option<&mut GateHandler<'_>>,
) -> Option<GateDecision> {
    match step.gate.as_deref() {
        None => None,
        Some(GATE_KIND_OPERATOR) => Some(match handler {
            Some(h) => h(step, facts),
            None => refusal_handler()(step, facts),
        }),
        Some(other) => Some(GateDecision::Declined {
            reason: format!(
                "step `{}` declares unrecognized gate kind \"{other}\" — only \"{GATE_KIND_OPERATOR}\" \
                 is recognized; refusing to run rather than treating an unknown gate as ungated \
                 (fail closed)",
                step.id
            ),
        }),
    }
}

/// The non-interactive default handler (the #1685 spec: "non-interactive →
/// blocks pending sign-off"). Always Declines, naming the gated step and
/// how to actually approve it — never silently completes, never hangs
/// waiting for input that will never arrive on a non-interactive surface
/// (CI, a headless dispatch, a subprocess with no controlling terminal).
pub fn refusal_handler<'a>() -> impl FnMut(&Step, &BTreeMap<String, String>) -> GateDecision + 'a {
    |step: &Step, _facts: &BTreeMap<String, String>| GateDecision::Declined {
        reason: format!(
            "step `{}` requires operator sign-off — run from the editor panel (ACP) or an \
             interactive terminal to approve it",
            step.id
        ),
    }
}

/// The interactive-CLI handler (the #1685 spec: "interactive CLI →
/// prompt"). Prints the step id + its composed input facts, then reads a
/// y/N line from stdin — `y`/`yes` (case-insensitive) approves, anything
/// else (including EOF/a read error) declines. Sorted by key for a stable,
/// diffable rendering across runs of the same step.
///
/// Callers decide WHETHER to use this handler by checking
/// `std::io::IsTerminal` on stdin themselves (see `src/mission_launch.rs`)
/// — this function does no tty detection of its own, so it stays testable
/// against an injected reader/writer pair (see [`tty_prompt_handler_with`]).
pub fn tty_prompt_handler<'a>() -> impl FnMut(&Step, &BTreeMap<String, String>) -> GateDecision + 'a {
    tty_prompt_handler_with(std::io::BufReader::new(std::io::stdin()), std::io::stdout())
}

/// The injectable core of [`tty_prompt_handler`] — `reader`/`writer` are
/// caller-supplied so a test can drive the prompt without a real tty.
pub fn tty_prompt_handler_with<'a, R, W>(
    mut reader: R,
    mut writer: W,
) -> impl FnMut(&Step, &BTreeMap<String, String>) -> GateDecision + 'a
where
    R: std::io::BufRead + 'a,
    W: std::io::Write + 'a,
{
    move |step: &Step, facts: &BTreeMap<String, String>| {
        let _ = writeln!(writer, "── operator sign-off required — step `{}` ──", step.id);
        if facts.is_empty() {
            let _ = writeln!(writer, "  (no upstream facts)");
        } else {
            for (k, v) in facts {
                let _ = writeln!(writer, "  {k}: {v}");
            }
        }
        let _ = write!(writer, "Approve? [y/N] ");
        let _ = writer.flush();
        let mut line = String::new();
        let approved = match reader.read_line(&mut line) {
            Ok(0) => false, // EOF — decline, never hang
            Ok(_) => matches!(line.trim().to_ascii_lowercase().as_str(), "y" | "yes"),
            Err(_) => false,
        };
        if approved {
            GateDecision::Approved
        } else {
            GateDecision::Declined {
                reason: format!("step `{}` — operator declined sign-off at the CLI prompt", step.id),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::NodeStatus;

    fn gated_step(gate: Option<&str>) -> Step {
        Step {
            id: "s1".to_string(),
            task_id: "t1".to_string(),
            kind: "procedural.noop".to_string(),
            gate: gate.map(str::to_string),
            status: NodeStatus::Planned,
            config: serde_json::Value::Null,
            started_ts: None,
            completed_ts: None,
            output: None,
        }
    }

    #[test]
    fn ungated_step_never_invokes_the_handler() {
        let step = gated_step(None);
        let mut calls = 0;
        let mut handler = |_s: &Step, _f: &BTreeMap<String, String>| {
            calls += 1;
            GateDecision::Approved
        };
        let decision = resolve_gate(&step, &BTreeMap::new(), Some(&mut handler));
        assert!(decision.is_none(), "an ungated step must not produce a gate decision at all");
        assert_eq!(calls, 0, "the handler must never be invoked for an ungated step");
    }

    #[test]
    fn recognized_gate_invokes_the_handler_and_returns_its_decision() {
        let step = gated_step(Some(GATE_KIND_OPERATOR));
        let mut handler = |_s: &Step, _f: &BTreeMap<String, String>| GateDecision::Approved;
        let decision = resolve_gate(&step, &BTreeMap::new(), Some(&mut handler));
        assert_eq!(decision, Some(GateDecision::Approved));
    }

    #[test]
    fn recognized_gate_can_be_declined_by_the_handler() {
        let step = gated_step(Some(GATE_KIND_OPERATOR));
        let mut handler = |_s: &Step, _f: &BTreeMap<String, String>| GateDecision::Declined {
            reason: "no thanks".to_string(),
        };
        let decision = resolve_gate(&step, &BTreeMap::new(), Some(&mut handler));
        assert_eq!(decision, Some(GateDecision::Declined { reason: "no thanks".to_string() }));
    }

    #[test]
    fn recognized_gate_with_no_handler_fails_closed_via_the_refusal_default() {
        let step = gated_step(Some(GATE_KIND_OPERATOR));
        let decision = resolve_gate(&step, &BTreeMap::new(), None);
        match decision {
            Some(GateDecision::Declined { reason }) => {
                assert!(reason.contains("operator sign-off"), "{reason}");
            }
            other => panic!("expected a fail-closed Declined default, got {other:?}"),
        }
    }

    #[test]
    fn unrecognized_gate_value_fails_closed_without_invoking_the_handler() {
        let step = gated_step(Some("some-future-kind"));
        let mut calls = 0;
        let mut handler = |_s: &Step, _f: &BTreeMap<String, String>| {
            calls += 1;
            GateDecision::Approved
        };
        let decision = resolve_gate(&step, &BTreeMap::new(), Some(&mut handler));
        assert_eq!(calls, 0, "an unrecognized gate kind must never reach the handler");
        match decision {
            Some(GateDecision::Declined { reason }) => {
                assert!(reason.contains("some-future-kind"), "{reason}");
                assert!(reason.contains("fail closed"), "{reason}");
            }
            other => panic!("expected a fail-closed Declined default, got {other:?}"),
        }
    }

    #[test]
    fn gate_handler_receives_the_composed_facts_map_verbatim() {
        let step = gated_step(Some(GATE_KIND_OPERATOR));
        let mut facts = BTreeMap::new();
        facts.insert("gather".to_string(), "42 open PRs".to_string());
        let mut received: Option<BTreeMap<String, String>> = None;
        let mut handler = |_s: &Step, f: &BTreeMap<String, String>| {
            received = Some(f.clone());
            GateDecision::Approved
        };
        resolve_gate(&step, &facts, Some(&mut handler));
        assert_eq!(received, Some(facts));
    }

    #[test]
    fn tty_prompt_handler_approves_on_y() {
        let step = gated_step(Some(GATE_KIND_OPERATOR));
        let reader = std::io::Cursor::new(b"y\n".to_vec());
        let mut writer = Vec::new();
        let mut handler = tty_prompt_handler_with(reader, &mut writer);
        let decision = handler(&step, &BTreeMap::new());
        assert_eq!(decision, GateDecision::Approved);
    }

    #[test]
    fn tty_prompt_handler_approves_on_yes_case_insensitive() {
        let step = gated_step(Some(GATE_KIND_OPERATOR));
        let reader = std::io::Cursor::new(b"YES\n".to_vec());
        let mut writer = Vec::new();
        let mut handler = tty_prompt_handler_with(reader, &mut writer);
        assert_eq!(handler(&step, &BTreeMap::new()), GateDecision::Approved);
    }

    #[test]
    fn tty_prompt_handler_declines_on_n() {
        let step = gated_step(Some(GATE_KIND_OPERATOR));
        let reader = std::io::Cursor::new(b"n\n".to_vec());
        let mut writer = Vec::new();
        let mut handler = tty_prompt_handler_with(reader, &mut writer);
        match handler(&step, &BTreeMap::new()) {
            GateDecision::Declined { reason } => assert!(reason.contains("declined")),
            other => panic!("expected Declined, got {other:?}"),
        }
    }

    #[test]
    fn tty_prompt_handler_declines_on_empty_input_eof() {
        // No trailing newline at all — read_line returns Ok(0) at EOF.
        let step = gated_step(Some(GATE_KIND_OPERATOR));
        let reader = std::io::Cursor::new(Vec::new());
        let mut writer = Vec::new();
        let mut handler = tty_prompt_handler_with(reader, &mut writer);
        assert!(matches!(handler(&step, &BTreeMap::new()), GateDecision::Declined { .. }));
    }

    #[test]
    fn tty_prompt_handler_renders_the_facts_in_its_prompt() {
        let step = gated_step(Some(GATE_KIND_OPERATOR));
        let mut facts = BTreeMap::new();
        facts.insert("gather".to_string(), "CI green, 0 unresolved".to_string());
        let reader = std::io::Cursor::new(b"n\n".to_vec());
        let mut writer = Vec::new();
        {
            let mut handler = tty_prompt_handler_with(reader, &mut writer);
            handler(&step, &facts);
        }
        let rendered = String::from_utf8(writer).unwrap();
        assert!(rendered.contains("s1"), "{rendered}");
        assert!(rendered.contains("CI green, 0 unresolved"), "{rendered}");
    }
}
