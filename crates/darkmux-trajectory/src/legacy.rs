//! Trajectories of the retired openclaw runtime (removed in #1405).
//!
//! The one place that shape is known. An openclaw run wrote one
//! `prompt.submitted` per turn, carrying the whole message thread, and
//! recorded a compaction only as a `compactionSummary` message inside that
//! thread (repeated on every later turn). Its own `model.completed` lines
//! carry the call's usage as `data.usage` (`input`/`output`/`total`) under
//! an ISO-string clock. Every line names `"traceSchema":
//! "openclaw-trajectory"`. Nothing current writes this; a handful of old lab
//! run directories still hold it.

use serde::{Deserialize, Serialize};

const TRACE_SCHEMA: &str = "openclaw-trajectory";
const PROMPT_SUBMITTED: &str = "prompt.submitted";

/// One line of the retired format, as the fold reads it.
#[derive(Debug, Clone, PartialEq)]
pub enum LegacyEvent {
    /// One openclaw turn.
    PromptSubmitted(PromptSubmitted),
    /// One openclaw model call's usage.
    ModelCompleted(crate::UsageCounts),
    /// Anything else the format wrote (session, trace and context lines).
    Other,
}

/// Cheap pre-check before [`parse`]: false for every line a current runtime
/// writes (a current line carrying one of these strings in its text is
/// re-checked by [`parse`], which returns `None` for it).
pub fn may_be_legacy(line: &str) -> bool {
    line.contains(TRACE_SCHEMA) || line.contains(PROMPT_SUBMITTED)
}

/// A line of the retired format, or `None` when the line is not one: it
/// neither names the openclaw `traceSchema` nor is a `prompt.submitted`
/// (a type only openclaw wrote). Lenient per field: a turn counts even if
/// its thread cannot be read, and a count that is not a whole number reads
/// as unreported.
pub fn parse(line: &str) -> Option<LegacyEvent> {
    let v: serde_json::Value = serde_json::from_str(line).ok()?;
    let ty = v.get("type").and_then(serde_json::Value::as_str);
    let openclaw = v.get("traceSchema").and_then(serde_json::Value::as_str) == Some(TRACE_SCHEMA);
    if !openclaw && ty != Some(PROMPT_SUBMITTED) {
        return None;
    }
    Some(match ty {
        Some(PROMPT_SUBMITTED) => LegacyEvent::PromptSubmitted(PromptSubmitted {
            data: PromptData {
                messages: v
                    .pointer("/data/messages")
                    .and_then(|m| serde_json::from_value(m.clone()).ok())
                    .unwrap_or_default(),
            },
        }),
        Some("model.completed") => {
            let count = |k: &str| v.pointer(&format!("/data/usage/{k}")).and_then(serde_json::Value::as_u64);
            LegacyEvent::ModelCompleted(crate::UsageCounts {
                prompt: count("input"),
                completion: count("output"),
                total: count("total"),
                ..Default::default()
            })
        }
        _ => LegacyEvent::Other,
    })
}

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
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LegacyCompaction {
    /// The 1-based turn whose thread first carried the summary.
    pub turn: u32,
    /// The prompt-token count the compaction was triggered at.
    pub tokens_before: u64,
    /// The summary the compactor wrote.
    pub summary: String,
}

impl LegacyCompaction {
    /// The summary's length in chars (never bytes: multi-byte text would
    /// over-report, #906).
    pub fn summary_chars(&self) -> u64 {
        self.summary.chars().count() as u64
    }
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
                    turn: self.turns,
                    tokens_before: m.tokens_before.unwrap_or(0),
                    summary: summary.to_string(),
                });
            }
        }
    }
}
