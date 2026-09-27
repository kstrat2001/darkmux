//! Trajectories of the retired openclaw runtime (removed in #1405).
//!
//! The one place that shape is known. An openclaw run wrote one
//! `prompt.submitted` per turn, carrying the whole message thread, and
//! recorded a compaction only as a `compactionSummary` message inside that
//! thread (repeated on every later turn). Nothing current writes this; a
//! handful of old lab run directories still hold it.

use serde::{Deserialize, Serialize};

/// `prompt.submitted`: one openclaw turn.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct PromptSubmitted {
    pub data: PromptData,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct PromptData {
    pub messages: Vec<PromptMessage>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct PromptMessage {
    pub role: String,
    /// `null` on every role but `compactionSummary`.
    pub summary: Option<String>,
    #[serde(rename = "tokensBefore")]
    pub tokens_before: Option<u64>,
}

const COMPACTION_SUMMARY_ROLE: &str = "compactionSummary";

/// How many leading chars identify one compaction summary across the turns
/// that repeat it.
const SUMMARY_KEY_CHARS: usize = 80;

/// One compaction an openclaw thread recorded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LegacyCompaction {
    pub tokens_before: u64,
    pub summary_chars: u64,
}

/// What an openclaw trajectory says: its turns and its distinct
/// compactions.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LegacyFold {
    pub turns: u32,
    pub compactions: Vec<LegacyCompaction>,
    seen_summaries: std::collections::BTreeSet<String>,
}

impl LegacyFold {
    pub fn apply(&mut self, e: &PromptSubmitted) {
        self.turns = self.turns.saturating_add(1);
        for m in &e.data.messages {
            let Some(summary) = m.summary.as_deref().filter(|s| !s.is_empty()) else { continue };
            if m.role != COMPACTION_SUMMARY_ROLE {
                continue;
            }
            let key: String = summary.chars().take(SUMMARY_KEY_CHARS).collect();
            if self.seen_summaries.insert(key) {
                self.compactions.push(LegacyCompaction {
                    tokens_before: m.tokens_before.unwrap_or(0),
                    summary_chars: summary.len() as u64,
                });
            }
        }
    }
}
