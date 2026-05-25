//! Compaction strategy.
//!
//! When the conversation's prompt-token count approaches the model's
//! loaded context window, the loop pauses to compact the middle of the
//! conversation — summarizing it via a small companion model and
//! replacing the summarized turns with a single synthetic user message.
//!
//! ## Architectural choices
//!
//! - **Middle-replace, not sliding-window.** Article 2's empirical
//!   finding: keeping the system prompt stable across compactions is
//!   critical for KV cache reuse. Middle-replace preserves the system
//!   prompt; sliding-window would drop earliest messages and break
//!   the prefix.
//! - **Keep last 4 messages uncompacted.** Preserves the active
//!   tool-call/tool-result thread and the last assistant turn so the
//!   model has immediate context for what's happening next.
//! - **Companion compactor model.** Same approach openclaw uses — a
//!   small 4B model summarizes via a separate chat-completion call
//!   with no tools.
//! - **Token-count-aware trigger, not heuristic percentage.** The
//!   threshold compares the chat-completion API's reported
//!   `usage.prompt_tokens` against an absolute number — no guessing
//!   what the model's "believed" context window is, the way openclaw's
//!   heuristic-percentage approach did per Article 2 findings.
//! - **Config via host-passed CLI args, not env vars (#368).** All
//!   tunables (threshold, compactor model, optional
//!   threshold_ratio) arrive as explicit parameters from the host's
//!   `dispatch_via_internal`, which derives them from
//!   `profile.runtime.compaction.*` (the typed schema landed in #357).
//!   No `std::env::var()` reads in this module — operator's tuning
//!   surface is the profile JSON, not shell env. Env vars are a
//!   hurdle, brittle, and ergonomically wrong for per-profile state.
//!
//! ## Deliberately NOT done in this phase
//!
//! - No tool-call elision (a cheaper alternative that drops
//!   intermediate tool results but keeps tool calls + assistant
//!   reasoning). Could be added later if measurements show summarize
//!   is too expensive.

use anyhow::{anyhow, Result};

use crate::lmstudio::{ChatRequest, LmStudioClient, Message};

/// Default threshold: when an incoming prompt's `prompt_tokens`
/// crosses this, compact before the next chat() call. 60K leaves
/// substantial headroom for the model's response generation when
/// loaded at 101K (the `balanced` profile context). Override via
/// `profile.runtime.compaction.threshold_tokens` (typed v0.1 field
/// landed in #357) → host passes as `--compact-threshold-tokens N`.
pub const DEFAULT_THRESHOLD_TOKENS: u32 = 60_000;

/// Default compactor model — the bake-off-hired admin agent
/// (Beat 21: "the dependable admin agent"). Same model openclaw uses
/// by convention. Override via `profile.runtime.compaction.extras.model`
/// (openclaw-shape passthrough that survived the #357 typed schema)
/// → host passes as `--compactor-model <id>`.
pub const DEFAULT_COMPACTOR_MODEL: &str = "darkmux:qwen3-4b-instruct-2507";

/// Number of trailing messages to preserve uncompacted. Keeps the
/// active tool-call / tool-result thread + the last assistant turn.
const PRESERVE_TAIL: usize = 4;

/// Number of leading messages to preserve uncompacted. Keeps the
/// system prompt + the initial user message (the task framing). Both
/// are load-bearing for the agent to know what it's doing.
const PRESERVE_HEAD: usize = 2;

/// Operator-tunable compaction config. Constructed on the host from
/// `profile.runtime.compaction.*` and passed into the runtime as
/// explicit CLI args (#368) — replaces the pre-fix env-var read path
/// that silently dropped operator overrides inside the docker
/// container. Single source of truth for compaction parameters that
/// the loop runner consults at trigger-check time.
#[derive(Debug, Clone)]
pub struct CompactionConfig {
    /// Absolute token threshold. When `latest_prompt_tokens` reaches
    /// this, compaction fires. Set to the typed
    /// `profile.runtime.compaction.threshold_tokens` or the default.
    pub threshold_tokens: u32,
    /// Compactor model id (e.g. `darkmux:qwen3-4b-instruct-2507`).
    pub compactor_model: String,
    /// Optional fraction-of-context-window trigger (0.1-0.9 per
    /// openclaw's range). When set AND `context_window` is also set,
    /// compaction fires when `latest_prompt_tokens >= context_window *
    /// threshold_ratio` — independent of `threshold_tokens`.
    /// Either trigger fires first wins.
    ///
    /// Mirrors openclaw's `agents.defaults.compaction.maxHistoryShare`
    /// (operator passthrough field in #357 `extras` map). The formula
    /// trigger is the load-bearing answer to operator's #368 critique:
    /// "absolute tokens is brittle — fractions adapt across loads."
    pub threshold_ratio: Option<f32>,
    /// Loaded model's context window in tokens (e.g. 101000 for the
    /// `balanced` profile's D primary). Needed to compute the
    /// `threshold_ratio` formula trigger. Host derives from the
    /// active profile's primary model `n_ctx`. `None` disables the
    /// formula trigger entirely (back-compat / absolute-only mode).
    pub context_window: Option<u32>,
}

impl CompactionConfig {
    /// Resolve a CompactionConfig from optional explicit overrides.
    /// `None` for any field uses the corresponding default (or
    /// disables the optional trigger). This is the host's choice
    /// point — it reads the operator's profile and passes `Some(v)`
    /// when set, `None` when not.
    pub fn from_overrides(
        threshold_tokens: Option<u32>,
        compactor_model: Option<String>,
        threshold_ratio: Option<f32>,
        context_window: Option<u32>,
    ) -> Self {
        Self {
            threshold_tokens: threshold_tokens.unwrap_or(DEFAULT_THRESHOLD_TOKENS),
            compactor_model: compactor_model
                .unwrap_or_else(|| DEFAULT_COMPACTOR_MODEL.to_string()),
            threshold_ratio,
            context_window,
        }
    }

    /// Compute the formula-trigger threshold in tokens, if both
    /// `threshold_ratio` and `context_window` are configured.
    /// Returns `None` when the formula trigger is disabled (either
    /// input absent). The result is the prompt-token level at which
    /// the formula trigger would fire.
    pub fn formula_trigger_tokens(&self) -> Option<u32> {
        match (self.threshold_ratio, self.context_window) {
            (Some(share), Some(window)) => {
                let trigger = (window as f32) * share;
                Some(trigger.floor() as u32)
            }
            _ => None,
        }
    }
}

impl Default for CompactionConfig {
    fn default() -> Self {
        Self::from_overrides(None, None, None, None)
    }
}

/// Decide whether the conversation needs compaction before the next
/// chat() call. True when:
/// - `latest_prompt_tokens` crossed the configured threshold (the most
///   recent request's input was big), AND
/// - The conversation is long enough that middle-replace has
///   something meaningful to replace (head + 1 middle + tail).
pub fn needs_compaction(
    latest_prompt_tokens: u32,
    message_count: usize,
    cfg: &CompactionConfig,
) -> bool {
    if !conversation_long_enough_to_compact(message_count) {
        return false;
    }
    // Two independent triggers; either fires first wins.
    let absolute_tripped = latest_prompt_tokens >= cfg.threshold_tokens;
    let formula_tripped = cfg
        .formula_trigger_tokens()
        .is_some_and(|t| latest_prompt_tokens >= t);
    absolute_tripped || formula_tripped
}

/// Sanity-check: middle-replace needs `head + 1 middle + tail`
/// messages to have something meaningful to replace. The compact()
/// pre-flight uses this independently of any token threshold.
fn conversation_long_enough_to_compact(message_count: usize) -> bool {
    message_count >= PRESERVE_HEAD + 1 + PRESERVE_TAIL
}

/// Apply middle-replace compaction in-place on `messages`.
///
/// Sends `messages[PRESERVE_HEAD .. n - PRESERVE_TAIL]` to the
/// compactor model with a "summarize this excerpt" prompt, then
/// replaces that slice with a single synthetic user message
/// containing the summary tagged `[compacted:<generation>]`.
pub fn compact(
    client: &LmStudioClient,
    messages: &mut Vec<Message>,
    generation: u32,
    cfg: &CompactionConfig,
) -> Result<()> {
    let n = messages.len();
    if !conversation_long_enough_to_compact(n) {
        return Err(anyhow!(
            "conversation too short to compact: {n} messages \
             (need >= {} for middle-replace)",
            PRESERVE_HEAD + 1 + PRESERVE_TAIL
        ));
    }

    let middle_start = PRESERVE_HEAD;
    let middle_end = n - PRESERVE_TAIL;
    let middle_count = middle_end - middle_start;

    let middle_messages: Vec<Message> = messages[middle_start..middle_end].to_vec();
    let middle_rendered = render_messages_as_excerpt(&middle_messages);

    let compactor_system = Message::system(
        "You are a conversation compactor. Read the conversation excerpt below \
         and produce a concise summary that preserves: (1) what's been decided, \
         (2) what tools have been called and what they returned, (3) what state \
         changes have been made (files written, commands run, edits applied), \
         (4) what's currently in progress. Be specific and dense — this summary \
         REPLACES the excerpt in the agent's working memory. Do not editorialize. \
         Do not add framing language. Just the summary.",
    );
    let compactor_user = Message::user(format!(
        "Summarize the following conversation excerpt:\n\n\
         ---\n{middle_rendered}\n---\n\nSummary:"
    ));

    let request = ChatRequest {
        model: cfg.compactor_model.clone(),
        messages: vec![compactor_system, compactor_user],
        tools: Vec::new(),
        tool_choice: None,
        temperature: 0.1,
        max_tokens: Some(4096),
    };

    eprintln!(
        "darkmux-runtime: compaction #{generation} — summarizing {middle_count} middle messages \
         (preserving {PRESERVE_HEAD} head + {PRESERVE_TAIL} tail)"
    );

    let response = client.chat(&request)?;
    let summary = response
        .choices
        .into_iter()
        .next()
        .and_then(|c| c.message.content)
        .ok_or_else(|| anyhow!("compactor model returned no content"))?;

    eprintln!(
        "darkmux-runtime: compaction #{generation} — summary {} chars",
        summary.len()
    );

    let replacement = Message::user(format!("[compacted:{generation}] {summary}"));
    messages.splice(middle_start..middle_end, std::iter::once(replacement));

    Ok(())
}

/// Render a slice of messages as a plaintext excerpt the compactor
/// can read. Each message gets a `[role]:` prefix and tool_calls are
/// serialized in a human-readable form. Tool results are tagged with
/// the tool name.
fn render_messages_as_excerpt(messages: &[Message]) -> String {
    let mut out = String::new();
    for m in messages {
        out.push_str(&format!("[{}]: ", m.role));
        if let Some(content) = &m.content {
            out.push_str(content);
        }
        if let Some(calls) = &m.tool_calls {
            for c in calls {
                out.push_str(&format!(
                    "\n  tool_call: {}({})",
                    c.function.name, c.function.arguments
                ));
            }
        }
        if let Some(name) = &m.name {
            out.push_str(&format!("\n  (tool result for: {name})"));
        }
        out.push_str("\n\n");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    // ─── #368: explicit-param CompactionConfig (no env-var reads) ─

    /// Defaults preserve pre-#368 behavior: 60K threshold + canonical
    /// 4B compactor. Operator overrides arrive via host's profile
    /// JSON → CLI flags, not env vars.
    #[test]
    fn default_cfg_matches_pre_368_defaults() {
        let cfg = CompactionConfig::default();
        assert_eq!(cfg.threshold_tokens, DEFAULT_THRESHOLD_TOKENS);
        assert_eq!(cfg.compactor_model, DEFAULT_COMPACTOR_MODEL);
    }

    #[test]
    fn from_overrides_threshold_only_uses_default_model() {
        let cfg = CompactionConfig::from_overrides(Some(30_000), None, None, None);
        assert_eq!(cfg.threshold_tokens, 30_000);
        assert_eq!(cfg.compactor_model, DEFAULT_COMPACTOR_MODEL);
        assert!(cfg.threshold_ratio.is_none());
        assert!(cfg.context_window.is_none());
    }

    #[test]
    fn from_overrides_model_only_uses_default_threshold() {
        let cfg = CompactionConfig::from_overrides(
            None,
            Some("custom-compactor".to_string()),
            None,
            None,
        );
        assert_eq!(cfg.threshold_tokens, DEFAULT_THRESHOLD_TOKENS);
        assert_eq!(cfg.compactor_model, "custom-compactor");
    }

    #[test]
    fn from_overrides_all_set() {
        let cfg = CompactionConfig::from_overrides(
            Some(45_000),
            Some("alt-compactor".to_string()),
            Some(0.35),
            Some(101_000),
        );
        assert_eq!(cfg.threshold_tokens, 45_000);
        assert_eq!(cfg.compactor_model, "alt-compactor");
        assert_eq!(cfg.threshold_ratio, Some(0.35));
        assert_eq!(cfg.context_window, Some(101_000));
    }

    #[test]
    fn needs_compaction_requires_both_size_and_length() {
        let cfg = CompactionConfig::default();
        // Tokens below threshold → no compaction even with many messages
        assert!(!needs_compaction(1000, 100, &cfg));
        // Tokens above threshold but conversation too short
        assert!(!needs_compaction(100_000, 5, &cfg));
        // Both conditions met
        assert!(needs_compaction(100_000, 10, &cfg));
    }

    #[test]
    fn needs_compaction_boundary() {
        let cfg = CompactionConfig::default();
        // Exactly at threshold + minimum length
        let min_len = PRESERVE_HEAD + 1 + PRESERVE_TAIL;
        assert!(needs_compaction(DEFAULT_THRESHOLD_TOKENS, min_len, &cfg));
        // One below threshold
        assert!(!needs_compaction(DEFAULT_THRESHOLD_TOKENS - 1, min_len, &cfg));
        // Length-1 below
        assert!(!needs_compaction(DEFAULT_THRESHOLD_TOKENS, min_len - 1, &cfg));
    }

    #[test]
    fn needs_compaction_uses_per_cfg_threshold() {
        let cfg_low = CompactionConfig {
            threshold_tokens: 5_000,
            compactor_model: DEFAULT_COMPACTOR_MODEL.to_string(),
            threshold_ratio: None,
            context_window: None,
        };
        let cfg_high = CompactionConfig {
            threshold_tokens: 100_000,
            compactor_model: DEFAULT_COMPACTOR_MODEL.to_string(),
            threshold_ratio: None,
            context_window: None,
        };
        // 10K crosses low (5K) but not high (100K) — assert per-cfg.
        let min_len = PRESERVE_HEAD + 1 + PRESERVE_TAIL;
        assert!(needs_compaction(10_000, min_len, &cfg_low));
        assert!(!needs_compaction(10_000, min_len, &cfg_high));
    }

    // ─── #368: formula trigger (threshold_ratio * context_window) ─

    #[test]
    fn formula_trigger_disabled_when_either_input_missing() {
        let cfg = CompactionConfig::from_overrides(None, None, Some(0.35), None);
        assert!(cfg.formula_trigger_tokens().is_none(), "missing window");

        let cfg = CompactionConfig::from_overrides(None, None, None, Some(101_000));
        assert!(cfg.formula_trigger_tokens().is_none(), "missing share");
    }

    #[test]
    fn formula_trigger_computes_correctly() {
        let cfg = CompactionConfig::from_overrides(None, None, Some(0.35), Some(101_000));
        // 101000 * 0.35 = 35350
        assert_eq!(cfg.formula_trigger_tokens(), Some(35_350));
    }

    #[test]
    fn formula_trigger_fires_independently_of_absolute() {
        // Absolute threshold deliberately high (won't trip); formula
        // (0.35 of 100K = 35K) trips at 36K prompt tokens.
        let cfg = CompactionConfig {
            threshold_tokens: 60_000,
            compactor_model: DEFAULT_COMPACTOR_MODEL.to_string(),
            threshold_ratio: Some(0.35),
            context_window: Some(100_000),
        };
        let min_len = PRESERVE_HEAD + 1 + PRESERVE_TAIL;
        // 36K > 35K formula trigger but < 60K absolute → still fires.
        assert!(needs_compaction(36_000, min_len, &cfg));
        // 34K below formula trigger AND below absolute → does NOT fire.
        assert!(!needs_compaction(34_000, min_len, &cfg));
    }

    #[test]
    fn absolute_trigger_fires_independently_of_formula() {
        // Formula trigger high (60K of 100K = 60K, requires 60K
        // prompt). Absolute lower at 40K — trips first.
        let cfg = CompactionConfig {
            threshold_tokens: 40_000,
            compactor_model: DEFAULT_COMPACTOR_MODEL.to_string(),
            threshold_ratio: Some(0.6),
            context_window: Some(100_000),
        };
        let min_len = PRESERVE_HEAD + 1 + PRESERVE_TAIL;
        // 45K > 40K absolute (formula trigger is 60K, untouched).
        assert!(needs_compaction(45_000, min_len, &cfg));
    }

    #[test]
    fn either_trigger_fires_whichever_is_first() {
        // Both triggers configured; whichever has the LOWER threshold
        // fires first. With absolute=50K and formula=35K (0.35*100K),
        // the formula's lower trigger should fire at 36K.
        let cfg = CompactionConfig {
            threshold_tokens: 50_000,
            compactor_model: DEFAULT_COMPACTOR_MODEL.to_string(),
            threshold_ratio: Some(0.35),
            context_window: Some(100_000),
        };
        let min_len = PRESERVE_HEAD + 1 + PRESERVE_TAIL;
        assert!(needs_compaction(36_000, min_len, &cfg));
    }

    #[test]
    fn render_excerpt_preserves_role_and_content() {
        let msgs = vec![
            Message::user("hello"),
            Message::system("system-text"),
        ];
        let out = render_messages_as_excerpt(&msgs);
        assert!(out.contains("[user]: hello"));
        assert!(out.contains("[system]: system-text"));
    }

    #[test]
    fn render_excerpt_includes_tool_calls() {
        let msg = Message {
            role: "assistant".into(),
            content: Some("thinking".into()),
            tool_calls: Some(vec![crate::lmstudio::ToolCall {
                id: "abc".into(),
                kind: "function".into(),
                function: crate::lmstudio::FunctionCall {
                    name: "read".into(),
                    arguments: r#"{"path":"foo"}"#.into(),
                },
            }]),
            tool_call_id: None,
            name: None,
        };
        let out = render_messages_as_excerpt(std::slice::from_ref(&msg));
        assert!(out.contains("[assistant]: thinking"));
        assert!(out.contains("tool_call: read({\"path\":\"foo\"})"));
    }
}
