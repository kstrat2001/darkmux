//! Token counts: the usage block a trajectory event carries, and the one
//! reading of any reply's counts.

use serde::{Deserialize, Serialize};

/// The usage block of a `model.completed` or `compaction.call` event, as the
/// runtime wrote it from the endpoint's reply.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
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

/// The token counts one model call reported. Every field is tri-state:
/// `None` means the provider did not say, never zero.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct UsageCounts {
    pub prompt: Option<u64>,
    pub completion: Option<u64>,
    pub total: Option<u64>,
    pub reasoning: Option<u64>,
    pub cached: Option<u64>,
}

impl UsageCounts {
    /// The counts of an optional usage block: no block is nothing reported.
    pub fn of(usage: Option<&Usage>) -> Self {
        usage.map(Usage::counts).unwrap_or_default()
    }

    /// True when the reply carried a usage block this call can count.
    pub fn reported(&self) -> bool {
        self.prompt.is_some() || self.completion.is_some() || self.total.is_some()
    }

    /// THE total of one call: the provider's own total when it sent one,
    /// else prompt + completion when it reported a split, else `None`.
    /// Arithmetic on reported numbers only. The usage record, every token
    /// sum and a step's budget settle all read this.
    pub fn total_tokens(&self) -> Option<u64> {
        self.total.or(match (self.prompt, self.completion) {
            (None, None) => None,
            (p, c) => Some(p.unwrap_or(0).saturating_add(c.unwrap_or(0))),
        })
    }
}

/// A running sum of [`UsageCounts`], one call at a time.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct TokenSum {
    pub prompt: u64,
    pub completion: u64,
    /// Each call's [`UsageCounts::total_tokens`], summed: never recomputed
    /// as prompt + completion over the whole run.
    pub total: u64,
    /// `None` until a call reports the field.
    pub reasoning: Option<u64>,
    pub cached: Option<u64>,
}

impl TokenSum {
    pub fn add(&mut self, c: &UsageCounts) {
        self.prompt = self.prompt.saturating_add(c.prompt.unwrap_or(0));
        self.completion = self.completion.saturating_add(c.completion.unwrap_or(0));
        self.total = self.total.saturating_add(c.total_tokens().unwrap_or(0));
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

    #[test]
    fn the_provider_total_wins_and_the_split_is_only_the_fallback() {
        let c = UsageCounts { prompt: Some(9970), completion: Some(128), total: Some(11598), ..Default::default() };
        assert_eq!(c.total_tokens(), Some(11598), "a provider total above prompt + completion stands");
        let split = UsageCounts { prompt: Some(30), completion: Some(12), ..Default::default() };
        assert_eq!(split.total_tokens(), Some(42));
        let half = UsageCounts { completion: Some(12), ..Default::default() };
        assert_eq!(half.total_tokens(), Some(12));
        assert_eq!(UsageCounts::default().total_tokens(), None, "nothing reported is no total, not 0");
        assert!(!UsageCounts::default().reported());
    }

    #[test]
    fn a_sum_adds_each_calls_own_total_and_keeps_details_tri_state() {
        let mut s = TokenSum::default();
        s.add(&UsageCounts { prompt: Some(10), completion: Some(2), total: Some(20), ..Default::default() });
        s.add(&UsageCounts { prompt: Some(5), completion: Some(1), ..Default::default() });
        s.add(&UsageCounts::default());
        assert_eq!((s.prompt, s.completion, s.total), (15, 3, 26));
        assert_eq!(s.reasoning, None, "no call reported reasoning");
        s.add(&UsageCounts { reasoning: Some(0), ..Default::default() });
        assert_eq!(s.reasoning, Some(0), "a reported 0 is a 0");
    }
}
