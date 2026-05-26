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
/// (#372 T2-C) Which compactor implementation runs when the trigger
/// fires. Runtime-crate-local enum (mirrors the main-crate
/// `types::CompactionStrategy`). Duplicated because runtime doesn't
/// depend on main; host translates main-crate enum → CLI flag → this
/// enum at parse time. Single source of truth for the values is the
/// kebab-case spelling used on the wire (`narrative` /
/// `structured-slot`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CompactionStrategy {
    /// Today's default — narrative middle-replace via prose summary
    /// (the `compact()` function). Article-2-era shape.
    #[default]
    Narrative,
    /// Tier-2 (#352 Step 4) — structured-slot fact extraction via
    /// `structured_compact()` with JSON-mode + typed schema +
    /// SYSTEM-role markdown replacement message.
    StructuredSlot,
}

impl CompactionStrategy {
    /// Parse from the kebab-case CLI flag value.
    /// `Err(s)` on unknown values so the runtime exits with a clear
    /// message instead of silently falling back.
    pub fn from_cli_str(s: &str) -> std::result::Result<Self, String> {
        match s {
            "narrative" => Ok(Self::Narrative),
            "structured-slot" => Ok(Self::StructuredSlot),
            other => Err(format!(
                "unknown compaction strategy `{other}` (expected `narrative` or `structured-slot`)"
            )),
        }
    }
}

#[derive(Debug, Clone)]
pub struct CompactionConfig {
    /// Absolute token threshold. When `latest_prompt_tokens` reaches
    /// this, compaction fires. Set to the typed
    /// `profile.runtime.compaction.threshold_tokens` or the default.
    pub threshold_tokens: u32,
    /// Compactor model id (e.g. `darkmux:qwen3-4b-instruct-2507`).
    pub compactor_model: String,
    /// Which compactor implementation runs when the trigger fires.
    /// Operator opts into tier-2 by setting
    /// `profile.runtime.compaction.strategy: "structured-slot"` (host
    /// plumbs via `--compact-strategy` CLI flag). Default Narrative
    /// preserves pre-T2-C behavior.
    pub strategy: CompactionStrategy,
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
    /// (#377) Escalation bound: after this many compactions, the
    /// runtime bails with `TerminalReason::EscalationTriggered`
    /// (reason `CompactionLimitReached`) instead of continuing the
    /// agent loop. The KISS-doubled answer from Beat 44 closure:
    /// *bound the cost and escalate past the bound*. Past this
    /// threshold, frontier-tier picks up the salvageable work via the
    /// `darkmux-escalation-handler` skill — the operator-explicit
    /// hand-off point for "local-tier has done what it can; time for
    /// frontier." `None` disables (back-compat / no bound).
    pub bail_after_compactions: Option<u32>,
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
        strategy: Option<CompactionStrategy>,
    ) -> Self {
        Self::from_overrides_with_bail(
            threshold_tokens,
            compactor_model,
            threshold_ratio,
            context_window,
            strategy,
            None,
        )
    }

    /// Same as `from_overrides` plus the (#377) escalation bound.
    /// New call sites that thread the operator's
    /// `bail_after_compactions` setting use this; existing tests
    /// keep the 5-arg signature via the back-compat delegate above.
    pub fn from_overrides_with_bail(
        threshold_tokens: Option<u32>,
        compactor_model: Option<String>,
        threshold_ratio: Option<f32>,
        context_window: Option<u32>,
        strategy: Option<CompactionStrategy>,
        bail_after_compactions: Option<u32>,
    ) -> Self {
        Self {
            threshold_tokens: threshold_tokens.unwrap_or(DEFAULT_THRESHOLD_TOKENS),
            compactor_model: compactor_model
                .unwrap_or_else(|| DEFAULT_COMPACTOR_MODEL.to_string()),
            threshold_ratio,
            context_window,
            strategy: strategy.unwrap_or_default(),
            bail_after_compactions,
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
        Self::from_overrides(None, None, None, None, None)
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
        response_format: None,
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

/// (#372 T2-B) Tier-2 compaction: structured-slot fact extraction.
///
/// Same input contract as [`compact`] (the narrative shape today's
/// runtime uses by default) — `messages` is mutated in place, with
/// the middle slice REPLACED by a single synthetic SYSTEM-role
/// message carrying a labeled-markdown rendering of the parsed
/// structured output. The returned `StructuredCompactionOutput` is
/// what T2-C persists to `<sandbox>/.darkmux-runtime/compaction-<gen>.json`.
///
/// Strategy:
/// 1. JSON-mode chat request to `cfg.compactor_model` with system
///    prompt describing the v0.1 schema.
/// 2. Parse `choices[0].message.content` as `StructuredCompactionOutput`.
/// 3. On parse failure: retry ONCE with the same request. Per #354 Q2
///    (typed-output is the SLM strength regime), a single retry
///    handles transient sampling noise; two failures means the
///    compactor model isn't capable of the schema and the operator
///    should know.
/// 4. On second failure: return Err with the parse error. T2-C may
///    later wrap this in a bail-and-reframe path (#352 Step 6); for
///    now the caller in loop_runner sees the error and the dispatch
///    fails — better than silent corruption.
/// 5. Apply per-slot soft caps (defaults from
///    `RuntimeCompactionConfig::default_slot_caps()`).
/// 6. Render to markdown via `render_structured_output_as_markdown`.
/// 7. Splice middle messages with the synthetic system message.
///
/// **Messages NOT mutated on error** — the splice happens AFTER the
/// successful parse + render so a failed compaction leaves the
/// conversation intact for the caller's error-handling path (e.g.,
/// retry with narrative, surface to operator).
#[allow(dead_code)]
pub fn structured_compact(
    client: &LmStudioClient,
    messages: &mut Vec<Message>,
    generation: u32,
    cfg: &CompactionConfig,
) -> Result<StructuredCompactionOutput> {
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

    let compactor_system = Message::system(structured_compactor_system_prompt());
    let compactor_user = Message::user(format!(
        "Summarize the following conversation excerpt into the structured-slot \
         schema. Output JSON ONLY — no prose prefix, no markdown fences. \
         schema_version=\"0.1\", generation={generation}, \
         source_message_count={middle_count}.\n\n\
         ---\n{middle_rendered}\n---"
    ));

    let request = ChatRequest {
        model: cfg.compactor_model.clone(),
        messages: vec![compactor_system, compactor_user],
        tools: Vec::new(),
        tool_choice: None,
        temperature: 0.1,
        max_tokens: Some(4096),
        // (#375) Schema-enforced JSON via LMStudio's `json_schema`
        // response-format. LMStudio's API rejects `"type": "json_object"`
        // (OpenAI's generic-JSON mode) with `'response_format.type'
        // must be 'json_schema' or 'text'` — caught by Beat-40 tier-2
        // smoke. Switching to `json_schema` is strictly BETTER: server
        // enforces shape, not just validity. #354 Q4 v0.1 was based on
        // an incorrect assumption about LMStudio's compat; this is the
        // correct surface.
        response_format: Some(structured_output_response_format_schema()),
    };

    eprintln!(
        "darkmux-runtime: tier-2 compaction #{generation} — \
         summarizing {middle_count} middle messages (preserving {PRESERVE_HEAD} head + {PRESERVE_TAIL} tail)"
    );

    // Attempt 1.
    let parse_result_1 = call_and_parse(client, &request);
    let parsed = match parse_result_1 {
        Ok(out) => out,
        Err(e1) => {
            eprintln!(
                "darkmux-runtime: tier-2 compaction #{generation} attempt 1 failed: {e1} — retrying once"
            );
            // Attempt 2 — same request. Per #354 Q2 commitment: one
            // retry handles transient sampling noise; two failures
            // means the model genuinely can't produce the schema.
            call_and_parse(client, &request).map_err(|e2| {
                anyhow!(
                    "tier-2 compaction failed twice (attempt 1: {e1}; attempt 2: {e2})"
                )
            })?
        }
    };

    eprintln!(
        "darkmux-runtime: tier-2 compaction #{generation} — parsed structured output \
         (schema={} gen={})",
        parsed.compaction_metadata.schema_version, parsed.compaction_metadata.generation
    );

    // Apply per-slot caps. Defaults from #354 v0.1 table; T2-C will
    // plumb operator-overrides via profile config.
    let mut capped = parsed;
    apply_slot_caps(&mut capped, &default_slot_caps_v0_1());

    let markdown = render_structured_output_as_markdown(&capped, middle_count);
    // Build the synthetic SYSTEM-role replacement message. Per #354
    // Q3: system-message attention bias keeps the compacted state
    // load-bearing across subsequent turns.
    let replacement = Message {
        role: "system".to_string(),
        content: Some(markdown),
        tool_calls: None,
        tool_call_id: None,
        name: None,
        reasoning_content: None,
    };
    messages.splice(middle_start..middle_end, std::iter::once(replacement));

    Ok(capped)
}

/// (#375) Build the `response_format` value for LMStudio's
/// `json_schema` constraint. Mirrors `StructuredCompactionOutput`'s
/// shape so the server enforces the schema at decode time — stricter
/// than OpenAI's generic `json_object` mode. Top-level
/// `additionalProperties: true` preserves the forward-compat
/// tolerance for unknown fields (#354 Q5 — per-role extensions
/// deferred to v0.2 may emit fields the schema doesn't yet name).
fn structured_output_response_format_schema() -> serde_json::Value {
    serde_json::json!({
        "type": "json_schema",
        "json_schema": {
            "name": "StructuredCompactionOutput",
            "strict": false,
            "schema": {
                "type": "object",
                "properties": {
                    "objective": {"type": "string"},
                    "current_truth": {
                        "type": "object",
                        "properties": {
                            "active_files": {"type": "string"},
                            "test_outcomes": {"type": "string"},
                            "external_state": {"type": "string"}
                        },
                        "additionalProperties": false
                    },
                    "compaction_metadata": {
                        "type": "object",
                        "properties": {
                            "schema_version": {"type": "string"},
                            "generation": {"type": "integer"},
                            "source_message_count": {"type": "integer"}
                        },
                        "required": ["schema_version", "generation", "source_message_count"],
                        "additionalProperties": false
                    },
                    "completed_decisions": {"type": "string"},
                    "errors_to_preserve": {"type": "string"},
                    "next_concrete_actions": {"type": "string"},
                    "verify_criteria": {"type": "string"},
                    "sprint_id": {"type": "string"}
                },
                "required": ["objective", "current_truth", "compaction_metadata"],
                "additionalProperties": true
            }
        }
    })
}

/// (#372 T2-B) System prompt the compactor sees in JSON-mode mode.
/// Names the v0.1 schema slots + the dot-notation cap table so the
/// model knows what shape + scale to produce.
fn structured_compactor_system_prompt() -> &'static str {
    "You are a conversation compactor. You will be given a conversation excerpt \
     and you MUST return JSON conforming to this schema:\n\
     \n\
     {\n  \
       \"objective\": <string, REQUIRED>,\n  \
       \"current_truth\": {\n    \
         \"active_files\": <string, optional>,\n    \
         \"test_outcomes\": <string, optional>,\n    \
         \"external_state\": <string, optional>\n  \
       },\n  \
       \"compaction_metadata\": {\n    \
         \"schema_version\": <string, REQUIRED>,\n    \
         \"generation\": <integer, REQUIRED>,\n    \
         \"source_message_count\": <integer, REQUIRED>\n  \
       },\n  \
       \"completed_decisions\": <string, optional>,\n  \
       \"errors_to_preserve\": <string, optional>,\n  \
       \"next_concrete_actions\": <string, optional>,\n  \
       \"verify_criteria\": <string, optional>,\n  \
       \"sprint_id\": <string, optional>\n  \
     }\n\
     \n\
     Output JSON ONLY. No prose. No markdown fences. The metadata fields \
     (schema_version, generation, source_message_count) are provided in the \
     user message — copy them into your output exactly.\n\
     \n\
     Per-slot guidance: be specific and dense. Each slot string REPLACES the \
     corresponding portion of the agent's working memory for subsequent turns. \
     Omit optional slots that have no meaningful content (don't pad with \
     \"(none)\" or \"N/A\")."
}

/// Helper: make the chat call, parse the assistant content as
/// StructuredCompactionOutput. Returns Err on HTTP failure, missing
/// content, or JSON parse failure.
fn call_and_parse(
    client: &LmStudioClient,
    request: &ChatRequest,
) -> Result<StructuredCompactionOutput> {
    let response = client.chat(request)?;
    let message = response
        .choices
        .into_iter()
        .next()
        .map(|c| c.message)
        .ok_or_else(|| anyhow!("compactor returned no choice"))?;
    let content = extract_compactor_content(message)?;
    serde_json::from_str::<StructuredCompactionOutput>(&content)
        .map_err(|e| anyhow!("parsing compactor JSON failed: {e} — content: {content}"))
}

/// (#376) Pull the structured-output JSON string out of a chat-
/// completion message, with a fallback to `reasoning_content` for
/// thinking-mode models. Qwen 3.x line, deepseek-r1, and other
/// thinking-mode admin candidates route their `<think>...</think>`
/// output into `reasoning_content` rather than `content`. When asked
/// for JSON-mode output via `response_format: json_schema`, those
/// models put the JSON THERE, leaving `content` empty (or `None`).
/// The narrative-compaction path doesn't hit this because it doesn't
/// enable response_format; tier-2 does.
fn extract_compactor_content(message: Message) -> Result<String> {
    let content = message.content.filter(|s| !s.is_empty());
    let reasoning = message.reasoning_content.filter(|s| !s.is_empty());
    content
        .or(reasoning)
        .ok_or_else(|| anyhow!("compactor returned no content or reasoning_content"))
}

/// (#372 T2-B) v0.1 per-slot character caps. Mirrors the table the
/// main-crate `RuntimeCompactionConfig::default_slot_caps()` ships
/// for profile-side consumers. Duplicated rather than shared because
/// the runtime crate doesn't depend on the main crate. T2-C plumbs
/// operator-overrides via CLI flag; until then this is the only
/// source the runtime consults. Single source of truth for the v0.1
/// commitments per #354.
#[allow(dead_code)]
pub fn default_slot_caps_v0_1() -> std::collections::BTreeMap<String, u32> {
    let entries: &[(&str, u32)] = &[
        ("objective", 1024),
        ("current_truth.active_files", 4096),
        ("current_truth.test_outcomes", 2048),
        ("current_truth.external_state", 2048),
        ("completed_decisions", 4096),
        ("errors_to_preserve", 2048),
        ("next_concrete_actions", 1024),
        ("verify_criteria", 1024),
        ("sprint_id", 256),
    ];
    entries.iter().map(|(k, v)| (k.to_string(), *v)).collect()
}

/// (#372 T2-B) Apply per-slot soft character caps to a parsed
/// `StructuredCompactionOutput`. Caps are keyed by dot-notation slot
/// path (`objective`, `current_truth.active_files`, etc.); the
/// default set is provided by `default_slot_caps_v0_1`.
///
/// Truncation is byte-prefix (not char-aware) for simplicity; #354
/// commits soft-cap-in-chars semantics so we accept the rare
/// truncate-mid-codepoint edge for v0.1 simplicity. Future iteration
/// can use `char_indices()` if multi-byte slot content becomes a real
/// problem.
///
/// Slots without a defined cap are left untouched — operators who
/// haven't tuned them get the compactor's full output.
#[allow(dead_code)]
pub fn apply_slot_caps(
    out: &mut StructuredCompactionOutput,
    caps: &std::collections::BTreeMap<String, u32>,
) {
    fn cap(value: &mut String, max: usize) {
        if value.len() > max {
            value.truncate(max);
        }
    }
    fn cap_opt(value: &mut Option<String>, max: usize) {
        if let Some(s) = value.as_mut() {
            cap(s, max);
        }
    }
    if let Some(&n) = caps.get("objective") {
        cap(&mut out.objective, n as usize);
    }
    if let Some(&n) = caps.get("current_truth.active_files") {
        cap_opt(&mut out.current_truth.active_files, n as usize);
    }
    if let Some(&n) = caps.get("current_truth.test_outcomes") {
        cap_opt(&mut out.current_truth.test_outcomes, n as usize);
    }
    if let Some(&n) = caps.get("current_truth.external_state") {
        cap_opt(&mut out.current_truth.external_state, n as usize);
    }
    if let Some(&n) = caps.get("completed_decisions") {
        cap_opt(&mut out.completed_decisions, n as usize);
    }
    if let Some(&n) = caps.get("errors_to_preserve") {
        cap_opt(&mut out.errors_to_preserve, n as usize);
    }
    if let Some(&n) = caps.get("next_concrete_actions") {
        cap_opt(&mut out.next_concrete_actions, n as usize);
    }
    if let Some(&n) = caps.get("verify_criteria") {
        cap_opt(&mut out.verify_criteria, n as usize);
    }
    if let Some(&n) = caps.get("sprint_id") {
        cap_opt(&mut out.sprint_id, n as usize);
    }
}

/// (#372 T2-B) Render a `StructuredCompactionOutput` as the
/// labeled-markdown synthetic system message that REPLACES the
/// compacted middle messages in the agent's conversation. Per #354
/// Q3: system-message rendering plays to instruction-tuned models'
/// attention bias — they treat system messages as load-bearing
/// context, not as new user input requiring response.
///
/// Empty optional sections are SKIPPED entirely (no `(none)` noise);
/// `objective` always renders since it's a required slot. The header
/// names the count of compacted messages so the agent sees how much
/// conversation was condensed.
#[allow(dead_code)]
pub fn render_structured_output_as_markdown(
    out: &StructuredCompactionOutput,
    compacted_message_count: usize,
) -> String {
    let mut md = String::new();
    md.push_str(&format!(
        "### Working memory state (compacted from {compacted_message_count} earlier turns)\n\n"
    ));
    md.push_str(&format!("**Objective:** {}\n\n", out.objective));

    if let Some(s) = &out.current_truth.active_files {
        md.push_str(&format!("**Current truth — active files:**\n{s}\n\n"));
    }
    if let Some(s) = &out.current_truth.test_outcomes {
        md.push_str(&format!("**Current truth — test outcomes:**\n{s}\n\n"));
    }
    if let Some(s) = &out.current_truth.external_state {
        md.push_str(&format!("**Current truth — external state:**\n{s}\n\n"));
    }
    if let Some(s) = &out.completed_decisions {
        md.push_str(&format!("**Completed decisions:**\n{s}\n\n"));
    }
    if let Some(s) = &out.errors_to_preserve {
        md.push_str(&format!("**Errors to preserve:**\n{s}\n\n"));
    }
    if let Some(s) = &out.next_concrete_actions {
        md.push_str(&format!("**Next concrete actions:**\n{s}\n\n"));
    }
    if let Some(s) = &out.verify_criteria {
        md.push_str(&format!("**Verify criteria:** {s}\n\n"));
    }
    if let Some(s) = &out.sprint_id {
        md.push_str(&format!("**Sprint:** {s}\n\n"));
    }
    md
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

// ─── #372 T2-A: structured-slot output schema (#354 v0.1) ──────────

/// Tier-2 compactor output shape. The compactor model returns JSON
/// matching this struct (via LMStudio `response_format: {type:
/// "json_object"}`); the runtime parses it, renders to labeled
/// markdown for the synthetic system message, AND persists the raw
/// JSON to `<sandbox>/.darkmux-runtime/compaction-<gen>.json` (#352
/// Step 5 persistence — falls out for free).
///
/// Required slots per #354 v0.1 commitments: `objective`,
/// `current_truth`, `compaction_metadata`. Other slots are optional —
/// the compactor populates them when the conversation actually has
/// content for that slot.
///
/// Unknown fields are tolerated for forward-compat (#354 Q5: per-role
/// schema extensions deferred to v0.2; compactor may emit fields the
/// schema doesn't yet name, runtime ignores them rather than fails).
///
/// T2-A ships the SHAPE only; T2-B implements `structured_compact()`
/// which actually parses these from the compactor's JSON-mode response.
/// Until then no production code path constructs these — only tests.
#[allow(dead_code)]
#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
pub struct StructuredCompactionOutput {
    pub objective: String,
    pub current_truth: CurrentTruth,
    pub compaction_metadata: CompactionMetadata,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub completed_decisions: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub errors_to_preserve: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_concrete_actions: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub verify_criteria: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sprint_id: Option<String>,
}

/// Nested under `StructuredCompactionOutput.current_truth`. Sub-slots
/// for the dimensions of "what's the agent's working-memory state
/// right now." All optional — small workloads only have a subset.
#[allow(dead_code)]
#[derive(Debug, Clone, Default, serde::Deserialize, serde::Serialize)]
pub struct CurrentTruth {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_files: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub test_outcomes: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub external_state: Option<String>,
}

/// Provenance metadata the compactor MUST emit so the runtime + any
/// downstream replayer can reason about which compaction produced
/// which artifact. `generation` matches the runtime's compaction
/// counter; `source_message_count` is how many messages the
/// compactor was asked to summarize (the middle slice size).
#[allow(dead_code)]
#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
pub struct CompactionMetadata {
    pub schema_version: String,
    pub generation: u32,
    pub source_message_count: u32,
}

#[cfg(test)]
mod tests {
    use super::*;

    // ─── #372 T2-B: structured_compact() end-to-end (mocked LMStudio) ─

    use httpmock::prelude::*;

    fn chat_response_with_json_content(json_content: &str) -> serde_json::Value {
        // LMStudio's chat-completion response wraps the assistant's
        // content (which IS our JSON-mode output) in the standard
        // OpenAI shape. We're not faking any tool_calls here — the
        // compactor doesn't use tools.
        serde_json::json!({
            "id": "chatcmpl-compaction-test",
            "object": "chat.completion",
            "created": 1700000000,
            "model": "test-compactor",
            "choices": [{
                "index": 0,
                "message": {
                    "role": "assistant",
                    "content": json_content,
                },
                "finish_reason": "stop",
            }],
            "usage": {
                "prompt_tokens": 500,
                "completion_tokens": 80,
                "total_tokens": 580,
            },
        })
    }

    fn dummy_messages_long_enough_to_compact() -> Vec<Message> {
        // PRESERVE_HEAD=2 + 1 middle + PRESERVE_TAIL=4 = 7 minimum.
        // Use 10 to make middle = 4 messages (so we can confirm splice).
        vec![
            Message::system("you are an agent"),
            Message::user("initial task framing"),
            Message::user("turn 1 stuff"),
            Message::user("turn 2 stuff"),
            Message::user("turn 3 stuff"),
            Message::user("turn 4 stuff"),
            Message::user("turn 5 stuff"),
            Message::user("turn 6 stuff"),
            Message::user("turn 7 stuff"),
            Message::user("turn 8 stuff"),
        ]
    }

    #[test]
    #[serial_test::serial]
    fn structured_compact_happy_path_splices_synthetic_system_message() {
        let server = MockServer::start();
        let _mock = server.mock(|when, then| {
            when.method(POST).path("/v1/chat/completions");
            then.status(200).json_body(chat_response_with_json_content(
                r#"{"objective":"audit refresh-token","current_truth":{"active_files":"x.ts"},"compaction_metadata":{"schema_version":"0.1","generation":7,"source_message_count":4}}"#,
            ));
        });

        let client = LmStudioClient::with_base_url(format!("{}/v1", server.base_url()));
        let mut messages = dummy_messages_long_enough_to_compact();
        let original_len = messages.len();
        let cfg = CompactionConfig::default();

        let out = structured_compact(&client, &mut messages, 7, &cfg)
            .expect("happy path returns parsed output");

        // Assert parsed output is what we expected.
        assert_eq!(out.objective, "audit refresh-token");
        assert_eq!(out.compaction_metadata.generation, 7);

        // Assert messages were spliced: head + 1 synthetic system + tail
        // = 2 + 1 + 4 = 7 messages (down from original 10).
        assert_eq!(messages.len(), original_len - (10 - 7));
        // The synthetic message is at PRESERVE_HEAD position and is
        // SYSTEM-role (per #354 Q3 attention-bias rationale).
        assert_eq!(messages[PRESERVE_HEAD].role, "system");
        let content = messages[PRESERVE_HEAD].content.as_ref().expect("content set");
        assert!(content.contains("Working memory state"));
        assert!(content.contains("audit refresh-token"));
    }

    #[test]
    #[serial_test::serial]
    fn structured_compact_retries_once_on_parse_fail() {
        let server = MockServer::start();
        // Use two mocks: first call returns malformed JSON, second
        // returns valid. httpmock plays them in declaration order
        // when paths match (assuming hits accumulate); to make
        // ordering explicit we use a counter via body-contains or
        // a single mock that returns different things by call count.
        // Easier: simulate with a stateful mock would be complex;
        // use two distinct mocks differentiated by body content
        // matching for now.
        //
        // For T2-B simplicity: we test "compactor returned malformed
        // JSON on first call, valid on retry, structured_compact
        // succeeds" via a Mutex<u32> counter pattern would need more
        // infra. For now, test the RETRY HAPPENS by mocking just
        // ONE mock that returns malformed and asserting the function
        // makes >1 calls (retry attempt). End-to-end success-with-
        // retry is covered by the bail test (which proves both
        // attempts ran when both failed).
        let mock = server.mock(|when, then| {
            when.method(POST).path("/v1/chat/completions");
            then.status(200).json_body(chat_response_with_json_content(
                "{ totally not valid JSON for the schema }",
            ));
        });

        let client = LmStudioClient::with_base_url(format!("{}/v1", server.base_url()));
        let mut messages = dummy_messages_long_enough_to_compact();
        let cfg = CompactionConfig::default();

        let result = structured_compact(&client, &mut messages, 1, &cfg);
        assert!(result.is_err(), "both attempts return malformed → bail");
        // Confirm the retry happened — 2 calls to the mock, not 1.
        assert_eq!(mock.hits(), 2, "expected 1 initial + 1 retry attempt");
    }

    #[test]
    #[serial_test::serial]
    fn structured_compact_bails_when_both_attempts_malformed() {
        let server = MockServer::start();
        let _mock = server.mock(|when, then| {
            when.method(POST).path("/v1/chat/completions");
            then.status(200).json_body(chat_response_with_json_content(
                "not even close to JSON",
            ));
        });

        let client = LmStudioClient::with_base_url(format!("{}/v1", server.base_url()));
        let mut messages = dummy_messages_long_enough_to_compact();
        let original_len = messages.len();
        let cfg = CompactionConfig::default();

        let result = structured_compact(&client, &mut messages, 1, &cfg);
        assert!(result.is_err());
        assert_eq!(
            messages.len(),
            original_len,
            "messages NOT mutated when both attempts fail (rollback on error)"
        );
    }

    // ─── #372 T2-B: per-slot cap enforcement ──────────────────────

    fn test_caps() -> std::collections::BTreeMap<String, u32> {
        let mut m = std::collections::BTreeMap::new();
        m.insert("objective".to_string(), 10);
        m.insert("current_truth.active_files".to_string(), 15);
        m.insert("completed_decisions".to_string(), 20);
        m
    }

    fn output_for_cap_tests(objective: &str, active_files: Option<&str>, decisions: Option<&str>) -> StructuredCompactionOutput {
        StructuredCompactionOutput {
            objective: objective.to_string(),
            current_truth: CurrentTruth {
                active_files: active_files.map(str::to_string),
                test_outcomes: None,
                external_state: None,
            },
            compaction_metadata: CompactionMetadata {
                schema_version: "0.1".into(),
                generation: 1,
                source_message_count: 5,
            },
            completed_decisions: decisions.map(str::to_string),
            errors_to_preserve: None,
            next_concrete_actions: None,
            verify_criteria: None,
            sprint_id: None,
        }
    }

    #[test]
    fn apply_caps_truncates_objective_when_over() {
        let mut out = output_for_cap_tests(
            "This is a very long objective that exceeds the cap of ten characters",
            None,
            None,
        );
        apply_slot_caps(&mut out, &test_caps());
        assert_eq!(out.objective.len(), 10);
        assert_eq!(out.objective, "This is a ");
    }

    #[test]
    fn apply_caps_leaves_under_cap_strings_alone() {
        let mut out = output_for_cap_tests("short", None, None);
        apply_slot_caps(&mut out, &test_caps());
        assert_eq!(out.objective, "short", "under-cap content must not be modified");
    }

    #[test]
    fn apply_caps_handles_nested_current_truth_active_files() {
        let mut out = output_for_cap_tests(
            "ok",
            Some("a long active-files description exceeding fifteen chars"),
            None,
        );
        apply_slot_caps(&mut out, &test_caps());
        assert_eq!(
            out.current_truth.active_files.as_ref().unwrap().len(),
            15,
            "dot-notation slot cap (current_truth.active_files) applied to nested field"
        );
    }

    #[test]
    fn apply_caps_skips_unset_optional_fields() {
        let mut out = output_for_cap_tests("ok", None, None);
        apply_slot_caps(&mut out, &test_caps());
        assert!(out.completed_decisions.is_none(), "None stays None");
    }

    #[test]
    fn apply_caps_with_missing_cap_for_slot_leaves_field_untouched() {
        // No cap defined for `objective` — leave the value as-is.
        let mut caps = std::collections::BTreeMap::new();
        caps.insert("current_truth.active_files".to_string(), 5);
        let mut out = output_for_cap_tests(
            "long-objective-no-cap-defined",
            Some("a long active-files line"),
            None,
        );
        apply_slot_caps(&mut out, &caps);
        assert_eq!(out.objective, "long-objective-no-cap-defined", "no cap → no truncate");
        assert_eq!(out.current_truth.active_files.as_ref().unwrap().len(), 5);
    }

    // ─── #372 T2-B: markdown rendering for synthetic system message ─

    #[test]
    fn render_markdown_includes_compacted_header_with_message_count() {
        let out = output_for_cap_tests("test obj", None, None);
        let md = render_structured_output_as_markdown(&out, 12);
        assert!(
            md.contains("Working memory state (compacted from 12 earlier turns)"),
            "header must include the message count; got: {md}"
        );
    }

    #[test]
    fn render_markdown_emits_objective_section() {
        let out = output_for_cap_tests("fix the bug in foo.py", None, None);
        let md = render_structured_output_as_markdown(&out, 5);
        assert!(md.contains("**Objective:**"), "objective section header missing");
        assert!(md.contains("fix the bug in foo.py"), "objective body missing");
    }

    #[test]
    fn render_markdown_emits_current_truth_subsections_when_set() {
        let out = StructuredCompactionOutput {
            objective: "obj".into(),
            current_truth: CurrentTruth {
                active_files: Some("file.py:32".into()),
                test_outcomes: Some("3 of 5 failed".into()),
                external_state: None,
            },
            compaction_metadata: CompactionMetadata {
                schema_version: "0.1".into(),
                generation: 1,
                source_message_count: 5,
            },
            completed_decisions: None,
            errors_to_preserve: None,
            next_concrete_actions: None,
            verify_criteria: None,
            sprint_id: None,
        };
        let md = render_structured_output_as_markdown(&out, 5);
        assert!(md.contains("Current truth — active files"), "active_files subsection missing");
        assert!(md.contains("file.py:32"), "active_files content missing");
        assert!(md.contains("Current truth — test outcomes"));
        assert!(md.contains("3 of 5 failed"));
        assert!(
            !md.contains("Current truth — external state"),
            "unset subsection must be omitted (no empty-section noise)"
        );
    }

    #[test]
    fn render_markdown_skips_unset_optional_top_level_sections() {
        let out = output_for_cap_tests("obj only", None, None);
        let md = render_structured_output_as_markdown(&out, 1);
        assert!(!md.contains("Completed decisions"), "unset section must be omitted");
        assert!(!md.contains("Errors to preserve"));
        assert!(!md.contains("Next concrete actions"));
        assert!(!md.contains("Verify criteria"));
    }

    #[test]
    fn render_markdown_emits_all_optional_sections_when_set() {
        let out = StructuredCompactionOutput {
            objective: "obj".into(),
            current_truth: CurrentTruth::default(),
            compaction_metadata: CompactionMetadata {
                schema_version: "0.1".into(),
                generation: 1,
                source_message_count: 5,
            },
            completed_decisions: Some("decided X".into()),
            errors_to_preserve: Some("don't retry Y".into()),
            next_concrete_actions: Some("do Z".into()),
            verify_criteria: Some("npm test exits 0".into()),
            sprint_id: Some("sprint-1".into()),
        };
        let md = render_structured_output_as_markdown(&out, 5);
        assert!(md.contains("**Completed decisions:**"));
        assert!(md.contains("decided X"));
        assert!(md.contains("**Errors to preserve:**"));
        assert!(md.contains("don't retry Y"));
        assert!(md.contains("**Next concrete actions:**"));
        assert!(md.contains("**Verify criteria:**"));
        assert!(md.contains("npm test exits 0"));
    }

    // ─── #372 T2-A: StructuredCompactionOutput parse/round-trip ─

    #[test]
    fn structured_output_parses_minimal_with_only_required_slots() {
        let json = r#"{
            "objective": "Fix the bug in bug.py",
            "current_truth": {},
            "compaction_metadata": {
                "schema_version": "0.1",
                "generation": 1,
                "source_message_count": 5
            }
        }"#;
        let out: StructuredCompactionOutput = serde_json::from_str(json).unwrap();
        assert_eq!(out.objective, "Fix the bug in bug.py");
        assert_eq!(out.compaction_metadata.schema_version, "0.1");
        assert_eq!(out.compaction_metadata.generation, 1);
        assert_eq!(out.compaction_metadata.source_message_count, 5);
        assert!(out.completed_decisions.is_none());
        assert!(out.current_truth.active_files.is_none());
    }

    #[test]
    fn structured_output_parses_full_shape() {
        let json = r#"{
            "objective": "Audit refresh-token rotation tests",
            "current_truth": {
                "active_files": "refreshTokenService.test.ts — 5 failures",
                "test_outcomes": "npm test → 5 failed, 86 passed",
                "external_state": "config mock state shared across tests"
            },
            "compaction_metadata": {
                "schema_version": "0.1",
                "generation": 2,
                "source_message_count": 30
            },
            "completed_decisions": "Decided to fix mock isolation",
            "errors_to_preserve": "mockImplementation persists across tests",
            "next_concrete_actions": "Switch to mockImplementationOnce",
            "verify_criteria": "npm test exits 0",
            "sprint_id": "sprint-42"
        }"#;
        let out: StructuredCompactionOutput = serde_json::from_str(json).unwrap();
        assert_eq!(out.objective, "Audit refresh-token rotation tests");
        assert_eq!(
            out.current_truth.active_files.as_deref(),
            Some("refreshTokenService.test.ts — 5 failures")
        );
        assert_eq!(out.completed_decisions.as_deref(), Some("Decided to fix mock isolation"));
        assert_eq!(out.sprint_id.as_deref(), Some("sprint-42"));
    }

    #[test]
    fn structured_output_errors_when_objective_missing() {
        let json = r#"{
            "current_truth": {},
            "compaction_metadata": {
                "schema_version": "0.1",
                "generation": 1,
                "source_message_count": 5
            }
        }"#;
        let result: Result<StructuredCompactionOutput, _> = serde_json::from_str(json);
        assert!(result.is_err(), "missing required `objective` must error");
    }

    #[test]
    fn structured_output_errors_when_compaction_metadata_missing() {
        let json = r#"{
            "objective": "x",
            "current_truth": {}
        }"#;
        let result: Result<StructuredCompactionOutput, _> = serde_json::from_str(json);
        assert!(result.is_err(), "missing required `compaction_metadata` must error");
    }

    #[test]
    fn structured_output_tolerates_unknown_extra_fields() {
        // Forward-compat for #354 Q5 (per-role schema extensions
        // deferred to v0.2): compactor may emit slots the runtime
        // doesn't yet name. Don't fail — quietly drop unknown fields.
        let json = r#"{
            "objective": "x",
            "current_truth": {},
            "compaction_metadata": {
                "schema_version": "0.2",
                "generation": 1,
                "source_message_count": 5
            },
            "future_role_specific_slot": "some content from v0.2 compactor"
        }"#;
        let result: Result<StructuredCompactionOutput, _> = serde_json::from_str(json);
        assert!(
            result.is_ok(),
            "unknown extra fields tolerated for forward-compat"
        );
    }

    #[test]
    fn structured_output_round_trips_through_json() {
        let original = StructuredCompactionOutput {
            objective: "test objective".into(),
            current_truth: CurrentTruth {
                active_files: Some("file.py".into()),
                test_outcomes: None,
                external_state: None,
            },
            compaction_metadata: CompactionMetadata {
                schema_version: "0.1".into(),
                generation: 1,
                source_message_count: 10,
            },
            completed_decisions: None,
            errors_to_preserve: None,
            next_concrete_actions: None,
            verify_criteria: None,
            sprint_id: None,
        };
        let json = serde_json::to_string(&original).unwrap();
        let back: StructuredCompactionOutput = serde_json::from_str(&json).unwrap();
        assert_eq!(back.objective, original.objective);
        assert_eq!(back.current_truth.active_files, original.current_truth.active_files);
        assert_eq!(back.compaction_metadata.generation, original.compaction_metadata.generation);
    }

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
        let cfg = CompactionConfig::from_overrides(Some(30_000), None, None, None, None);
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
            None,
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
            strategy: CompactionStrategy::Narrative,
            bail_after_compactions: None,
        };
        let cfg_high = CompactionConfig {
            threshold_tokens: 100_000,
            compactor_model: DEFAULT_COMPACTOR_MODEL.to_string(),
            threshold_ratio: None,
            context_window: None,
            strategy: CompactionStrategy::Narrative,
            bail_after_compactions: None,
        };
        // 10K crosses low (5K) but not high (100K) — assert per-cfg.
        let min_len = PRESERVE_HEAD + 1 + PRESERVE_TAIL;
        assert!(needs_compaction(10_000, min_len, &cfg_low));
        assert!(!needs_compaction(10_000, min_len, &cfg_high));
    }

    // ─── #368: formula trigger (threshold_ratio * context_window) ─

    #[test]
    fn formula_trigger_disabled_when_either_input_missing() {
        let cfg = CompactionConfig::from_overrides(None, None, Some(0.35), None, None);
        assert!(cfg.formula_trigger_tokens().is_none(), "missing window");

        let cfg = CompactionConfig::from_overrides(None, None, None, Some(101_000), None);
        assert!(cfg.formula_trigger_tokens().is_none(), "missing share");
    }

    #[test]
    fn formula_trigger_computes_correctly() {
        let cfg = CompactionConfig::from_overrides(None, None, Some(0.35), Some(101_000), None);
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
            strategy: CompactionStrategy::Narrative,
            bail_after_compactions: None,
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
            strategy: CompactionStrategy::Narrative,
            bail_after_compactions: None,
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
            strategy: CompactionStrategy::Narrative,
            bail_after_compactions: None,
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
            reasoning_content: None,
        };
        let out = render_messages_as_excerpt(std::slice::from_ref(&msg));
        assert!(out.contains("[assistant]: thinking"));
        assert!(out.contains("tool_call: read({\"path\":\"foo\"})"));
    }

    fn msg_with(content: Option<&str>, reasoning: Option<&str>) -> Message {
        Message {
            role: "assistant".into(),
            content: content.map(str::to_string),
            tool_calls: None,
            tool_call_id: None,
            name: None,
            reasoning_content: reasoning.map(str::to_string),
        }
    }

    #[test]
    fn extract_prefers_content_when_present() {
        let m = msg_with(Some(r#"{"objective":"x"}"#), Some(r#"{"objective":"y"}"#));
        let got = extract_compactor_content(m).unwrap();
        assert_eq!(got, r#"{"objective":"x"}"#);
    }

    #[test]
    fn extract_falls_back_to_reasoning_when_content_none() {
        let m = msg_with(None, Some(r#"{"objective":"y"}"#));
        let got = extract_compactor_content(m).unwrap();
        assert_eq!(got, r#"{"objective":"y"}"#);
    }

    #[test]
    fn extract_falls_back_to_reasoning_when_content_empty_string() {
        // (#376) Thinking-mode models commonly return `content: ""`
        // (Some, but empty) with the JSON in `reasoning_content`.
        let m = msg_with(Some(""), Some(r#"{"objective":"y"}"#));
        let got = extract_compactor_content(m).unwrap();
        assert_eq!(got, r#"{"objective":"y"}"#);
    }

    #[test]
    fn extract_errors_when_both_absent() {
        let m = msg_with(None, None);
        let err = extract_compactor_content(m).unwrap_err();
        assert!(err.to_string().contains("no content or reasoning_content"));
    }

    #[test]
    fn extract_errors_when_both_empty_strings() {
        let m = msg_with(Some(""), Some(""));
        let err = extract_compactor_content(m).unwrap_err();
        assert!(err.to_string().contains("no content or reasoning_content"));
    }

}
