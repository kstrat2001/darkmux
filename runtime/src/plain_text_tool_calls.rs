//! Plain-text tool-call promoter (#406).
//!
//! Recovers tool calls that LMStudio failed to extract from model
//! output into the structured `tool_calls` field. Mirrors openclaw's
//! `promoteLmstudioPlainTextToolCalls` pattern (see openclaw
//! `extensions/lmstudio/src/plain-text-tool-calls.ts` + the parser at
//! `src/plugin-sdk/tool-payload.ts`).
//!
//! Three plain-text tool-call formats this parser recognizes:
//!
//! 1. **Bracket format** (OpenAI-style):
//!    `[NAME]\n{json}\n[END_TOOL_REQUEST]` or `[NAME]\n{json}\n[/NAME]`
//! 2. **Harmony format** (gpt-oss-120b, contributed to openclaw by the
//!    operator 2026-05-06, featured in Article 1):
//!    `<|channel|>commentary to=NAME code <|message|>{json}<|call|>`
//!    (with variable presence of channel marker, message marker, call
//!    marker)
//! 3. **XML format** (Qwen 3.x thinking-mode — the gap this module
//!    addresses; not yet in openclaw):
//!    `<tool_call><function=NAME><parameter=key>value</parameter></function></tool_call>`
//!
//! The XML format is what Qwen 3.x emits when the model has entered
//! thinking mode mid-loop — the call lands in `reasoning_content`
//! rather than `content`. LMStudio's response handler doesn't surface
//! it to the structured `tool_calls` field. Without the promoter, the
//! runtime sees `tool_calls=null`, `finish_reason=stop`, and exits the
//! agent loop silently — the 20% bail rate observed in Beat 46 V4 N=5
//! (#405).
//!
//! ## Channel fallback
//!
//! `promote_plain_text_tool_calls()` scans `content` first (the
//! openclaw case for non-thinking models), falls back to
//! `reasoning_content` if `content` is empty (the darkmux extension
//! for Qwen 3.x thinking-mode). This is the same fallback pattern as
//! `extract_compactor_content()` in compaction.rs (PR #376).

use std::collections::HashSet;

use crate::lmstudio::{FunctionCall, Message, ToolCall};

/// Per-call cap on the plain-text JSON payload size we'll attempt to
/// parse. Mirrors openclaw's `DEFAULT_MAX_PLAIN_TEXT_TOOL_PAYLOAD_BYTES`.
/// Defends against pathological model output that fills tokens with
/// brace-balanced garbage.
pub const DEFAULT_MAX_PAYLOAD_BYTES: usize = 256_000;

const END_TOOL_REQUEST: &str = "[END_TOOL_REQUEST]";
const HARMONY_CHANNEL_MARKER: &str = "<|channel|>";
const HARMONY_MESSAGE_MARKER: &str = "<|message|>";
const HARMONY_CALL_MARKER: &str = "<|call|>";

const XML_TOOL_CALL_OPEN: &str = "<tool_call>";
const XML_TOOL_CALL_CLOSE: &str = "</tool_call>";
const XML_FUNCTION_OPEN: &str = "<function=";
const XML_FUNCTION_CLOSE: &str = "</function>";
const XML_PARAMETER_OPEN: &str = "<parameter=";
const XML_PARAMETER_CLOSE: &str = "</parameter>";

/// A parsed plain-text tool call from the model's output.
#[derive(Debug, Clone, PartialEq)]
pub struct PlainTextToolCallBlock {
    pub name: String,
    pub arguments: serde_json::Map<String, serde_json::Value>,
    /// Byte offsets into the source text the block spans (start..end).
    pub start: usize,
    pub end: usize,
}

/// Which text channel the promoter found the tool-call markup in.
/// Surfaces in observability so operators can see the rate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PromotionSource {
    /// Promoted from `message.content` — the openclaw case (non-thinking models).
    Content,
    /// Promoted from `message.reasoning_content` — the Qwen 3.x
    /// thinking-mode case (#405; the darkmux extension over openclaw).
    Reasoning,
}

impl PromotionSource {
    pub fn as_str(self) -> &'static str {
        match self {
            PromotionSource::Content => "content",
            PromotionSource::Reasoning => "reasoning",
        }
    }
}

/// Which textual format the parser recognized. Surfaces in trajectory
/// so operators can split the openclaw-class cases (bracket / harmony)
/// from the Qwen 3.x thinking-mode case (xml).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PromotionFormat {
    /// Bracket-format (`[NAME]\n{...}\n[END_TOOL_REQUEST]`) or harmony-
    /// format (`<|channel|>commentary to=NAME code <|message|>{...}`).
    /// Lumped because openclaw's parser allows mixed blocks in the
    /// same text and the two share the JSON-payload consumer; the
    /// distinction matters less than the openclaw-vs-darkmux extension.
    BracketOrHarmony,
    /// XML-format
    /// (`<tool_call><function=NAME><parameter=KEY>VAL</parameter></function></tool_call>`).
    /// The Qwen 3.x thinking-mode case — the darkmux extension over openclaw.
    Xml,
}

impl PromotionFormat {
    pub fn as_str(self) -> &'static str {
        match self {
            PromotionFormat::BracketOrHarmony => "bracket-or-harmony",
            PromotionFormat::Xml => "xml",
        }
    }
}

/// Result of a successful promotion — describes both which channel
/// the markup was found in and which format the parser recognized.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PromotionInfo {
    pub source: PromotionSource,
    pub format: PromotionFormat,
    pub call_count: usize,
}

/// Promote plain-text tool calls in a chat-completion response message
/// to the structured `tool_calls` field.
///
/// Returns `Some(PromotionInfo)` when promotion fired, indicating
/// which channel the markup was found in, which format was used,
/// and how many tool calls were recovered. Returns `None` when:
/// - `tool_calls` is already populated (model used the structured field)
/// - Neither `content` nor `reasoning_content` parses as plain-text tool
///   calls
/// - The parsed tool name is not in `allowed_tool_names`
///
/// On successful promotion, the source channel (`content` or
/// `reasoning_content`) is cleared — the raw text was the tool-call
/// markup itself, now structured. Same behavior as openclaw's
/// `promoteLmstudioPlainTextToolCalls`.
pub fn promote_plain_text_tool_calls(
    message: &mut Message,
    allowed_tool_names: &HashSet<String>,
) -> Option<PromotionInfo> {
    // (#406) Mirror openclaw's `wrapLmstudioPlainTextToolCalls`
    // invariant: refuse to promote when the caller hasn't enumerated
    // allowed tool names. The empty-set case is the only way an
    // adversarial / hallucinated tool name could otherwise slip
    // through the gate inside the per-format parsers (which permit
    // anything when the set is empty). The runtime never legitimately
    // calls in with an empty set — but defending here keeps the
    // contract self-evident.
    if allowed_tool_names.is_empty() {
        return None;
    }
    if message
        .tool_calls
        .as_ref()
        .map(|tc| !tc.is_empty())
        .unwrap_or(false)
    {
        return None;
    }

    let content_to_try = message
        .content
        .as_deref()
        .filter(|s| !s.trim().is_empty())
        .map(|s| (s.to_string(), PromotionSource::Content))
        .into_iter()
        .chain(
            message
                .reasoning_content
                .as_deref()
                .filter(|s| !s.trim().is_empty())
                .map(|s| (s.to_string(), PromotionSource::Reasoning)),
        );

    for (text, source) in content_to_try {
        if let Some((blocks, format)) = parse_plain_text_tool_call_blocks(
            &text,
            allowed_tool_names,
            DEFAULT_MAX_PAYLOAD_BYTES,
        ) {
            let call_count = blocks.len();
            apply_promotion(message, blocks, source);
            return Some(PromotionInfo {
                source,
                format,
                call_count,
            });
        }
    }
    None
}

fn apply_promotion(
    message: &mut Message,
    blocks: Vec<PlainTextToolCallBlock>,
    source: PromotionSource,
) {
    let tool_calls: Vec<ToolCall> = blocks
        .into_iter()
        .map(|block| ToolCall {
            id: synthesize_id(),
            kind: "function".to_string(),
            function: FunctionCall {
                name: block.name,
                arguments: serde_json::Value::Object(block.arguments).to_string(),
            },
        })
        .collect();
    message.tool_calls = Some(tool_calls);
    // Clear the source channel — the text was the tool-call markup
    // itself, now structured. Same behavior as openclaw's promoter.
    match source {
        PromotionSource::Content => message.content = None,
        PromotionSource::Reasoning => message.reasoning_content = None,
    }
}

fn synthesize_id() -> String {
    // Mirrors openclaw's `createLmstudioSyntheticToolCallId` shape
    // (`call_<hex>`). Counter alone guarantees within-process
    // uniqueness — synthetic IDs only need to be unique within a
    // single dispatch's call sequence per the runtime's tool-call
    // bookkeeping (the runtime never indexes them across dispatches
    // or across processes). Timestamp is appended for forensic
    // legibility but isn't load-bearing. The `_` separator prevents
    // ambiguous concatenation (e.g., `(0x1, 0x23)` and `(0x12, 0x3)`
    // would otherwise both render as `call_123`).
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::SystemTime;
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let t = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    format!("call_{:x}_{:x}", t, n)
}

/// Parse `text` end-to-end as a sequence of plain-text tool-call
/// blocks. Returns `Some(blocks)` only when EVERY whitespace-separated
/// segment of `text` parses as a valid block — partial parses return
/// `None`. Mirrors openclaw's `parseStandalonePlainTextToolCallBlocks`
/// strictness.
///
/// The XML-format extension is more permissive: it scans for
/// `<tool_call>` markers anywhere in the text (since Qwen 3.x
/// thinking-mode output can have prose around the tool call). When
/// the bracket+harmony all-or-nothing parser fails, falls back to XML
/// scan-and-extract.
pub fn parse_plain_text_tool_call_blocks(
    text: &str,
    allowed_tool_names: &HashSet<String>,
    max_payload_bytes: usize,
) -> Option<(Vec<PlainTextToolCallBlock>, PromotionFormat)> {
    // Try strict bracket+harmony parse first (openclaw semantics).
    if let Some(blocks) = parse_strict_bracket_or_harmony(text, allowed_tool_names, max_payload_bytes) {
        return Some((blocks, PromotionFormat::BracketOrHarmony));
    }
    // Fall back to XML scan (Qwen 3.x).
    parse_xml_tool_calls(text, allowed_tool_names, max_payload_bytes)
        .map(|blocks| (blocks, PromotionFormat::Xml))
}

// ─── bracket + harmony parsers (port of openclaw tool-payload.ts) ──────

fn parse_strict_bracket_or_harmony(
    text: &str,
    allowed_tool_names: &HashSet<String>,
    max_payload_bytes: usize,
) -> Option<Vec<PlainTextToolCallBlock>> {
    let mut blocks = Vec::new();
    let mut cursor = skip_whitespace(text, 0);
    while cursor < text.len() {
        let block = parse_one_block_at(text, cursor, allowed_tool_names, max_payload_bytes)?;
        cursor = skip_whitespace(text, block.end);
        blocks.push(block);
    }
    if blocks.is_empty() {
        None
    } else {
        Some(blocks)
    }
}

struct Opening {
    end: usize,
    name: String,
    requires_closing: bool,
}

fn parse_one_block_at(
    text: &str,
    start: usize,
    allowed_tool_names: &HashSet<String>,
    max_payload_bytes: usize,
) -> Option<PlainTextToolCallBlock> {
    let opening = parse_bracket_opening(text, start).or_else(|| parse_harmony_opening(text, start))?;
    if !allowed_tool_names.is_empty() && !allowed_tool_names.contains(&opening.name) {
        return None;
    }
    let (json_end, json_value) = consume_json_object(text, opening.end, max_payload_bytes)?;
    let closing_end = if opening.requires_closing {
        parse_bracket_closing(text, json_end, &opening.name)?
    } else {
        parse_optional_harmony_closing(text, json_end)
    };
    Some(PlainTextToolCallBlock {
        name: opening.name,
        arguments: json_value,
        start,
        end: closing_end,
    })
}

fn parse_bracket_opening(text: &str, start: usize) -> Option<Opening> {
    let bytes = text.as_bytes();
    if bytes.get(start)? != &b'[' {
        return None;
    }
    let mut cursor = start + 1;
    let name_start = cursor;
    while cursor < bytes.len() && is_tool_name_char(bytes[cursor]) {
        cursor += 1;
    }
    if cursor == name_start || bytes.get(cursor)? != &b']' {
        return None;
    }
    let name = text[name_start..cursor].to_string();
    cursor += 1;
    cursor = skip_horizontal_whitespace(text, cursor);
    let after_line_break = consume_line_break(text, cursor)?;
    Some(Opening {
        end: after_line_break,
        name,
        requires_closing: true,
    })
}

fn parse_harmony_opening(text: &str, start: usize) -> Option<Opening> {
    let mut cursor = start;
    if text[cursor..].starts_with(HARMONY_CHANNEL_MARKER) {
        cursor += HARMONY_CHANNEL_MARKER.len();
    }
    let bytes = text.as_bytes();
    let channel_start = cursor;
    while cursor < bytes.len() && is_harmony_channel_char(bytes[cursor]) {
        cursor += 1;
    }
    let channel = &text[channel_start..cursor];
    if channel != "commentary" && channel != "analysis" && channel != "final" {
        return None;
    }
    cursor = skip_horizontal_whitespace(text, cursor);
    if !text[cursor..].starts_with("to=") {
        return None;
    }
    cursor += 3;
    let name_start = cursor;
    while cursor < bytes.len() && is_tool_name_char(bytes[cursor]) {
        cursor += 1;
    }
    if cursor == name_start {
        return None;
    }
    let name = text[name_start..cursor].to_string();
    cursor = skip_horizontal_whitespace(text, cursor);
    if !text[cursor..].starts_with("code") {
        return None;
    }
    cursor += 4;
    cursor = skip_whitespace(text, cursor);
    if text[cursor..].starts_with(HARMONY_MESSAGE_MARKER) {
        cursor = skip_whitespace(text, cursor + HARMONY_MESSAGE_MARKER.len());
    }
    Some(Opening {
        end: cursor,
        name,
        requires_closing: false,
    })
}

fn parse_bracket_closing(text: &str, start: usize, name: &str) -> Option<usize> {
    let cursor = skip_whitespace(text, start);
    if text[cursor..].starts_with(END_TOOL_REQUEST) {
        return Some(cursor + END_TOOL_REQUEST.len());
    }
    let named = format!("[/{}]", name);
    if text[cursor..].starts_with(&named) {
        return Some(cursor + named.len());
    }
    None
}

fn parse_optional_harmony_closing(text: &str, start: usize) -> usize {
    let cursor = skip_whitespace(text, start);
    if text[cursor..].starts_with(HARMONY_CALL_MARKER) {
        return cursor + HARMONY_CALL_MARKER.len();
    }
    start
}

// (#905) `max_payload_bytes` is the SAME `DEFAULT_MAX_PAYLOAD_BYTES`
// (256 KB) cap the XML-block path uses — both call sites pass the one
// constant, so the JSON-object and `<tool_call>`-block scanners are
// bounded identically. The cap is measured here against the running
// object span (`i + 1 - cursor`), not the whole input.
fn consume_json_object(
    text: &str,
    start: usize,
    max_payload_bytes: usize,
) -> Option<(usize, serde_json::Map<String, serde_json::Value>)> {
    let cursor = skip_whitespace(text, start);
    let bytes = text.as_bytes();
    if bytes.get(cursor)? != &b'{' {
        return None;
    }
    let mut depth: i32 = 0;
    let mut in_string = false;
    let mut escaped = false;
    let mut i = cursor;
    while i < bytes.len() {
        if i + 1 - cursor > max_payload_bytes {
            return None;
        }
        let c = bytes[i];
        if in_string {
            if escaped {
                escaped = false;
            } else if c == b'\\' {
                escaped = true;
            } else if c == b'"' {
                in_string = false;
            }
        } else if c == b'"' {
            in_string = true;
        } else if c == b'{' {
            depth += 1;
        } else if c == b'}' {
            depth -= 1;
            if depth == 0 {
                let raw = &text[cursor..=i];
                let parsed: serde_json::Value = serde_json::from_str(raw).ok()?;
                let obj = parsed.as_object()?.clone();
                return Some((i + 1, obj));
            }
        }
        i += 1;
    }
    None
}

// ─── XML format parser (Qwen 3.x thinking-mode — new, not in openclaw) ─

fn parse_xml_tool_calls(
    text: &str,
    allowed_tool_names: &HashSet<String>,
    max_payload_bytes: usize,
) -> Option<Vec<PlainTextToolCallBlock>> {
    let mut blocks = Vec::new();
    let mut cursor = 0;
    while let Some(open_idx) = text[cursor..].find(XML_TOOL_CALL_OPEN) {
        let block_start = cursor + open_idx;
        let payload_start = block_start + XML_TOOL_CALL_OPEN.len();
        // Bound the close-tag search by max_payload_bytes so a
        // missing close tag in adversarial input doesn't scan the
        // entire remaining text. Mirrors the per-iteration cap the
        // bracket parser enforces in `consume_json_object`.
        let scan_end = payload_start
            .saturating_add(max_payload_bytes)
            .min(text.len());
        // (#409) The structural `</tool_call>` is, by grammar, the first
        // one that appears AFTER this block's `</function>`. Anchoring on
        // `</function>` makes a literal `</tool_call>` inside a parameter
        // value safe — it lives before `</function>`, so it is never
        // mistaken for the block close. We additionally bound the search at
        // the next `<tool_call>` opener so multi-block input still splits
        // correctly. (A literal `<tool_call>` opener inside a value is the
        // residual edge this can't disambiguate without a real tokenizer;
        // models effectively never emit one.)
        let next_open = text[payload_start..scan_end]
            .find(XML_TOOL_CALL_OPEN)
            .map(|rel| payload_start + rel)
            .unwrap_or(scan_end);
        let region = &text[payload_start..next_open];
        let close_rel = match region.rfind(XML_FUNCTION_CLOSE) {
            Some(fn_close) => region[fn_close..].find(XML_TOOL_CALL_CLOSE).map(|rel| fn_close + rel),
            None => region.find(XML_TOOL_CALL_CLOSE),
        };
        // (#905) Fail soft per block: if this block's close tag can't be
        // located (the rare literal-`<tool_call>`-in-a-value edge) or its
        // payload won't parse, STOP scanning but KEEP the blocks already
        // found — a single bad block must not silently drop the whole turn's
        // tool-call promotion (the old `?` returned None for everything).
        let Some(close_rel) = close_rel else { break };
        let payload_end = payload_start + close_rel;
        let payload = &text[payload_start..payload_end];
        let Some(mut block) = parse_xml_block(payload, allowed_tool_names) else { break };
        block.start = block_start;
        block.end = payload_end + XML_TOOL_CALL_CLOSE.len();
        blocks.push(block);
        cursor = payload_end + XML_TOOL_CALL_CLOSE.len();
    }
    if blocks.is_empty() {
        None
    } else {
        Some(blocks)
    }
}

/// Parse the inside of a `<tool_call>...</tool_call>` block.
/// Expected shape:
///   `<function=NAME>`
///     `<parameter=KEY1>VALUE1</parameter>`
///     `<parameter=KEY2>VALUE2</parameter>`
///   `</function>`
///
/// VALUE may be JSON-encoded (parsed if possible, kept as string
/// otherwise — matches the Qwen 3.x convention where structured args
/// can be passed as JSON strings or scalars).
fn parse_xml_block(
    payload: &str,
    allowed_tool_names: &HashSet<String>,
) -> Option<PlainTextToolCallBlock> {
    let fn_open_idx = payload.find(XML_FUNCTION_OPEN)?;
    let name_start = fn_open_idx + XML_FUNCTION_OPEN.len();
    let name_end = payload[name_start..].find('>')?;
    let name = payload[name_start..name_start + name_end].trim().to_string();
    if !allowed_tool_names.is_empty() && !allowed_tool_names.contains(&name) {
        return None;
    }

    let params_start = name_start + name_end + 1;
    // (#409) rfind, not find: there is exactly one structural `</function>`
    // per block, so the LAST occurrence is the real one even when a
    // parameter value contains the literal `</function>` substring.
    let fn_close_idx = payload[params_start..].rfind(XML_FUNCTION_CLOSE);
    let params_end = match fn_close_idx {
        Some(idx) => params_start + idx,
        None => payload.len(),
    };
    let params_section = &payload[params_start..params_end];

    let mut arguments = serde_json::Map::new();
    let mut p_cursor = 0;
    while let Some(p_open_rel) = params_section[p_cursor..].find(XML_PARAMETER_OPEN) {
        let key_start = p_cursor + p_open_rel + XML_PARAMETER_OPEN.len();
        let key_end_rel = params_section[key_start..].find('>')?;
        let key = params_section[key_start..key_start + key_end_rel].trim().to_string();
        let value_start = key_start + key_end_rel + 1;
        // (#409) The value's closing `</parameter>` is the LAST one before
        // the next `<parameter=` opener (or before the end of the params
        // section, for the final parameter). Region-anchored rfind makes a
        // literal `</parameter>` inside the value safe: any embedded close
        // tag is an earlier occurrence than the structural one.
        let region_end = params_section[value_start..]
            .find(XML_PARAMETER_OPEN)
            .map(|rel| value_start + rel)
            .unwrap_or(params_section.len());
        let value_region = &params_section[value_start..region_end];
        let value_end_rel = value_region.rfind(XML_PARAMETER_CLOSE)?;
        let value_text = &value_region[..value_end_rel];
        let value = parse_xml_parameter_value(value_text);
        arguments.insert(key, value);
        p_cursor = value_start + value_end_rel + XML_PARAMETER_CLOSE.len();
    }

    if arguments.is_empty() {
        // Tool call with zero params is allowed by the grammar but
        // most tools require args — keep permissive; caller's
        // validation tier rejects if needed.
    }

    Some(PlainTextToolCallBlock {
        name,
        arguments,
        start: 0, // Filled by parse_xml_tool_calls.
        end: 0,
    })
}

/// Try to interpret a parameter VALUE as JSON; fall back to the raw
/// string when it's not valid JSON. Trims leading/trailing whitespace
/// since Qwen 3.x sometimes emits indented multi-line values.
fn parse_xml_parameter_value(text: &str) -> serde_json::Value {
    let trimmed = text.trim();
    // Heuristic: only try JSON parsing for things that look like JSON
    // values (start with `{`, `[`, `"`, `t/f/n/0-9`). Plain bash command
    // strings shouldn't go through serde_json (would fail on most
    // characters and waste cycles).
    let first = trimmed.chars().next();
    let looks_jsonish = matches!(
        first,
        Some('{') | Some('[') | Some('"') | Some('-')
            | Some('0'..='9') | Some('t') | Some('f') | Some('n')
    );
    if looks_jsonish {
        if let Ok(parsed) = serde_json::from_str::<serde_json::Value>(trimmed) {
            return parsed;
        }
    }
    serde_json::Value::String(text.to_string())
}

// ─── helpers ───────────────────────────────────────────────────────────

fn is_tool_name_char(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-' || byte == b'.'
}

fn is_harmony_channel_char(byte: u8) -> bool {
    byte.is_ascii_alphabetic() || byte == b'_'
}

fn skip_horizontal_whitespace(text: &str, start: usize) -> usize {
    let bytes = text.as_bytes();
    let mut i = start;
    while i < bytes.len() && (bytes[i] == b' ' || bytes[i] == b'\t') {
        i += 1;
    }
    i
}

fn skip_whitespace(text: &str, start: usize) -> usize {
    let bytes = text.as_bytes();
    let mut i = start;
    while i < bytes.len() && bytes[i].is_ascii_whitespace() {
        i += 1;
    }
    i
}

fn consume_line_break(text: &str, start: usize) -> Option<usize> {
    let bytes = text.as_bytes();
    if bytes.get(start) == Some(&b'\r') {
        if bytes.get(start + 1) == Some(&b'\n') {
            return Some(start + 2);
        }
        return Some(start + 1);
    }
    if bytes.get(start) == Some(&b'\n') {
        return Some(start + 1);
    }
    None
}

// ─── tests ─────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn allowed(names: &[&str]) -> HashSet<String> {
        names.iter().map(|s| s.to_string()).collect()
    }

    fn assert_block(block: &PlainTextToolCallBlock, name: &str, key: &str, value: &str) {
        assert_eq!(block.name, name);
        let v = block.arguments.get(key).expect("expected argument key");
        assert_eq!(v.as_str().unwrap_or_else(|| panic!("not a string: {v}")), value);
    }

    /// Discard the format tag — most parser-shape tests only care
    /// about the parsed blocks. Format-tagging is asserted explicitly
    /// in the dedicated `format_detection_*` tests further down.
    fn blocks_only(
        result: Option<(Vec<PlainTextToolCallBlock>, PromotionFormat)>,
    ) -> Option<Vec<PlainTextToolCallBlock>> {
        result.map(|(b, _)| b)
    }

    // ─── Bracket format (5 cases) ──────────────────────────────────────

    #[test]
    fn bracket_format_basic_call() {
        let text = "[bash]\n{\"command\": \"ls\"}\n[END_TOOL_REQUEST]";
        let blocks = blocks_only(parse_plain_text_tool_call_blocks(text, &allowed(&["bash"]), DEFAULT_MAX_PAYLOAD_BYTES)).unwrap();
        assert_eq!(blocks.len(), 1);
        assert_block(&blocks[0], "bash", "command", "ls");
    }

    #[test]
    fn bracket_format_named_closing() {
        let text = "[read]\n{\"path\": \"/x\"}\n[/read]";
        let blocks = blocks_only(parse_plain_text_tool_call_blocks(text, &allowed(&["read"]), DEFAULT_MAX_PAYLOAD_BYTES)).unwrap();
        assert_eq!(blocks.len(), 1);
        assert_block(&blocks[0], "read", "path", "/x");
    }

    #[test]
    fn bracket_format_multiple_calls() {
        let text = "[read]\n{\"path\":\"/a\"}\n[END_TOOL_REQUEST]\n[bash]\n{\"command\":\"pwd\"}\n[END_TOOL_REQUEST]";
        let blocks = blocks_only(parse_plain_text_tool_call_blocks(text, &allowed(&["read", "bash"]), DEFAULT_MAX_PAYLOAD_BYTES)).unwrap();
        assert_eq!(blocks.len(), 2);
        assert_block(&blocks[0], "read", "path", "/a");
        assert_block(&blocks[1], "bash", "command", "pwd");
    }

    #[test]
    fn bracket_format_disallowed_tool_returns_none() {
        let text = "[evil_tool]\n{}\n[END_TOOL_REQUEST]";
        let blocks = parse_plain_text_tool_call_blocks(text, &allowed(&["read", "bash"]), DEFAULT_MAX_PAYLOAD_BYTES);
        assert!(blocks.is_none(), "disallowed tool must reject");
    }

    #[test]
    fn bracket_format_malformed_json_returns_none() {
        let text = "[bash]\n{not valid json\n[END_TOOL_REQUEST]";
        let blocks = parse_plain_text_tool_call_blocks(text, &allowed(&["bash"]), DEFAULT_MAX_PAYLOAD_BYTES);
        assert!(blocks.is_none());
    }

    // ─── Harmony format (operator's prior contribution; ported faithfully) ──

    #[test]
    fn harmony_format_full_with_channel_marker() {
        let text = "<|channel|>commentary to=bash code <|message|>{\"command\": \"pwd\"}<|call|>";
        let blocks = blocks_only(parse_plain_text_tool_call_blocks(text, &allowed(&["bash"]), DEFAULT_MAX_PAYLOAD_BYTES)).unwrap();
        assert_eq!(blocks.len(), 1);
        assert_block(&blocks[0], "bash", "command", "pwd");
    }

    #[test]
    fn harmony_format_no_channel_marker() {
        let text = "commentary to=read code <|message|>{\"path\": \"/x\"}<|call|>";
        let blocks = blocks_only(parse_plain_text_tool_call_blocks(text, &allowed(&["read"]), DEFAULT_MAX_PAYLOAD_BYTES)).unwrap();
        assert_eq!(blocks.len(), 1);
        assert_block(&blocks[0], "read", "path", "/x");
    }

    #[test]
    fn harmony_format_no_call_marker() {
        let text = "<|channel|>commentary to=bash code <|message|>{\"command\": \"pwd\"}";
        let blocks = blocks_only(parse_plain_text_tool_call_blocks(text, &allowed(&["bash"]), DEFAULT_MAX_PAYLOAD_BYTES)).unwrap();
        assert_eq!(blocks.len(), 1);
    }

    #[test]
    fn harmony_format_analysis_channel() {
        let text = "<|channel|>analysis to=bash code <|message|>{\"command\":\"x\"}<|call|>";
        let blocks = blocks_only(parse_plain_text_tool_call_blocks(text, &allowed(&["bash"]), DEFAULT_MAX_PAYLOAD_BYTES)).unwrap();
        assert_eq!(blocks.len(), 1);
    }

    #[test]
    fn harmony_format_unknown_channel_returns_none() {
        let text = "<|channel|>weirdchannel to=bash code <|message|>{}<|call|>";
        let blocks = parse_plain_text_tool_call_blocks(text, &allowed(&["bash"]), DEFAULT_MAX_PAYLOAD_BYTES);
        assert!(blocks.is_none());
    }

    // ─── XML format — the Qwen 3.x extension (new — not in openclaw) ───

    #[test]
    fn xml_format_basic_single_call() {
        let text = "<tool_call><function=bash><parameter=command>ls -la</parameter></function></tool_call>";
        let blocks = blocks_only(parse_plain_text_tool_call_blocks(text, &allowed(&["bash"]), DEFAULT_MAX_PAYLOAD_BYTES)).unwrap();
        assert_eq!(blocks.len(), 1);
        assert_block(&blocks[0], "bash", "command", "ls -la");
    }

    #[test]
    fn xml_format_multiple_parameters() {
        let text = "<tool_call><function=read><parameter=path>/tmp/x</parameter><parameter=offset>1</parameter><parameter=limit>50</parameter></function></tool_call>";
        let blocks = blocks_only(parse_plain_text_tool_call_blocks(text, &allowed(&["read"]), DEFAULT_MAX_PAYLOAD_BYTES)).unwrap();
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0].name, "read");
        assert_eq!(blocks[0].arguments.get("path").unwrap().as_str().unwrap(), "/tmp/x");
        assert_eq!(blocks[0].arguments.get("offset").unwrap().as_i64().unwrap(), 1);
        assert_eq!(blocks[0].arguments.get("limit").unwrap().as_i64().unwrap(), 50);
    }

    #[test]
    fn xml_format_multiline_bash_command() {
        let text = "<tool_call><function=bash><parameter=command>cd /workspace && npm test -- tests/services/refreshTokenService.test.ts 2>&amp;1 | grep -A 30 \"FAIL\\|●\"</parameter><parameter=timeout>60</parameter></function></tool_call>";
        let blocks = blocks_only(parse_plain_text_tool_call_blocks(text, &allowed(&["bash"]), DEFAULT_MAX_PAYLOAD_BYTES)).unwrap();
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0].name, "bash");
        assert!(blocks[0].arguments.get("command").unwrap().as_str().unwrap().contains("npm test"));
    }

    #[test]
    fn xml_format_nested_json_parameter() {
        let text = "<tool_call><function=edit><parameter=path>/x.ts</parameter><parameter=edits>[{\"old_string\":\"a\",\"new_string\":\"b\"}]</parameter></function></tool_call>";
        let blocks = blocks_only(parse_plain_text_tool_call_blocks(text, &allowed(&["edit"]), DEFAULT_MAX_PAYLOAD_BYTES)).unwrap();
        assert_eq!(blocks.len(), 1);
        let edits = blocks[0].arguments.get("edits").unwrap();
        assert!(edits.is_array(), "JSON-shaped parameter must parse as array: {edits:?}");
    }

    #[test]
    fn xml_format_multiple_calls_in_text() {
        let text = "<tool_call><function=read><parameter=path>/a</parameter></function></tool_call>\nsome reasoning\n<tool_call><function=bash><parameter=command>ls</parameter></function></tool_call>";
        let blocks = blocks_only(parse_plain_text_tool_call_blocks(text, &allowed(&["read", "bash"]), DEFAULT_MAX_PAYLOAD_BYTES)).unwrap();
        assert_eq!(blocks.len(), 2);
    }

    #[test]
    fn xml_format_disallowed_tool_returns_none() {
        let text = "<tool_call><function=evil_tool><parameter=x>1</parameter></function></tool_call>";
        let blocks = parse_plain_text_tool_call_blocks(text, &allowed(&["read", "bash"]), DEFAULT_MAX_PAYLOAD_BYTES);
        assert!(blocks.is_none());
    }

    #[test]
    fn xml_format_missing_close_returns_none() {
        let text = "<tool_call><function=bash><parameter=command>ls</parameter></function>";
        let blocks = parse_plain_text_tool_call_blocks(text, &allowed(&["bash"]), DEFAULT_MAX_PAYLOAD_BYTES);
        assert!(blocks.is_none());
    }

    #[test]
    fn xml_format_v4_n5_run2_bail_trace_extracted_correctly() {
        // The verbatim reasoning_content shape from V4 N=5 Run 2 (the
        // silent bail observed in Beat 46). With this parser in place,
        // the bash call should be recoverable from the reasoning channel.
        let text = "There's still a failing test. Let me see what failed:\n\n\
            <tool_call>\n\
            <function=bash>\n\
            <parameter=command>\n\
            cd /workspace && npm test -- tests/services/refreshTokenService.test.ts 2>&1 | grep -A 30 \"FAIL\\|●\"\n\
            </parameter>\n\
            <parameter=timeout>\n\
            60\n\
            </parameter>\n\
            </function>\n\
            </tool_call>";
        let blocks = blocks_only(parse_plain_text_tool_call_blocks(text, &allowed(&["bash"]), DEFAULT_MAX_PAYLOAD_BYTES)).unwrap();
        assert_eq!(blocks.len(), 1, "the V4 bail's tool call must be recoverable");
        assert_eq!(blocks[0].name, "bash");
        let cmd = blocks[0].arguments.get("command").unwrap().as_str().unwrap();
        assert!(cmd.contains("npm test"));
        assert!(cmd.contains("refreshTokenService.test.ts"));
        let timeout = blocks[0].arguments.get("timeout").unwrap();
        assert_eq!(timeout.as_i64().unwrap_or(-1), 60);
    }

    #[test]
    fn xml_format_value_contains_parameter_close_tag() {
        // Regression for #409: a parameter value containing the literal
        // "</parameter>" substring (e.g. grepping for it) must not truncate
        // the value or fail the parse. This was the silent-bail edge the
        // first-match `.find()` produced.
        let text = "<tool_call><function=bash><parameter=command>grep \"</parameter>\" file.txt</parameter><parameter=timeout>60</parameter></function></tool_call>";
        let blocks = blocks_only(parse_plain_text_tool_call_blocks(text, &allowed(&["bash"]), DEFAULT_MAX_PAYLOAD_BYTES)).unwrap();
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0].name, "bash");
        assert_eq!(
            blocks[0].arguments.get("command").unwrap().as_str().unwrap(),
            "grep \"</parameter>\" file.txt"
        );
        assert_eq!(blocks[0].arguments.get("timeout").unwrap().as_i64().unwrap(), 60);
    }

    #[test]
    fn xml_format_value_contains_function_and_tool_call_close_tags() {
        // Regression for #409: literal "</function>" and "</tool_call>"
        // inside a value must not truncate the params section or the block.
        // Both are resolved by anchoring on the LAST </function> and the
        // first </tool_call> after it.
        let text = "<tool_call><function=bash><parameter=command>echo \"</function></tool_call>\"</parameter></function></tool_call>";
        let blocks = blocks_only(parse_plain_text_tool_call_blocks(text, &allowed(&["bash"]), DEFAULT_MAX_PAYLOAD_BYTES)).unwrap();
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0].name, "bash");
        assert_eq!(
            blocks[0].arguments.get("command").unwrap().as_str().unwrap(),
            "echo \"</function></tool_call>\""
        );
    }

    #[test]
    fn xml_format_close_tag_in_value_preserves_multiblock_split() {
        // Region-anchoring must not collapse two independent blocks even
        // when the first block's value contains a "</parameter>" substring.
        let text = "<tool_call><function=bash><parameter=command>grep \"</parameter>\"</parameter></function></tool_call>\nthinking\n<tool_call><function=read><parameter=path>/a</parameter></function></tool_call>";
        let blocks = blocks_only(parse_plain_text_tool_call_blocks(text, &allowed(&["bash", "read"]), DEFAULT_MAX_PAYLOAD_BYTES)).unwrap();
        assert_eq!(blocks.len(), 2);
        assert_eq!(blocks[0].name, "bash");
        assert_eq!(blocks[1].name, "read");
        assert_eq!(blocks[1].arguments.get("path").unwrap().as_str().unwrap(), "/a");
    }

    // ─── Format detection (the trajectory observability split) ─────────

    #[test]
    fn format_detection_bracket_classified_as_bracket_or_harmony() {
        let text = "[bash]\n{\"command\":\"ls\"}\n[END_TOOL_REQUEST]";
        let (_, fmt) =
            parse_plain_text_tool_call_blocks(text, &allowed(&["bash"]), DEFAULT_MAX_PAYLOAD_BYTES)
                .unwrap();
        assert_eq!(fmt, PromotionFormat::BracketOrHarmony);
    }

    #[test]
    fn format_detection_harmony_classified_as_bracket_or_harmony() {
        let text = "<|channel|>commentary to=bash code <|message|>{\"command\":\"ls\"}<|call|>";
        let (_, fmt) =
            parse_plain_text_tool_call_blocks(text, &allowed(&["bash"]), DEFAULT_MAX_PAYLOAD_BYTES)
                .unwrap();
        assert_eq!(fmt, PromotionFormat::BracketOrHarmony);
    }

    #[test]
    fn format_detection_xml_classified_as_xml() {
        let text =
            "<tool_call><function=bash><parameter=command>ls</parameter></function></tool_call>";
        let (_, fmt) =
            parse_plain_text_tool_call_blocks(text, &allowed(&["bash"]), DEFAULT_MAX_PAYLOAD_BYTES)
                .unwrap();
        assert_eq!(fmt, PromotionFormat::Xml);
    }

    // ─── Channel fallback semantics ────────────────────────────────────

    fn msg(content: Option<&str>, reasoning: Option<&str>, tool_calls: Option<Vec<ToolCall>>) -> Message {
        Message {
            role: "assistant".into(),
            content: content.map(String::from),
            reasoning_content: reasoning.map(String::from),
            tool_calls,
            tool_call_id: None,
            name: None,
        }
    }

    #[test]
    fn promote_from_content_when_text_has_xml_tool_call() {
        let mut m = msg(
            Some("<tool_call><function=bash><parameter=command>ls</parameter></function></tool_call>"),
            None,
            None,
        );
        let info = promote_plain_text_tool_calls(&mut m, &allowed(&["bash"])).unwrap();
        assert_eq!(info.source, PromotionSource::Content);
        assert_eq!(info.format, PromotionFormat::Xml);
        assert_eq!(info.call_count, 1);
        assert_eq!(m.tool_calls.as_ref().unwrap().len(), 1);
        assert_eq!(m.tool_calls.as_ref().unwrap()[0].function.name, "bash");
        assert!(m.content.is_none(), "promoted content channel must clear");
    }

    #[test]
    fn promote_from_reasoning_when_content_empty_and_reasoning_has_xml() {
        let mut m = msg(
            None,
            Some("Now I should run tests:\n<tool_call><function=bash><parameter=command>cargo test</parameter></function></tool_call>"),
            None,
        );
        let info = promote_plain_text_tool_calls(&mut m, &allowed(&["bash"])).unwrap();
        assert_eq!(info.source, PromotionSource::Reasoning);
        assert_eq!(info.format, PromotionFormat::Xml);
        assert_eq!(info.call_count, 1);
        assert_eq!(m.tool_calls.as_ref().unwrap().len(), 1);
        assert_eq!(m.tool_calls.as_ref().unwrap()[0].function.name, "bash");
        assert!(m.reasoning_content.is_none(), "promoted reasoning channel must clear");
    }

    #[test]
    fn promote_from_content_with_bracket_format_classifies_as_bracket_or_harmony() {
        let mut m = msg(
            Some("[bash]\n{\"command\":\"ls\"}\n[END_TOOL_REQUEST]"),
            None,
            None,
        );
        let info = promote_plain_text_tool_calls(&mut m, &allowed(&["bash"])).unwrap();
        assert_eq!(info.source, PromotionSource::Content);
        assert_eq!(info.format, PromotionFormat::BracketOrHarmony);
    }

    #[test]
    fn promote_returns_none_when_tool_calls_already_populated() {
        let existing = vec![ToolCall {
            id: "call_existing".to_string(),
            kind: "function".to_string(),
            function: FunctionCall {
                name: "read".to_string(),
                arguments: r#"{"path":"/x"}"#.to_string(),
            },
        }];
        let mut m = msg(
            Some("<tool_call><function=bash><parameter=command>ls</parameter></function></tool_call>"),
            None,
            Some(existing),
        );
        let result = promote_plain_text_tool_calls(&mut m, &allowed(&["bash", "read"]));
        assert!(result.is_none());
        // The structured field is preserved (still "read"), not overwritten by XML "bash".
        assert_eq!(m.tool_calls.as_ref().unwrap()[0].function.name, "read");
    }

    #[test]
    fn promote_returns_none_when_neither_channel_has_tool_call_text() {
        let mut m = msg(Some("just some prose, no tool calls"), Some("just thinking"), None);
        let result = promote_plain_text_tool_calls(&mut m, &allowed(&["bash"]));
        assert!(result.is_none());
        assert!(m.tool_calls.is_none());
    }

    #[test]
    fn promote_returns_none_when_allowed_tool_names_empty() {
        // Mirrors openclaw's `wrapLmstudioPlainTextToolCalls` early-
        // return: an empty allowed-set must not be treated as "allow
        // everything" — that would be a state-drift hazard if any
        // future caller ever passed an empty set.
        let mut m = msg(
            Some("<tool_call><function=bash><parameter=command>ls</parameter></function></tool_call>"),
            None,
            None,
        );
        let empty: HashSet<String> = HashSet::new();
        let result = promote_plain_text_tool_calls(&mut m, &empty);
        assert!(result.is_none(), "empty allowed-set must refuse promotion");
        assert!(m.tool_calls.is_none(), "tool_calls must not be synthesized on empty allowed-set");
        assert!(m.content.is_some(), "source content must be preserved when no promotion fired");
    }

    #[test]
    fn xml_format_size_cap_fires_for_oversized_payload() {
        // Payload exceeds max_payload_bytes — parser must return None.
        let huge_value: String = "a".repeat(10_000);
        let text = format!(
            "<tool_call><function=bash><parameter=command>{huge_value}</parameter></function></tool_call>"
        );
        let blocks = parse_plain_text_tool_call_blocks(&text, &allowed(&["bash"]), 500);
        assert!(blocks.is_none(), "oversize XML payload must reject");
    }

    #[test]
    fn bracket_format_missing_line_break_after_close_bracket_returns_none() {
        // Bracket parser requires a line break after `]`. Single-line
        // emissions are not the recognized format — fail rather than
        // partially match.
        let text = "[bash] {\"command\":\"ls\"} [END_TOOL_REQUEST]";
        let blocks = parse_plain_text_tool_call_blocks(text, &allowed(&["bash"]), DEFAULT_MAX_PAYLOAD_BYTES);
        assert!(blocks.is_none());
    }

    #[test]
    fn harmony_format_final_channel_recognized() {
        // The parser accepts commentary / analysis / final as the
        // three valid harmony channels — test the third explicitly
        // since the others have dedicated cases above.
        let text = "<|channel|>final to=bash code <|message|>{\"command\":\"x\"}<|call|>";
        let blocks = blocks_only(parse_plain_text_tool_call_blocks(text, &allowed(&["bash"]), DEFAULT_MAX_PAYLOAD_BYTES)).unwrap();
        assert_eq!(blocks.len(), 1);
    }

    #[test]
    fn promote_prefers_content_over_reasoning_when_both_have_calls() {
        // Operator-checkable invariant: if both channels parse, content
        // wins. Matches the model's intent — content is the formal output
        // channel; reasoning_content is the secondary "thinking" channel
        // that should only fire when content is empty.
        let mut m = msg(
            Some("<tool_call><function=bash><parameter=command>ls</parameter></function></tool_call>"),
            Some("<tool_call><function=read><parameter=path>/a</parameter></function></tool_call>"),
            None,
        );
        let info = promote_plain_text_tool_calls(&mut m, &allowed(&["bash", "read"])).unwrap();
        assert_eq!(info.source, PromotionSource::Content);
        assert_eq!(m.tool_calls.as_ref().unwrap()[0].function.name, "bash");
        // Reasoning channel preserved when promotion came from content.
        assert!(m.reasoning_content.is_some());
    }

    // ─── Real-world fixture replays (#437 / E13) ─────────────────────────

    /// (Fixture: Beat 48 run-5 — production trace)
    ///
    /// New micro-pattern observed in N=5 validation: the model emitted
    /// only XML CLOSING tags (`</parameter></function></tool_call>`)
    /// inside `reasoning_content` without the OPENING `<tool_call>`.
    /// Model "intended" a tool call but never formulated the open
    /// half. `finish_reason=stop`, dispatch terminated.
    ///
    /// The promoter must correctly NOT match this — there's no
    /// extractable structure (orphan-close ≠ tool-call). Regression
    /// guard against a future parser change that would over-match on
    /// closing tags alone.
    #[test]
    fn fixture_beat48_run5_orphan_xml_close_tags_not_promoted() {
        let raw = include_str!(
            "../tests/fixtures/promoter-emissions/beat48-run5-orphan-xml-close-tags.txt"
        );
        let allowed: HashSet<String> = ["read", "edit", "write", "bash", "search"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let result = parse_plain_text_tool_call_blocks(raw, &allowed, DEFAULT_MAX_PAYLOAD_BYTES);
        assert!(
            result.is_none(),
            "orphan XML closing tags (no opening <tool_call>) must not produce a match"
        );
    }
}
