//! Token counts: the usage block a trajectory event carries, and the one
//! reading of any reply's counts.

use serde::{Deserialize, Serialize};

/// `ceil(chars / 4)`: the one project-wide token ESTIMATE, for text no
/// provider has counted (a crawl unit sized against a chunk budget, a
/// prompt whose count a reply left out). Deliberately crude: real
/// tokenization is model-specific.
pub const CHARS_PER_TOKEN: usize = 4;

/// The estimated token count of `text` ([`CHARS_PER_TOKEN`]).
pub fn estimate_tokens(text: &str) -> usize {
    text.chars().count().div_ceil(CHARS_PER_TOKEN)
}

/// The usage block of a `model.completed` or `compaction.call` event, as the
/// runtime wrote it from the endpoint's reply. Read leniently per field: a
/// count that is not a whole number reads as unreported and never costs the
/// event its other counts.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct Usage {
    /// `None` when the block did not carry the count (the runtime always
    /// writes both; a null is read as unreported, never as zero).
    pub prompt_tokens: Option<u64>,
    pub completion_tokens: Option<u64>,
    /// The provider's own total. It can exceed prompt + completion: some
    /// providers bill a token class outside `completion_tokens`.
    pub total_tokens: Option<u64>,
    /// `None` when the provider reported no such field (never a fabricated
    /// zero). Whether it sits inside `completion_tokens` is provider-scoped.
    pub reasoning_tokens: Option<u64>,
    /// Same contract as `reasoning_tokens`.
    pub cached_tokens: Option<u64>,
}

impl<'de> Deserialize<'de> for Usage {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let v = serde_json::Value::deserialize(d)?;
        let count = |k: &str| v.get(k).and_then(serde_json::Value::as_u64);
        Ok(Self {
            prompt_tokens: count("prompt_tokens"),
            completion_tokens: count("completion_tokens"),
            total_tokens: count("total_tokens"),
            reasoning_tokens: count("reasoning_tokens"),
            cached_tokens: count("cached_tokens"),
        })
    }
}

impl Usage {
    /// The counts this block reported.
    pub fn counts(&self) -> UsageCounts {
        UsageCounts {
            prompt: self.prompt_tokens,
            completion: self.completion_tokens,
            total: self.total_tokens,
            reasoning: self.reasoning_tokens,
            cached: self.cached_tokens,
        }
    }
}

impl From<&UsageCounts> for Usage {
    /// The usage block the runtime records for a call: exactly the counts
    /// the provider reported, each unreported one as `null`.
    fn from(c: &UsageCounts) -> Self {
        Self {
            prompt_tokens: c.prompt,
            completion_tokens: c.completion,
            total_tokens: c.total,
            reasoning_tokens: c.reasoning,
            cached_tokens: c.cached,
        }
    }
}

/// The token counts one model call reported. Every field is tri-state:
/// `None` means the provider did not say, never zero.
///
/// Deserializes from a provider's `usage` object (the OpenAI-compatible
/// reply shape) through [`UsageCounts::from_provider`], the one parse the
/// runtime client and the host's direct calls share.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct UsageCounts {
    pub prompt: Option<u64>,
    pub completion: Option<u64>,
    /// The provider's own total. Recorded replies from some OpenAI-compatible
    /// layers (Gemini's and xAI's) report a total GREATER than prompt +
    /// completion: a token class outside `completion_tokens`. So a total is
    /// never reconstructed when the provider sent one ([`Self::total_tokens`]).
    pub total: Option<u64>,
    /// `completion_tokens_details.reasoning_tokens`. OpenAI and Azure document
    /// it as a breakdown INSIDE `completion_tokens`; that is provider-scoped,
    /// not a universal rule, so nothing adds it to or subtracts it from the
    /// completion count. LMStudio never sends it.
    pub reasoning: Option<u64>,
    /// `prompt_tokens_details.cached_tokens`: prompt tokens served from the
    /// provider's cache.
    pub cached: Option<u64>,
}

impl<'de> Deserialize<'de> for UsageCounts {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        Ok(Self::from_provider(&serde_json::Value::deserialize(d)?))
    }
}

impl UsageCounts {
    /// THE parse of a provider's `usage` object. Lenient per field: a count
    /// that is absent, null, negative, fractional or not a number reads as
    /// unreported, and never sinks the rest of the block (or the reply it
    /// arrived in).
    pub fn from_provider(usage: &serde_json::Value) -> Self {
        let count = |path: &str| usage.pointer(path).and_then(serde_json::Value::as_u64);
        Self {
            prompt: count("/prompt_tokens"),
            completion: count("/completion_tokens"),
            total: count("/total_tokens"),
            reasoning: count("/completion_tokens_details/reasoning_tokens"),
            cached: count("/prompt_tokens_details/cached_tokens"),
        }
    }

    /// The counts of a whole chat-completion reply: its `usage` object, or
    /// nothing reported when it has none.
    pub fn of_reply(reply: &serde_json::Value) -> Self {
        reply.get("usage").map(Self::from_provider).unwrap_or_default()
    }

    /// The counts of an optional usage block: no block is nothing reported.
    pub fn of(usage: Option<&Usage>) -> Self {
        usage.map(Usage::counts).unwrap_or_default()
    }

    /// True when the reply carried a usage block this call can count.
    pub fn reported(&self) -> bool {
        self.prompt.is_some() || self.completion.is_some() || self.total.is_some()
    }

    /// THE total of one call: the provider's own total when it sent one,
    /// (a total of 0 is unreported) else prompt + completion when it reported BOTH, else `None`: a split
    /// missing a half is not a total, and the prompt half is usually most
    /// of the spend. `None` means the call's spend is unknown, which a
    /// budget must never read as small. The usage record, every token sum
    /// and a step's budget settle all read this.
    pub fn total_tokens(&self) -> Option<u64> {
        let halves = match (self.prompt, self.completion) {
            (Some(p), Some(c)) => Some(p.saturating_add(c)),
            _ => None,
        };
        match self.total {
            Some(t) if t > 0 => Some(t),
            // A reported total of 0 beside non-zero halves is unreported (#3067):
            // the halves, or what one half reported, so the budget settles what
            // the display shows.
            Some(0) if self.prompt.is_some() || self.completion.is_some() => Some(self.prompt.unwrap_or(0).saturating_add(self.completion.unwrap_or(0))),
            reported => halves.or(reported),
        }
    }

    /// What every DISPLAY sum and every run total counts for one call: its
    /// [`Self::total_tokens`] when known, else whatever halves it reported (a
    /// floor: the unreported half is spend nobody measured). One rule on both
    /// sides of the wire: a usage record's reader (`darkmux_crew::usage::amount_of`)
    /// reads the same figure off the record's fields. A budget's own
    /// conservative charge for an unknown spend is a different question, and
    /// is answered by `darkmux_crew::budget::conservative_spend`.
    pub fn floor_tokens(&self) -> u64 {
        let halves = self.prompt.unwrap_or(0).saturating_add(self.completion.unwrap_or(0));
        self.total_tokens().filter(|t| *t > 0).unwrap_or(halves)
    }
}

/// A running sum of [`UsageCounts`], one call at a time.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct TokenSum {
    pub prompt: u64,
    pub completion: u64,
    /// Each call's [`UsageCounts::floor_tokens`], summed: never recomputed
    /// as prompt + completion over the whole run. A call that reported only
    /// one half adds that half (a floor), the figure a usage record's display
    /// sum shows.
    pub total: u64,
    /// `None` until a call reports the field.
    pub reasoning: Option<u64>,
    pub cached: Option<u64>,
}

impl TokenSum {
    pub fn add(&mut self, c: &UsageCounts) {
        self.prompt = self.prompt.saturating_add(c.prompt.unwrap_or(0));
        self.completion = self.completion.saturating_add(c.completion.unwrap_or(0));
        self.total = self.total.saturating_add(c.floor_tokens());
        if let Some(r) = c.reasoning {
            self.reasoning = Some(self.reasoning.unwrap_or(0).saturating_add(r));
        }
        if let Some(k) = c.cached {
            self.cached = Some(self.cached.unwrap_or(0).saturating_add(k));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `None` on a sum means no call reported the field: not zero. A reported
    /// zero is a real reading and makes the sum `Some(0)`.
    #[test]
    fn a_sum_of_calls_that_never_reported_cached_or_reasoning_has_none_not_zero() {
        let mut sum = TokenSum::default();
        sum.add(&UsageCounts { prompt: Some(10), completion: Some(5), ..Default::default() });
        sum.add(&UsageCounts { prompt: Some(3), completion: Some(2), ..Default::default() });
        assert_eq!((sum.prompt, sum.completion, sum.total), (13, 7, 20));
        assert_eq!((sum.cached, sum.reasoning), (None, None), "unreported is not zero");

        sum.add(&UsageCounts { prompt: Some(1), completion: Some(1), cached: Some(0), reasoning: Some(4), ..Default::default() });
        assert_eq!((sum.cached, sum.reasoning), (Some(0), Some(4)), "a reported zero is a reading, and the first report starts the sum");
        sum.add(&UsageCounts { prompt: Some(1), completion: Some(1), cached: Some(6), ..Default::default() });
        assert_eq!((sum.cached, sum.reasoning), (Some(6), Some(4)), "a later call that omits a field leaves it alone");
    }

    /// (#3067) One rule for a half-reported call: the run's total counts what
    /// the call DID report (its prompt half alone is a floor on the spend),
    /// the same figure a usage record's display sum shows. A sum that dropped
    /// the half would read a prompt-only call as free.
    #[test]
    fn a_call_reporting_one_half_adds_that_half_to_the_total() {
        let mut sum = TokenSum::default();
        sum.add(&UsageCounts { prompt: Some(900), ..Default::default() });
        sum.add(&UsageCounts { completion: Some(40), ..Default::default() });
        sum.add(&UsageCounts { prompt: Some(10), completion: Some(5), ..Default::default() });
        assert_eq!((sum.prompt, sum.completion, sum.total), (910, 45, 955));
        assert_eq!(UsageCounts { prompt: Some(900), ..Default::default() }.floor_tokens(), 900);
        assert_eq!(UsageCounts::default().floor_tokens(), 0, "nothing reported adds nothing");
        assert_eq!(UsageCounts { total: Some(7), prompt: Some(900), ..Default::default() }.floor_tokens(), 7, "the provider's total still wins");
        let zero_total = UsageCounts { prompt: Some(900), completion: Some(40), total: Some(0), ..Default::default() };
        assert_eq!(zero_total.floor_tokens(), 940, "a total of 0 beside halves is unreported (#3067)");
        assert_eq!(zero_total.total_tokens(), Some(940));
        // One half beside a reported 0: the budget settle and the display agree.
        let one_half = UsageCounts { prompt: Some(900), total: Some(0), ..Default::default() };
        assert_eq!(one_half.total_tokens(), Some(900));
        assert_eq!(one_half.floor_tokens(), 900);
    }

    #[test]
    fn the_provider_total_wins_and_the_split_is_only_the_fallback() {
        let c = UsageCounts { prompt: Some(9970), completion: Some(128), total: Some(11598), ..Default::default() };
        assert_eq!(c.total_tokens(), Some(11598), "a provider total above prompt + completion stands");
        let split = UsageCounts { prompt: Some(30), completion: Some(12), ..Default::default() };
        assert_eq!(split.total_tokens(), Some(42));
        let half = UsageCounts { completion: Some(12), ..Default::default() };
        assert_eq!(half.total_tokens(), None, "a split missing its prompt half is no total: the spend is unknown");
        let other_half = UsageCounts { prompt: Some(30), ..Default::default() };
        assert_eq!(other_half.total_tokens(), None);
        assert_eq!(UsageCounts::default().total_tokens(), None, "nothing reported is no total, not 0");
        assert!(!UsageCounts::default().reported());
    }

    /// The one parse of a provider's `usage` object: the runtime client and
    /// the host's direct calls both read replies through it.
    #[test]
    fn a_provider_usage_block_reads_its_counts_and_details() {
        let v = serde_json::json!({
            "prompt_tokens": 75, "completion_tokens": 1186, "total_tokens": 1261,
            "completion_tokens_details": {"reasoning_tokens": 1024},
            "prompt_tokens_details": {"cached_tokens": 64},
        });
        let c = UsageCounts::from_provider(&v);
        assert_eq!(
            c,
            UsageCounts { prompt: Some(75), completion: Some(1186), total: Some(1261), reasoning: Some(1024), cached: Some(64) }
        );
        let via_serde: UsageCounts = serde_json::from_value(v).unwrap();
        assert_eq!(via_serde, c, "serde and the direct read are the same parse");
    }

    /// (#1444) Details absent, present but empty, and a true zero are three
    /// different answers: the first two are "the provider did not say".
    #[test]
    fn provider_details_distinguish_unsaid_from_zero() {
        let base = serde_json::json!({"prompt_tokens": 100, "completion_tokens": 50, "total_tokens": 150});
        let absent = UsageCounts::from_provider(&base);
        assert_eq!((absent.reasoning, absent.cached), (None, None));
        let mut empty = base.clone();
        empty["completion_tokens_details"] = serde_json::json!({});
        empty["prompt_tokens_details"] = serde_json::json!({});
        let empty = UsageCounts::from_provider(&empty);
        assert_eq!((empty.reasoning, empty.cached), (None, None), "an empty object names no zero");
        let mut zero = base;
        zero["completion_tokens_details"] = serde_json::json!({"reasoning_tokens": 0});
        zero["prompt_tokens_details"] = serde_json::json!({"cached_tokens": 0});
        let zero = UsageCounts::from_provider(&zero);
        assert_eq!((zero.reasoning, zero.cached), (Some(0), Some(0)));
    }

    /// (#1444 review) A recorded gemini-2.5-flash reply: the provider's total
    /// exceeds prompt + completion by 1500 and must survive the parse as sent.
    #[test]
    fn a_provider_total_above_the_split_survives_the_parse() {
        let c = UsageCounts::from_provider(&serde_json::json!({"prompt_tokens": 9970, "completion_tokens": 128, "total_tokens": 11598}));
        assert_eq!(c.total_tokens(), Some(11598));
    }

    #[test]
    fn a_provider_count_that_is_not_a_whole_number_reads_as_unreported() {
        for bad in [serde_json::json!(42.5), serde_json::json!("42"), serde_json::json!(-1), serde_json::Value::Null] {
            let c = UsageCounts::from_provider(&serde_json::json!({"total_tokens": bad, "completion_tokens": 3}));
            assert_eq!(c.total, None, "{bad}");
            assert_eq!(c.completion, Some(3), "one bad field does not sink the block");
        }
        let odd_details = serde_json::json!({"prompt_tokens": 1, "completion_tokens_details": "n/a"});
        assert_eq!(UsageCounts::from_provider(&odd_details).reasoning, None);
        let reply = serde_json::json!({"choices": [], "usage": {"prompt_tokens": 9}});
        assert_eq!(UsageCounts::of_reply(&reply).prompt, Some(9));
        assert_eq!(UsageCounts::of_reply(&serde_json::json!({"choices": []})), UsageCounts::default());
    }

    #[test]
    fn a_recorded_usage_block_carries_exactly_the_reported_counts() {
        let c = UsageCounts { prompt: Some(10), total: Some(12), cached: Some(4), ..Default::default() };
        let u = Usage::from(&c);
        assert_eq!(u.counts(), c, "recording and reading back are inverse");
        assert_eq!(u.completion_tokens, None, "an unreported count is recorded as unreported");
    }

    #[test]
    fn a_sum_adds_each_calls_own_total_and_keeps_details_tri_state() {
        let mut s = TokenSum::default();
        s.add(&UsageCounts { prompt: Some(10), completion: Some(2), total: Some(20), ..Default::default() });
        s.add(&UsageCounts { prompt: Some(5), completion: Some(1), ..Default::default() });
        s.add(&UsageCounts::default());
        s.add(&UsageCounts { completion: Some(7), ..Default::default() });
        assert_eq!((s.prompt, s.completion, s.total), (15, 10, 33), "a call with no known total adds the halves it reported (#3067)");
        assert_eq!(s.reasoning, None, "no call reported reasoning");
        s.add(&UsageCounts { reasoning: Some(0), ..Default::default() });
        assert_eq!(s.reasoning, Some(0), "a reported 0 is a 0");
    }
}
