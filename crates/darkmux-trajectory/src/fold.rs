//! [`TrajectoryFold`]: the one reading of a trajectory.
//!
//! Every count a surface quotes about a role execution (turns, tool calls,
//! compactions, rests, tokens, checkpoints, detector firings, stream
//! timing) is derived here, from the events, one event at a time. The
//! host's live tailer applies each event as it streams; the lab folds a
//! finished file. Both call [`TrajectoryFold::apply`], so a live number and
//! a post-hoc number are the same computation.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use crate::event::{
    Checkpoint, DispatchComplete, DispatchStart, MalformedReason, RestReason, TrajectoryEvent,
    Verdict,
};
use crate::usage::{TokenSum, UsageCounts};

/// One model stream, from `model.streaming.start` to `model.streaming.end`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Stream {
    pub seq: u64,
    pub start_ms: u64,
    /// `None` while the stream is open; a run that died mid-call leaves it
    /// open, and its generation time is then unknown, not zero.
    pub end_ms: Option<u64>,
    /// Answer chars this stream produced (`model.partial` restarts its
    /// cumulative count with every stream).
    pub content_chars: u64,
}

impl Stream {
    /// Generation time of a closed stream; 0 while open.
    pub fn ms(&self) -> u64 {
        match self.end_ms {
            Some(end) if end >= self.start_ms => end - self.start_ms,
            _ => 0,
        }
    }
}

/// What one logical turn (one `seq`) accumulated across its calls.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TurnDetail {
    /// Summed over every call of the turn: each checkpoint continuation
    /// reports its own usage.
    pub completion_tokens: u64,
    pub reasoning_tokens: u64,
    pub reasoning_chars: u64,
    /// Answer chars seen with no stream open (a runtime that did not
    /// record streams).
    pub content_chars: u64,
}

/// One executed tool call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolTaken {
    pub seq: u64,
    pub name: String,
    pub ok: bool,
    /// The runtime's arguments preview.
    pub args: String,
}

/// One rest the loop took.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RestTaken {
    pub ms: u64,
    pub reason: RestReason,
    pub state: Option<String>,
}

/// One checkpoint ruling.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CheckpointRuling {
    pub seq: u64,
    pub tail_ratio: Option<f64>,
    pub verdict: Verdict,
    /// The detector's finding, whatever the policy did about it.
    pub judged_degenerate: bool,
}

/// The streaming gate's record.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct StreamGateFold {
    pub observations: u64,
    /// Turns the gate judged degenerate (its own flag, acted on or not).
    pub degenerate_turns: BTreeSet<u64>,
    pub aborted_turns: BTreeSet<u64>,
    /// Abort RECORDS: a retry can be aborted too, and the seq set above
    /// collapses the two.
    pub abort_events: u64,
    pub min_tail_ratio: Option<f64>,
}

/// How often each detector-class event fired.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
pub struct DetectorCounts {
    pub cycle: u32,
    pub reasoning_loop: u32,
    pub repeated_failure: u32,
    pub intra_turn_stall: u32,
    pub empty_tool_calls: u32,
    pub per_turn_cap: u32,
    pub feedback_injected: u32,
    /// Plain-text tool calls the runtime promoted back into structured
    /// calls, summed over the promotion events.
    pub promoted_calls: u32,
}

/// Everything one pass over a trajectory yields. See the module doc.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct TrajectoryFold {
    /// Parseable events applied, any type: zero means the trajectory said
    /// nothing at all.
    pub events: u64,
    pub start: Option<DispatchStart>,
    pub complete: Option<DispatchComplete>,
    /// The latest clock any event carried.
    pub last_ts: Option<u64>,
    turn_seqs: BTreeSet<u64>,
    /// `model.completed` events: API calls, not turns.
    pub model_calls: u32,
    /// Summed over every `model.completed`, by [`UsageCounts`].
    pub tokens: TokenSum,
    /// One entry per `model.completed`, in order: its completion tokens, or
    /// `None` for a call that returned no usage (a cut stream is never
    /// billed).
    pub frames: Vec<Option<u64>>,
    /// `model.completed` events that carried no usage: their tokens are
    /// missing from `tokens`, so a total read beside a non-zero count is a
    /// floor, not the spend.
    pub unreported_calls: u32,
    /// The runtime's own estimate of the completion tokens of the calls it
    /// cut (`model.completed.completion_estimate`). Kept OUT of `tokens`,
    /// which holds only what an endpoint reported.
    pub estimated_completion_tokens: u64,
    pub turn_detail: BTreeMap<u64, TurnDetail>,
    pub streams: Vec<Stream>,
    compaction_events: u32,
    pub rests: Vec<RestTaken>,
    pub tools: Vec<ToolTaken>,
    /// Structured calls that named no darkmux tool; they never ran.
    pub tool_calls_invalid_name: u32,
    /// Structured calls that named a tool the role was not granted.
    pub tool_calls_ungranted: u32,
    pub checkpoints: Vec<CheckpointRuling>,
    /// The degeneracy policy the latest checkpoint recorded.
    pub checkpoint_policy: Option<String>,
    pub gate: StreamGateFold,
    pub detectors: DetectorCounts,
}

impl TrajectoryFold {
    /// Fold every parseable line of `raw`.
    pub fn from_lines(raw: &str) -> Self {
        let mut f = Self::default();
        for e in raw.lines().filter_map(crate::parse_line) {
            f.apply(&e);
        }
        f
    }

    /// Fold the trajectory at `path`. A missing or unreadable file folds to
    /// the empty fold: a run that died before writing one has no events.
    pub fn from_path(path: &Path) -> Self {
        Self::from_lines(&std::fs::read_to_string(path).unwrap_or_default())
    }

    /// Take one event into the fold.
    pub fn apply(&mut self, e: &TrajectoryEvent) {
        use TrajectoryEvent as E;
        self.events = self.events.saturating_add(1);
        if let Some(ts) = e.ts() {
            self.last_ts = Some(self.last_ts.map_or(ts, |t| t.max(ts)));
        }
        match e {
            E::DispatchStart(s) => {
                if self.start.is_none() {
                    self.start = Some(s.clone());
                }
            }
            E::DispatchComplete(c) => self.complete = Some(c.clone()),
            E::ModelCompleted(m) => {
                self.model_completed(m.seq, UsageCounts::of(m.usage.as_ref()));
                self.estimated_completion_tokens =
                    self.estimated_completion_tokens.saturating_add(m.completion_estimate.unwrap_or(0));
            }
            E::StreamingStart(s) => self.streams.push(Stream {
                seq: s.seq,
                start_ms: s.ts,
                end_ms: None,
                content_chars: 0,
            }),
            E::StreamingEnd(s) => {
                // The most recent OPEN stream on this seq: an aborted turn is
                // retried under the same seq, so keying by seq alone would
                // pair the retry's end with the abort's start.
                if let Some(open) =
                    self.streams.iter_mut().rev().find(|st| st.seq == s.seq && st.end_ms.is_none())
                {
                    open.end_ms = Some(s.ts);
                }
            }
            E::Partial(p) => self.partial(p.seq, p.cumulative_chars),
            E::Reasoning(r) => {
                let t = self.turn_detail.entry(r.seq).or_default();
                t.reasoning_chars = t.reasoning_chars.saturating_add(r.reasoning_chars);
            }
            E::ToolCompleted(t) => self.tools.push(ToolTaken {
                seq: t.seq,
                name: t.tool_name.clone(),
                ok: t.ok,
                args: t.args.clone(),
            }),
            E::MalformedToolNames(m) => {
                let n = u32::try_from(m.count).unwrap_or(u32::MAX);
                let bucket = match m.reason {
                    MalformedReason::NotATool => &mut self.tool_calls_invalid_name,
                    MalformedReason::RealToolNotGranted => &mut self.tool_calls_ungranted,
                };
                *bucket = bucket.saturating_add(n);
            }
            E::Compaction(_) => self.compaction_events = self.compaction_events.saturating_add(1),
            E::Rest(r) => self.rests.push(RestTaken {
                ms: r.ms,
                reason: r.reason.clone(),
                state: r.state.clone(),
            }),
            E::Checkpoint(c) => self.checkpoint(c),
            E::GateObservation(g) => {
                self.gate.observations = self.gate.observations.saturating_add(1);
                if g.degenerate {
                    self.gate.degenerate_turns.insert(g.seq);
                }
                if let Some(r) = g.tail_ratio {
                    self.gate.min_tail_ratio = Some(self.gate.min_tail_ratio.map_or(r, |m| m.min(r)));
                }
            }
            E::GateAbort(g) => {
                self.gate.abort_events = self.gate.abort_events.saturating_add(1);
                self.gate.aborted_turns.insert(g.seq);
            }
            E::CycleSuspected(_) => bump(&mut self.detectors.cycle, 1),
            E::ReasoningLoopSuspected(_) => bump(&mut self.detectors.reasoning_loop, 1),
            E::RepeatedToolFailure(_) => bump(&mut self.detectors.repeated_failure, 1),
            E::IntraTurnStallRecovered(_) => bump(&mut self.detectors.intra_turn_stall, 1),
            E::EmptyToolCallsRecovered(_) => bump(&mut self.detectors.empty_tool_calls, 1),
            E::PerTurnCapSalvaged(_) => bump(&mut self.detectors.per_turn_cap, 1),
            E::FeedbackInjected(_) => bump(&mut self.detectors.feedback_injected, 1),
            E::ToolCallPromoted(p) => {
                bump(&mut self.detectors.promoted_calls, u32::try_from(p.promoted_call_count).unwrap_or(u32::MAX))
            }
            E::ToolCallWriting(_)
            | E::PromotionSuppressed(_)
            | E::CompactionStart(_)
            | E::CompactionCall(_)
            | E::CompactionSkipped(_)
            | E::CompactionUnproductive(_)
            | E::Context(_)
            | E::StaleContextTokens(_)
            | E::PreSendBound(_)
            | E::ReasoningBoundNotApplied(_)
            | E::ToolCallDiscarded(_)
            | E::EscalationTriggered(_)
            | E::Unknown => {}
        }
    }

    fn model_completed(&mut self, seq: u64, counts: UsageCounts) {
        self.turn_seqs.insert(seq);
        self.model_calls = self.model_calls.saturating_add(1);
        self.tokens.add(&counts);
        if !counts.reported() {
            self.unreported_calls = self.unreported_calls.saturating_add(1);
        }
        self.frames.push(counts.completion);
        let t = self.turn_detail.entry(seq).or_default();
        t.completion_tokens = t.completion_tokens.saturating_add(counts.completion.unwrap_or(0));
        t.reasoning_tokens = t.reasoning_tokens.saturating_add(counts.reasoning.unwrap_or(0));
    }

    fn partial(&mut self, seq: u64, cumulative_chars: u64) {
        // Cumulative within ONE stream, so it belongs to the latest stream
        // on this seq and is summed across streams.
        if let Some(s) = self.streams.iter_mut().rev().find(|s| s.seq == seq) {
            s.content_chars = s.content_chars.max(cumulative_chars);
        } else {
            let t = self.turn_detail.entry(seq).or_default();
            t.content_chars = t.content_chars.max(cumulative_chars);
        }
    }

    fn checkpoint(&mut self, c: &Checkpoint) {
        if let Some(p) = &c.policy {
            self.checkpoint_policy = Some(p.clone());
        }
        self.checkpoints.push(CheckpointRuling {
            seq: c.seq,
            tail_ratio: c.tail_ratio,
            verdict: c.verdict,
            judged_degenerate: c.judged_degenerate(),
        });
    }

    /// Logical turns: the distinct `seq` among the `model.completed` events
    /// (a checkpoint continuation resumes its turn under the same `seq`).
    pub fn turns(&self) -> u32 {
        u32::try_from(self.turn_seqs.len()).unwrap_or(u32::MAX)
    }

    /// Installed compactions.
    pub fn compactions(&self) -> u32 {
        self.compaction_events
    }

    pub fn tool_calls(&self) -> u32 {
        u32::try_from(self.tools.len()).unwrap_or(u32::MAX)
    }

    /// Calls that dispatched and came back `ok: false`.
    pub fn tool_calls_failed(&self) -> u32 {
        u32::try_from(self.tools.iter().filter(|t| !t.ok).count()).unwrap_or(u32::MAX)
    }

    pub fn rest_count(&self) -> u32 {
        u32::try_from(self.rests.len()).unwrap_or(u32::MAX)
    }

    /// Every rest's duration, summed.
    pub fn rest_ms(&self) -> u64 {
        self.rests.iter().fold(0u64, |acc, r| acc.saturating_add(r.ms))
    }

    /// Of [`Self::rest_ms`], the paced rests (a pause or the thermal
    /// governor, never routine cool-down).
    pub fn paced_rest_ms(&self) -> u64 {
        self.rests
            .iter()
            .filter(|r| matches!(r.reason, RestReason::Paced(_)))
            .fold(0u64, |acc, r| acc.saturating_add(r.ms))
    }

    /// Generation time of the closed streams on one turn.
    pub fn generation_ms(&self, seq: u64) -> Option<u64> {
        let mut closed = self.streams.iter().filter(|s| s.seq == seq && s.end_ms.is_some()).peekable();
        closed.peek()?;
        Some(closed.fold(0u64, |acc, s| acc.saturating_add(s.ms())))
    }

    /// The runtime's own wall clock when it reached `dispatch.complete`;
    /// for a run killed before that, the time its events span, which is
    /// what is known of it.
    pub fn wall_ms(&self) -> Option<u64> {
        if let Some(c) = &self.complete {
            return Some(c.wall_ms);
        }
        Some(self.last_ts?.saturating_sub(self.start.as_ref()?.ts))
    }

    /// The clock of the run's first event, `dispatch.start`.
    pub fn started_at_ms(&self) -> Option<u64> {
        self.start.as_ref().map(|s| s.ts)
    }

    /// Checkpoints whose verdict was to conclude.
    pub fn checkpoints_concluded(&self) -> u32 {
        u32::try_from(self.checkpoints.iter().filter(|c| c.verdict == Verdict::Conclude).count())
            .unwrap_or(u32::MAX)
    }

    /// (#1959) The WORST and the MEAN novelty ratio across the run's
    /// checkpoints, `None` when no checkpoint measured one.
    ///
    /// The last ratio alone INVERTED the ranking on two real crawls: a run
    /// that decayed to 0.193, tripped the gate and recovered reported
    /// `last = 0.997`, above a clean run's 0.976. Together, `min` answers
    /// "did this ever degenerate" and `mean` "how much of the run was
    /// compromised": low min + high mean is one excursion caught and
    /// recovered, low both is chronic, high both is clean.
    pub fn checkpoint_tail_ratios(&self) -> (Option<f64>, Option<f64>) {
        let ratios: Vec<f64> = self.checkpoints.iter().filter_map(|c| c.tail_ratio).collect();
        if ratios.is_empty() {
            return (None, None);
        }
        let min = ratios.iter().copied().fold(f64::INFINITY, f64::min);
        (Some(min), Some(ratios.iter().sum::<f64>() / ratios.len() as f64))
    }
}

fn bump(counter: &mut u32, by: u32) {
    *counter = counter.saturating_add(by);
}
