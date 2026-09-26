//! (#2928) The live channel's sampler: which of an execution's per-chunk
//! model events become live samples.
//!
//! The runtime writes one `model.partial` trajectory event per streamed
//! chunk (tens per second), and the host tailer reads every one of them on
//! its 250 ms poll. The durable path coalesces them to one heartbeat per
//! 2 s. The live path forwards more, but not all:
//!
//! - **At the cadence.** At most one sample per `cadence_ms` of the
//!   runtime's own event time while the state is steady.
//! - **Every transition, as it happens.** When the model changes what it
//!   is doing (reasoning, writing visible text, writing a tool call, a new
//!   turn opening), the last sample of the old state and the first of the
//!   new one are both sent at once, whatever the cadence says. A think
//!   burst shorter than one cadence window is therefore still delimited on
//!   both sides, rather than being read as generation across the pair.
//!   No smoothing: nothing is averaged, held back or interpolated.
//! - **Bounded.** A model that flips state on every chunk would otherwise
//!   turn "every transition" into "every chunk"; at most
//!   [`MAX_SENDS_PER_WINDOW`] samples leave per cadence window, and the
//!   rest are held like steady samples.
//!
//! Pure over event timestamps: [`LiveGate::offer`] reads only the sample's
//! own `ts`, and [`LiveGate::flush_due`] takes the caller's clock as an
//! argument, so a test drives it with a frozen clock.

use serde_json::Value;

/// The most samples one cadence window may send, transitions included.
pub(crate) const MAX_SENDS_PER_WINDOW: u32 = 6;

/// What the model is doing, as far as a sample can tell.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LiveMode {
    /// A turn's opening sample: the request is going out, nothing generated.
    Opening,
    /// The total grew and the visible text did not: reasoning.
    Thinking,
    /// The visible text grew.
    Visible,
    /// The model is writing a named tool call.
    Writing,
}

#[derive(Debug)]
pub(crate) struct LiveGate {
    cadence_ms: u64,
    /// Event time the current cadence window opened at.
    window_start: Option<u64>,
    /// Samples sent in the current window.
    window_sends: u32,
    mode: Option<LiveMode>,
    turn: Option<Value>,
    /// (generated, visible) of the previous offered sample.
    counts: Option<(u64, u64)>,
    /// The newest sample not sent yet, with its event time.
    held: Option<(u64, Value)>,
}

fn count(p: &Value, key: &str) -> Option<u64> {
    p.get(key).and_then(Value::as_u64)
}

impl LiveGate {
    pub(crate) fn new(cadence_ms: u64) -> Self {
        LiveGate {
            cadence_ms: cadence_ms.max(1),
            window_start: None,
            window_sends: 0,
            mode: None,
            turn: None,
            counts: None,
            held: None,
        }
    }

    fn classify(&self, p: &Value, same_turn: bool) -> LiveMode {
        if p.get("prompt_chars").is_some() {
            return LiveMode::Opening;
        }
        if p.get("phase").and_then(Value::as_str).is_some() {
            return LiveMode::Writing;
        }
        let gen = count(p, "generated_chars")
            .or_else(|| count(p, "cumulative_chars"))
            .unwrap_or(0);
        let vis = count(p, "cumulative_chars").unwrap_or(0);
        match self.counts.filter(|_| same_turn) {
            Some((_, pv)) if vis > pv => LiveMode::Visible,
            Some((pg, _)) if gen > pg => LiveMode::Thinking,
            // Nothing grew: the state has not changed.
            Some(_) => self.mode.unwrap_or(LiveMode::Visible),
            None if vis > 0 => LiveMode::Visible,
            None if gen > 0 => LiveMode::Thinking,
            None => LiveMode::Opening,
        }
    }

    /// Offer one heartbeat-shaped sample observed at event time `ts`.
    /// Returns the samples to send now, oldest first (zero, one, or two: the
    /// held last sample of the old state, then this one).
    pub(crate) fn offer(&mut self, ts: u64, sample: Value) -> Vec<Value> {
        let turn = sample.get("turn_seq").cloned().unwrap_or(Value::Null);
        let same_turn = self.turn.as_ref() == Some(&turn);
        let mode = self.classify(&sample, same_turn);
        let transition = !same_turn || self.mode != Some(mode);
        self.turn = Some(turn);
        self.mode = Some(mode);
        let gen = count(&sample, "generated_chars")
            .or_else(|| count(&sample, "cumulative_chars"))
            .unwrap_or(0);
        self.counts = Some((gen, count(&sample, "cumulative_chars").unwrap_or(0)));

        let window_over = self
            .window_start
            .map_or(true, |start| ts >= start.saturating_add(self.cadence_ms));
        if window_over {
            // A new window: this sample opens it. A held sample from the
            // closing window is superseded (steady state, same meaning) —
            // unless this is a transition, where it marks the old state's
            // last point.
            let mut out = Vec::with_capacity(2);
            if transition {
                if let Some((_, held)) = self.held.take() {
                    out.push(held);
                }
            }
            self.held = None;
            out.push(sample);
            self.window_start = Some(ts);
            self.window_sends = out.len() as u32;
            return out;
        }
        if transition && self.window_sends < MAX_SENDS_PER_WINDOW {
            let mut out = Vec::with_capacity(2);
            if let Some((_, held)) = self.held.take() {
                if self.window_sends + 2 <= MAX_SENDS_PER_WINDOW {
                    out.push(held);
                }
            }
            out.push(sample);
            self.window_sends += out.len() as u32;
            return out;
        }
        self.held = Some((ts, sample));
        Vec::new()
    }

    /// The held sample, once its window has closed on the caller's clock
    /// `now_ms`. Called after each poll so the last state of a burst is sent
    /// even when no newer event arrives to push it out.
    pub(crate) fn flush_due(&mut self, now_ms: u64) -> Option<Value> {
        let start = self.window_start?;
        if now_ms < start.saturating_add(self.cadence_ms) {
            return None;
        }
        let (ts, held) = self.held.take()?;
        self.window_start = Some(ts.max(start));
        self.window_sends = 1;
        Some(held)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn chunk(turn: u64, gen: u64, vis: u64) -> Value {
        json!({ "turn_seq": turn, "generated_chars": gen, "cumulative_chars": vis })
    }

    /// Steady generation, a chunk every 20 ms for 2 s on a frozen clock:
    /// samples leave at the cadence and never closer together.
    #[test]
    fn steady_state_samples_at_the_cadence() {
        let mut g = LiveGate::new(250);
        let mut sent_at = Vec::new();
        for i in 0..100u64 {
            let ts = 1_000 + i * 20;
            for _ in g.offer(ts, chunk(1, 10 * (i + 1), 10 * (i + 1))) {
                sent_at.push(ts);
            }
        }
        assert_eq!(
            sent_at.len(),
            8,
            "2 s at 250 ms, the first chunk included: {sent_at:?}"
        );
        for w in sent_at.windows(2) {
            assert!(
                w[1] - w[0] >= 250,
                "never closer than the cadence: {sent_at:?}"
            );
            assert!(
                w[1] - w[0] < 250 + 20,
                "and never later than the next chunk after it: {sent_at:?}"
            );
        }
    }

    /// A think burst shorter than one window: both edges are sent the moment
    /// they are seen, so the pair across the burst reads as thinking and the
    /// pair after it as visible text.
    #[test]
    fn a_short_think_burst_is_delimited_on_both_sides() {
        let mut g = LiveGate::new(250);
        assert_eq!(
            g.offer(1_000, chunk(1, 10, 10)).len(),
            1,
            "the first sample opens a window"
        );
        // Visible text, then 60 ms of reasoning inside the same window.
        assert!(g.offer(1_020, chunk(1, 20, 20)).is_empty(), "steady: held");
        let think = g.offer(1_040, chunk(1, 40, 20));
        assert_eq!(
            think,
            vec![chunk(1, 20, 20), chunk(1, 40, 20)],
            "the last visible sample, then the first thinking one"
        );
        assert!(g.offer(1_060, chunk(1, 60, 20)).is_empty());
        assert!(g.offer(1_080, chunk(1, 80, 20)).is_empty());
        let back = g.offer(1_100, chunk(1, 90, 30));
        assert_eq!(
            back,
            vec![chunk(1, 80, 20), chunk(1, 90, 30)],
            "the burst's last point was held and goes out with the transition back"
        );
    }

    /// A turn change is a transition even when the counts look alike.
    #[test]
    fn a_new_turn_and_its_opener_send_at_once() {
        let mut g = LiveGate::new(250);
        g.offer(1_000, chunk(1, 10, 10));
        assert!(g.offer(1_050, chunk(1, 20, 20)).is_empty());
        let opener = json!({ "turn_seq": 2, "generated_chars": 0, "cumulative_chars": 0, "prompt_chars": 9_000 });
        let out = g.offer(1_060, opener.clone());
        assert_eq!(out, vec![chunk(1, 20, 20), opener]);
        let first = g.offer(1_070, chunk(2, 5, 5));
        assert_eq!(
            first,
            vec![chunk(2, 5, 5)],
            "the new turn's first chunk is a transition too"
        );
    }

    #[test]
    fn a_writing_tool_call_is_its_own_state() {
        let mut g = LiveGate::new(250);
        g.offer(1_000, chunk(1, 10, 10));
        let w = json!({ "turn_seq": 1, "generated_chars": 10, "cumulative_chars": 10, "phase": "writing_tool_call", "tool_name": "bash" });
        assert_eq!(g.offer(1_010, w.clone()), vec![w.clone()]);
        assert!(
            g.offer(1_100, w.clone()).is_empty(),
            "silence ticks inside the window are steady"
        );
        assert_eq!(
            g.offer(1_260, w.clone()).len(),
            1,
            "and leave at the cadence"
        );
    }

    /// A model flipping state on every chunk cannot turn the channel into a
    /// per-chunk stream.
    #[test]
    fn flapping_is_capped_per_window() {
        let mut g = LiveGate::new(250);
        let mut sent = 0;
        let (mut gen, mut vis) = (0u64, 0u64);
        for i in 0..12u64 {
            gen += 10;
            if i % 2 == 0 {
                vis += 10;
            }
            sent += g.offer(1_000 + i * 10, chunk(1, gen, vis)).len();
        }
        assert_eq!(
            sent as u32, MAX_SENDS_PER_WINDOW,
            "one window's worth, however often it flips"
        );
    }

    /// The flush reads the caller's clock: nothing before the window
    /// closes, the held sample after.
    #[test]
    fn flush_sends_the_held_sample_once_the_window_closes() {
        let mut g = LiveGate::new(250);
        g.offer(1_000, chunk(1, 10, 10));
        assert!(g.offer(1_100, chunk(1, 20, 20)).is_empty());
        assert_eq!(g.flush_due(1_249), None, "the window is still open");
        assert_eq!(g.flush_due(1_250), Some(chunk(1, 20, 20)));
        assert_eq!(g.flush_due(2_000), None, "sent once");
        // The next window is measured from the flushed sample.
        assert!(
            g.offer(1_300, chunk(1, 30, 30)).is_empty(),
            "within 250 ms of the flushed sample"
        );
        assert_eq!(g.offer(1_350, chunk(1, 40, 40)).len(), 1);
    }
}
