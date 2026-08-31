//! The `radio` interpreter core (#1698 Packet A) — surface-neutral ENGINE
//! capability, never ACP-specific. `src/acp_panel.rs`'s ratified doc line:
//! "the interpreter is ENGINE capability; ACP is its first consumer, the
//! CLI its second." Two independent consumers exist (or will): the CLI verb
//! (`src/radio_cli.rs`, this packet) and the ACP no-slash channel
//! (Packet B). Both call INTO this module; neither owns any piece of it.
//! This module in turn calls INTO `crate::acp_panel` (catalog enumeration,
//! `RoutePlan`, `run_ephemeral`) rather than duplicating that logic — the
//! same single-derivation principle the module doc there names ("the shared
//! facts script"): there is exactly one place that decides which mission
//! configs are advertised, and exactly one place that decides how an
//! advertised command actually runs.
//!
//! # The two-seat receiver architecture (issue #1698, "naming ratified")
//!
//! "You key the CHANNEL, never a model — who's tuned in is config." Two
//! seats exist per the role-family doctrine:
//!
//! - **The ROUTING seat** (this module, Packet A) — bounded classification:
//!   free text + the advertised catalog in, one command id + args (or a
//!   refusal) out. Dispatches through the `radio-router` role
//!   (`role_family: "utility"`), the SAME `crate::fleet::dispatch_routed`
//!   mechanism `src/mission_propose.rs`'s `dispatch_compiler` uses — no new
//!   dispatch path invented.
//! - **The ANSWERING seat** (Packet B) — reasoning-bearing grounded answers
//!   over session artifacts. Not built here.
//!
//! **Deferred to Packet B in the original design (deliberately, "one schema
//! change, not two"), landed in Packet B2:** config-block staffing for the
//! routing seat. Packet A's `dispatch_router_call` passed
//! `profile_name: None` unconditionally, resolving through the SAME
//! precedence every other role dispatch uses (the `role_profiles.<role_id>`
//! map if an operator has set one, else `default_profile`). Packet B2 adds
//! `radio.router_profile` — see `dispatch_router_call`'s own doc — an EMPTY
//! value still resolves to `None` here, so the pre-B2 fallback chain (and
//! any interim `role_profiles.radio-router` pin) keeps working unchanged
//! until the operator opts into the new knob.
//!
//! # Safety walls this module is directly responsible for (issue #1698)
//!
//! - **Wall 2 (selection-never-composition boxes output).** [`RouteDecision::
//!   Route`] can only ever name a `command` that was already present in the
//!   `catalog` this call was given — [`validate_router_output`] re-checks
//!   the model's claimed id against the catalog after parsing, the model
//!   never gets to hand back a bare string that becomes the answer
//!   verbatim.
//! - **Wall 6 (fail closed).** Every failure mode — empty output, JSON that
//!   doesn't parse, JSON that parses but names a command outside the
//!   catalog, a `call` closure that errors — becomes [`RouteDecision::
//!   Refuse`], never a guess and never a panic. [`route`] itself never
//!   returns `Result` for exactly this reason: there is no "routing
//!   failed" outcome distinct from "refuse."
//!
//! # Frozen model-facing text (contract 6)
//!
//! The routing seat's SYSTEM prompt lives at
//! `templates/builtin/roles/radio-router.md`, loaded like any other role's
//! prompt (`crate::crew::loader::role_prompt`) — a single committed
//! artifact, byte-locked by
//! `tests::radio_router_role_prompt_matches_frozen_golden`. The USER
//! message this module assembles per call ([`build_router_message`]) is
//! likewise byte-locked, by `tests::build_router_message_matches_frozen_golden`.
//! Both follow the AI-convention-terminology doctrine (CLAUDE.md's "Model-
//! facing prompt construction"): "the user's message", "available
//! commands" — no darkmux-internal jargon a clean-context model would need
//! provenance for.

use anyhow::Result;
use serde::Deserialize;

/// One entry in the model-facing command catalog — what the routing seat
/// is allowed to choose among. Compiled fresh per call by [`compile_catalog`]
/// (never cached across calls: the registry can change between the CLI
/// process starting and the router dispatch actually running).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CatalogEntry {
    /// The REGISTRY-RESOLVABLE id — see `crate::acp_panel::PanelCommand::id`'s
    /// own doc for why this is never the document body's own `id` field.
    pub id: String,
    /// The model-grounding text for THIS catalog — the config's TOP-LEVEL
    /// `description` field — the SAME text `crate::acp_panel::
    /// list_panel_commands` advertises to an editor's command palette
    /// (`PanelConfig::description`, falling back to `MissionConfig.name`).
    ///
    /// **Deliberately NOT `MissionConfig.description`** (the config's
    /// TOP-LEVEL field) — an earlier draft of this module preferred that
    /// field on the theory that a routing model benefits from more
    /// substantive provenance text than a two-word palette label. Reviewed
    /// against the actual advertised registry and reverted: the built-in
    /// `review` config's top-level `description` is ~2KB of engineering
    /// provenance (crate paths, issue numbers, `StepKind` names) — exactly
    /// the darkmux-internal-jargon shape `acp_panel.rs`'s own `PanelCommand`
    /// doc already calls out as "unsuitable to render... in an editor's
    /// slash-command list", and CLAUDE.md's "Model-facing prompt
    /// construction" audit question ("what does this read as to a
    /// fresh-context model with no darkmux history?") answers the same way
    /// for a routing model. `panel.description` is short, human-written FOR
    /// exactly this kind of at-a-glance classification, and reusing it here
    /// keeps [`compile_catalog`] a pure single-derivation `map`.
    pub description: String,
    /// `panel.hint` — the same optional input hint the editor shows before
    /// the user has typed anything after the command name.
    pub hint: Option<String>,
}

/// Compile the model-facing catalog from the SAME merged, panel-blocked
/// enumeration `crate::acp_panel::list_panel_commands` already produces
/// (single derivation of "what's advertised" — see this module's own doc).
/// A pure `map`: no second registry load, no divergent filter logic.
/// Deterministic ordering: `list_panel_commands` sorts by id, and this
/// function's `map` preserves that order.
pub fn compile_catalog() -> Vec<CatalogEntry> {
    crate::acp_panel::list_panel_commands()
        .into_iter()
        .map(|cmd| CatalogEntry {
            id: cmd.id,
            description: cmd.description,
            hint: cmd.hint,
        })
        .collect()
}

/// The routing seat's decision for one exchange — see this module's doc on
/// walls 2 and 6. There is no third variant and no `Result`: every failure
/// mode this module can encounter collapses into `Refuse`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RouteDecision {
    /// `command` is guaranteed to be one of `catalog`'s own `id` strings,
    /// copied from the CATALOG entry (never the model's raw text) after a
    /// case-insensitive match — see [`validate_router_output`]'s doc for
    /// why this mirrors `crate::acp_panel::route_command`'s own rule.
    Route { command: String, args: String },
    /// A short, model- or validator-supplied reason. Never blank —
    /// [`validate_router_output`] and [`route`] always supply one.
    Refuse { reason: String },
    /// The routing dispatch could not RUN: no profile registry, a
    /// placeholder or missing model, no `lms`, LM Studio's server down.
    /// Distinct from [`RouteDecision::Refuse`] on purpose: a refusal is
    /// the model declining and is worth handing to the answering seat; an
    /// Unavailable would fail that seat identically, so consumers print
    /// the error ONCE and exit non-zero (first-run probes, 2026-08-28).
    Unavailable { error: String },
}

/// The injectable model-call seam (issue #1698, "the model call is
/// injectable... so every routing test runs WITHOUT a live model"). The
/// production implementation is [`dispatch_router_call`]; every test in
/// this module supplies a canned closure instead.
pub type ModelCall<'a> = dyn FnMut(&str) -> Result<String> + 'a;

/// Route `text` against `catalog` via ONE call through `call`. Fails closed
/// (returns [`RouteDecision::Refuse`]) rather than invoking `call` at all
/// when `catalog` is empty (there is nothing to route to) OR `text` is
/// empty/whitespace-only (there is nothing to classify) — either way,
/// dispatching a model for a decision with no real input would just be
/// theater. `crate::acp_panel::parse_command` applies the same "nothing
/// here" short-circuit for the slash-command channel.
pub fn route(text: &str, catalog: &[CatalogEntry], call: &mut ModelCall<'_>) -> RouteDecision {
    if catalog.is_empty() {
        return RouteDecision::Refuse {
            reason: "no commands are currently advertised — there is nothing to route to".to_string(),
        };
    }
    if text.trim().is_empty() {
        return RouteDecision::Refuse {
            reason: "no message text was given — there is nothing to route".to_string(),
        };
    }
    let message = build_router_message(text, catalog);
    match call(&message) {
        Ok(raw) => validate_router_output(&raw, catalog),
        Err(e) => RouteDecision::Unavailable { error: format!("{e:#}") },
    }
}

/// Which consumer initiated a routed invocation — issue #1698's wall 4
/// ("provenance boxes invisibility ... drops a flow record with source
/// text + chosen route") needs to know which surface it came from.
/// `src/radio_cli.rs` (the `darkmux radio` verb, Packet A) and the ACP
/// no-slash channel (`src/acp.rs`, Packet B) are the two consumers today.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RadioSurface {
    Cli,
    Panel,
}

impl RadioSurface {
    fn as_str(self) -> &'static str {
        match self {
            RadioSurface::Cli => "cli",
            RadioSurface::Panel => "panel",
        }
    }
}

/// How much of the raw source text wall 4's flow record carries — mirrors
/// `dispatch.tool`'s own `args` cap (512 chars, FLOW_SCHEMA 1.16.0) so one
/// long paste into the panel doesn't blow up a flow record. Counted in
/// `char`s (not bytes), matching every other length-cap convention in this
/// codebase (`crate::dispatch::capped_prompt` counts `chars().count()`
/// too), so a multi-byte-heavy message isn't capped more aggressively than
/// an ASCII one of the same visible length.
const SOURCE_TEXT_RECORD_CAP: usize = 512;

/// [`route`] PLUS wall 4's flow record — the ONE place both consumers of
/// this module (`src/radio_cli.rs`'s CLI verb; `src/acp.rs`'s ACP no-slash
/// channel) go through, so the record is written exactly once per
/// invocation regardless of which surface it came from ("emit from the
/// shared core, once" — issue #1698's Packet B carry-list item 2).
///
/// The record carries the source text (capped, see
/// [`SOURCE_TEXT_RECORD_CAP`]), the chosen command id + args on a
/// [`RouteDecision::Route`], or the refusal reason on a
/// [`RouteDecision::Refuse`] — emitted AFTER the decision is known (a
/// single record per invocation, not a start/complete bookend pair: the
/// underlying model call already gets its own `dispatch.start`/
/// `dispatch.complete` bookends via [`dispatch_router_call`]'s
/// `dispatch_routed_via` call, so this is a HIGHER-LEVEL record about the
/// routing OUTCOME, not a second liveness pair for the same call — the
/// same "outer record wraps an inner dispatch's own bookends" shape
/// `mission_propose.rs`'s `mission.compile.start`/`.complete` uses around
/// its own `dispatch_routed` call).
pub fn route_and_record(
    text: &str,
    catalog: &[CatalogEntry],
    surface: RadioSurface,
    call: &mut ModelCall<'_>,
) -> RouteDecision {
    let decision = route(text, catalog, call);
    emit_route_record(text, surface, &decision);
    decision
}

/// Build + write wall 4's flow record for one routed invocation. Best-
/// effort — a flow-write failure (e.g. an unwritable flows dir) must never
/// turn a successful route into a failed one, so the `Result` from
/// `crate::flow::record` is intentionally discarded here, same posture
/// every other flow-record emission site in this codebase takes.
fn emit_route_record(text: &str, surface: RadioSurface, decision: &RouteDecision) {
    let truncated: String = text.chars().take(SOURCE_TEXT_RECORD_CAP).collect();
    let mut payload = serde_json::json!({
        "surface": surface.as_str(),
        "source_text": truncated,
    });
    match decision {
        RouteDecision::Route { command, args } => {
            payload["decision"] = serde_json::json!("route");
            payload["command"] = serde_json::json!(command);
            payload["args"] = serde_json::json!(args);
        }
        RouteDecision::Refuse { reason } => {
            payload["decision"] = serde_json::json!("refuse");
            payload["reason"] = serde_json::json!(reason);
        }
        RouteDecision::Unavailable { error } => {
            payload["decision"] = serde_json::json!("unavailable");
            payload["error"] = serde_json::json!(error);
        }
    }
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let synth_session_id = crate::types::session_id::session_id("radio", &nanos.to_string(), "");
    let record = crate::crew::dispatch::build_dispatch_record_with_payload(
        crate::flow::Level::Info,
        "radio.route",
        "radio-router",
        &synth_session_id,
        None,
        None,
        None,
        Some(payload),
    );
    let _ = crate::flow::record(record);
}

/// Assemble the routing seat's USER message (the routing seat's SYSTEM
/// prompt is the frozen `radio-router.md`, loaded by the dispatch
/// machinery, not built here). Byte-locked by
/// `tests::build_router_message_matches_frozen_golden` — this is the
/// "assembly" half of contract 6, alongside the frozen `.md` prompt itself.
///
/// AI-convention terminology throughout (CLAUDE.md's "Model-facing prompt
/// construction"): "the user's message", "available commands" — command
/// ids and descriptions are self-explanatory as listed, no darkmux-jargon
/// provenance marker needed.
pub fn build_router_message(text: &str, catalog: &[CatalogEntry]) -> String {
    let mut msg = String::new();
    msg.push_str("Available commands:\n");
    for entry in catalog {
        msg.push_str("- ");
        msg.push_str(&entry.id);
        msg.push_str(": ");
        msg.push_str(&entry.description);
        if let Some(hint) = &entry.hint {
            msg.push_str(" (hint: ");
            msg.push_str(hint);
            msg.push(')');
        }
        msg.push('\n');
    }
    msg.push_str("\nThe user's message:\n---\n");
    msg.push_str(text.trim());
    msg.push_str("\n---\n\n");
    msg.push_str(
        "Decide whether this message maps onto exactly one of the commands above, and \
         respond with exactly one fenced ```json block per your instructions.\n",
    );
    msg
}

/// The two shapes the routing seat's fenced JSON block can take — see the
/// module doc's "Output contract". `#[serde(untagged)]` tries `Route` first
/// (requires `command`), then `Refuse` (requires `refuse`); a JSON object
/// matching neither shape fails to deserialize as either, which
/// [`validate_router_output`] treats as malformed (wall 6).
///
/// **`deny_unknown_fields` on BOTH variants is load-bearing, not
/// decoration.** Without it, a confused response carrying BOTH keys —
/// `{"command": "review", "refuse": "too ambiguous to route"}` — matches
/// `Route` on the first (and only) attempt `untagged` makes (`Route`'s own
/// fields are all present; the model's own explicit refusal in `refuse`
/// would otherwise be silently dropped as an "unknown" field) and the
/// result EXECUTES — exactly the fail-OPEN case wall 6 exists to prevent.
/// With `deny_unknown_fields`, that same object fails BOTH shapes (each
/// sees the OTHER shape's field as unrecognized) and
/// [`validate_router_output`] correctly falls through to its generic
/// "didn't match the route-or-refuse contract" refusal instead.
#[derive(Debug, Deserialize)]
#[serde(untagged, deny_unknown_fields)]
enum RawRouterOutput {
    Route {
        command: String,
        #[serde(default)]
        args: String,
    },
    Refuse { refuse: String },
}

/// Validate one raw model response against the fail-closed contract (wall
/// 6). This is the piece every routing test in this module exercises
/// directly with canned strings — no live model, per the issue's
/// injectability requirement.
///
/// Failure modes, all collapsing to `Refuse`:
/// - empty (or whitespace-only) `raw`
/// - no parseable JSON object (fenced ```json block preferred, a bare JSON
///   object as a forgiving fallback)
/// - JSON that parses but matches neither `RawRouterOutput` shape
/// - `command` that doesn't case-insensitively match any `catalog` entry
///
/// A well-formed `{"refuse": "..."}` is NOT a failure mode — it's the
/// model's own explicit refusal, relayed verbatim (or a default reason if
/// the model somehow emitted an empty string there).
fn validate_router_output(raw: &str, catalog: &[CatalogEntry]) -> RouteDecision {
    if raw.trim().is_empty() {
        return RouteDecision::Refuse {
            reason: "the routing seat returned no output".to_string(),
        };
    }
    let Some(value) = extract_json(raw) else {
        return RouteDecision::Refuse {
            reason: "the routing seat's response wasn't valid JSON".to_string(),
        };
    };
    let parsed: RawRouterOutput = match serde_json::from_value(value) {
        Ok(p) => p,
        Err(_) => {
            return RouteDecision::Refuse {
                reason: "the routing seat's response didn't match the route-or-refuse contract".to_string(),
            };
        }
    };
    match parsed {
        RawRouterOutput::Refuse { refuse } => {
            let reason = if refuse.trim().is_empty() {
                "the routing seat declined to route this message".to_string()
            } else {
                refuse
            };
            RouteDecision::Refuse { reason }
        }
        RawRouterOutput::Route { command, args } => {
            // (wall 2) Case-insensitive match against the CATALOG, mirroring
            // `crate::acp_panel::route_command`'s own rule — the resolved
            // `command` is the CATALOG entry's own (correctly-cased) id,
            // never the model's raw text, so a model that echoes back a
            // differently-cased id still resolves correctly while an
            // out-of-catalog id still refuses.
            match catalog.iter().find(|c| c.id.eq_ignore_ascii_case(&command)) {
                Some(entry) => RouteDecision::Route {
                    command: entry.id.clone(),
                    args,
                },
                None => RouteDecision::Refuse {
                    reason: format!("the routing seat named `{command}`, which isn't an advertised command"),
                },
            }
        }
    }
}

/// Extract a JSON value from a routing-seat response: prefer a fenced
/// ```json block (matching `templates/builtin/roles/radio-router.md`'s own
/// instructed output shape and `src/mission_propose.rs::extract_json_block`'s
/// established convention in this codebase), falling back to parsing the
/// WHOLE trimmed response as bare JSON for a model that skips the fence.
/// `None` when neither yields valid JSON — the caller turns that into a
/// fail-closed refusal (wall 6), never a guess at a truncated/partial parse.
fn extract_json(raw: &str) -> Option<serde_json::Value> {
    if let Some(block) = extract_fenced_json_block(raw) {
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(&block) {
            return Some(v);
        }
    }
    serde_json::from_str::<serde_json::Value>(raw.trim()).ok()
}

/// Return the content of the first ```json-tagged fenced block (falling
/// back to the first bare ``` fence when no tagged opener exists anywhere),
/// or `None` when no CLOSED fence is found at all, OR (see the strictness
/// note below) a SECOND fence opener follows the first block's close.
/// Deliberately simpler than `mission_propose.rs::extract_json_block` (no
/// "unterminated" distinction — this module's caller only needs "did we
/// get JSON or not," and an unterminated block naturally fails the JSON
/// parse in [`extract_json`]'s bare-parse fallback too).
///
/// Substring search over the whole response, not a line-by-line scan — a
/// small model that emits the fence and its content on ONE physical line
/// (`` ```json {"command": "x"} ``` `` with no newlines at all) still
/// extracts correctly, since the opener and closer are found by byte
/// position rather than by which line they're on.
///
/// **Fence-extraction strictness (#1698 Packet B carry-list item 4):
/// REFUSE-ON-MULTIPLE-BLOCKS, not first-fence-wins.** If more than one
/// fenced block appears ANYWHERE in the response — before OR after the
/// block this function would otherwise extract — this returns `None`
/// rather than silently treating one block as authoritative. Checked by
/// TOTAL fence-delimiter count (every `` ``` `` — tagged or bare — is
/// exactly one delimiter; a single well-formed block has exactly two:
/// its opener and its closer), not by scanning only the text AFTER the
/// first recognized block — an earlier version of this check only looked
/// forward from the first ` ```json`/` ```` opener it found, which missed
/// a block that came BEFORE it (e.g. a bare ` ``` ` scratch/reasoning
/// fence followed by the real ` ```json ` answer) — a real gap a fresh
/// review caught, not a hypothetical. Why refuse rather than pick one: the
/// router's own system prompt (`templates/builtin/roles/radio-router.md`)
/// instructs "Emit exactly one fenced `json` block and no prose outside
/// it" — a response carrying a second block anywhere already violated
/// that contract, and picking one anyway is a GUESS about which the model
/// actually meant. The router's own rules already name the correct
/// response to any uncertainty ("When in doubt, refuse... Refusing is
/// always the safer answer") — a response that can't even follow its own
/// output-shape instruction is the same class of uncertainty, and wall 6
/// (fail closed) treats every uncertain case identically. `None` here
/// routes through [`extract_json`]'s bare-JSON fallback, which a
/// multi-block response (fences + surrounding text) will not satisfy
/// either, so the net effect is a genuine "the routing seat's response
/// wasn't valid JSON" refusal — no separate error path needed for this
/// case.
fn extract_fenced_json_block(raw: &str) -> Option<String> {
    if raw.matches("```").count() > 2 {
        return None;
    }
    let (open_idx, tag_len) = if let Some(i) = raw.find("```json") {
        (i, "```json".len())
    } else if let Some(i) = raw.find("```JSON") {
        (i, "```JSON".len())
    } else {
        let i = raw.find("```")?;
        (i, "```".len())
    };
    let after_open = &raw[open_idx + tag_len..];
    let close_rel = after_open.find("```")?;
    Some(after_open[..close_rel].to_string())
}

/// The production [`ModelCall`] implementation — ONE single-shot dispatch to
/// the `radio-router` role via `crate::fleet::dispatch_routed_via(opts,
/// crate::crew::dispatch::dispatch_local_single_shot)` (#1698 Packet B —
/// updated from Packet A's plain `dispatch_routed`, which rode the full
/// internal-runtime container for a tool-less single-shot; see
/// `dispatch_local_single_shot`'s own doc for the container-free path).
/// The underlying primitive still emits the `dispatch.start`/
/// `dispatch.complete` flow-record bookends (contract 2, dispatch
/// liveness) — no additional bookend is added here, unlike
/// `dispatch_compiler`'s extra `mission.compile.*` pair. Wall 4's own
/// flow record (source text + chosen route/refusal + surface) is a
/// SEPARATE, higher-level record — see [`route_and_record`]/
/// [`emit_route_record`] above, which now exists (Packet B landed it in
/// this same module, not deferred any further).
///
/// `timeout_seconds: 300` — a deliberately BOUNDED ceiling for a
/// bounded-classification dispatch (well under `dispatch_compiler`'s 600s,
/// which budgets for a much larger structured-proposal task). Not yet
/// operator-tunable.
pub fn dispatch_router_call(message: &str) -> Result<String> {
    let opts = crate::crew::dispatch::DispatchOpts {
        workspace_read_only: false,
        record_context: None,
        resume_from: None,
        host_out: None,
        max_turns_override: None,
        role_id: "radio-router".to_string(),
        message: message.to_string(),
        session_id: None,
        timeout_seconds: 300,
        skip_preflight: false,
        // radio-router parses its answer from the dispatch's human-readable
        // stdout (a fenced ```json block) — no JSON envelope needed, same
        // as mission-compiler.
        json: false,
        workdir: None,
        phase_id: None,
        // A system-level utility dispatch; local-only (`machine: None`
        // never routes to the fleet queue — #309, #1405).
        machine: None,
        wait: true,
        compaction: crate::crew::dispatch::CompactionDispatchArgs::default(),
        // (#1698 Packet B2) `radio.router_profile` when the operator has
        // set one — an explicit override that takes precedence OVER
        // `role_profiles.radio-router` (the same precedence every other
        // `--profile`-style override uses). Unset (the common case today)
        // resolves to `None` here, preserving the PRE-B2 behavior exactly:
        // falls through to `role_profiles.radio-router` (the interim pin
        // operators set per this issue's own live-dogfood notes) then
        // `default_profile`. See `RadioConfig::router_profile`'s own doc.
        profile_name: darkmux_types::config_access::radio_router_profile(),
        config_path: None,
        force_container: false,
        max_completion_tokens: None,
        image: None,
        model_base_url_override: None,
        step_id: None,
        system_prompt_override: None,
    };
    // (#1698 Packet B — the container-path fix the issue's live dogfood
    // comment named: "the route rides the FULL internal-runtime container
    // dispatch ... for a tool-less single-shot — the routing seat should
    // take a direct single-shot HTTP path.") `dispatch_routed_via` (not the
    // plain `dispatch_routed` Packet A used) takes a caller-injected LOCAL
    // execution primitive — the exact substitution seam #1509 built for
    // `dispatch_as_crew_of_one` — so this swaps in
    // `crate::crew::dispatch::dispatch_local_single_shot` (a container-free
    // HTTP call straight to LMStudio, #1135-safe residency included) in
    // place of the ordinary `crate::crew::dispatch::dispatch` (full
    // internal-runtime container spin). `opts.machine` stays `None` above,
    // so this is always the LOCAL fall-through — never a fleet-queue
    // publish — and `dispatch_local_single_shot` itself falls back to the
    // light REMOTE path automatically when the resolved profile targets a
    // hosted endpoint (see its own doc), same as before this change.
    let result = crate::fleet::dispatch_routed_via(opts, crate::crew::dispatch::dispatch_local_single_shot)?;
    Ok(result.stdout)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(id: &str, description: &str, hint: Option<&str>) -> CatalogEntry {
        CatalogEntry {
            id: id.to_string(),
            description: description.to_string(),
            hint: hint.map(str::to_string),
        }
    }

    fn fixture_catalog() -> Vec<CatalogEntry> {
        vec![
            entry("pr-list", "List open pull requests against the current repository.", None),
            entry("review", "Run the review pipeline against the current branch's diff.", Some("optional PR number")),
        ]
    }

    // ── build_router_message (frozen golden, contract 6) ────────────────

    #[test]
    fn build_router_message_matches_frozen_golden() {
        let msg = build_router_message("review this when you get a sec", &fixture_catalog());
        let expected = "Available commands:\n\
             - pr-list: List open pull requests against the current repository.\n\
             - review: Run the review pipeline against the current branch's diff. (hint: optional PR number)\n\
             \n\
             The user's message:\n\
             ---\n\
             review this when you get a sec\n\
             ---\n\
             \n\
             Decide whether this message maps onto exactly one of the commands above, and \
             respond with exactly one fenced ```json block per your instructions.\n";
        assert_eq!(msg, expected, "byte-locked assembly drifted — see contract 6 in this module's doc");
    }

    #[test]
    fn build_router_message_trims_the_users_text() {
        let msg = build_router_message("  spaced out  \n", &fixture_catalog());
        assert!(msg.contains("---\nspaced out\n---"), "{msg}");
    }

    // ── radio-router.md frozen golden (contract 6) ──────────────────────

    #[test]
    fn radio_router_role_prompt_matches_frozen_golden() {
        // A LITERAL duplicate of `templates/builtin/roles/radio-router.md`
        // — independently typed here so editing the file can't silently
        // avoid failing this test (contract 6: "'Frozen' means one hash,
        // not one intention"). Mirrors `darkmux-lab`'s
        // `verify_prompt_matches_frozen_golden` pattern (a literal
        // `VERIFY_TAIL_INSTRUCTION` constant), not
        // `build_router_message_matches_frozen_golden` above (which is
        // already a literal, since it's asserting THIS module's own
        // assembly function against a hand-written expected string, not
        // re-reading a file).
        //
        // (#1698 Packet B carry-list item 3, #1701's merge-gate finding —
        // "observed live" against an operator's own persona override) this
        // is compared against an `include_str!` of the SHIPPED template
        // directly, NEVER `crate::crew::loader::role_prompt("radio-router")`.
        // `role_prompt` resolves the SAME operator-tier-override-wins
        // precedence every dispatch honors (`~/.darkmux/crew/roles/
        // radio-router.md`, loader-preferred over the embedded default —
        // see the issue's own "RADIO's persona" comment, which documents
        // exactly this override as the delivery mechanism for the
        // operator's TARS persona) — a golden test that resolved through
        // that precedence would spuriously fail on any machine carrying a
        // persona override, exactly what happened live: the override is
        // sovereign operator config (CLAUDE.md's "operator sovereignty"),
        // not a test failure. `include_str!` of the literal shipped path
        // bypasses the override entirely, so this test verifies the
        // SHIPPED artifact stays byte-locked regardless of what any given
        // machine has layered on top of it.
        const SHIPPED_TEMPLATE: &str = include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/templates/builtin/roles/radio-router.md"
        ));
        let expected = "# Radio Router\n\
            \n\
            You take one short message from the user and decide which of a fixed list of commands it maps onto, if any.\n\
            \n\
            ## Your job\n\
            \n\
            Every call gives you:\n\
            1. A list of available commands, each with an id and a description of what it does.\n\
            2. The user's message.\n\
            \n\
            Decide whether the message clearly asks for ONE of the listed commands. If it does, name that command's id and pull out any trailing text the command should receive as its argument. If it does not clearly match any listed command — the message is ambiguous, open-ended, matches more than one command about equally well, or matches none of them — refuse instead of guessing.\n\
            \n\
            ## Output: exactly one JSON object, nothing else\n\
            \n\
            Emit exactly one fenced `json` block and no prose outside it.\n\
            \n\
            To route the message to a command:\n\
            \n\
            ```json\n\
            {\"command\": \"<the exact command id from the list>\", \"args\": \"<any trailing text the command should receive, or an empty string>\"}\n\
            ```\n\
            \n\
            To refuse:\n\
            \n\
            ```json\n\
            {\"refuse\": \"<a short, one-sentence reason>\"}\n\
            ```\n\
            \n\
            ## Rules\n\
            \n\
            - `command` MUST be copied EXACTLY from the list of available command ids you were given — never invent one, never guess at a close spelling, never combine two.\n\
            - When in doubt, refuse. A wrong refusal costs the user one extra step; a wrong route runs the wrong command. Refusing is always the safer answer.\n\
            - `args` is free text — copy the user's own words that follow the command's intent, don't paraphrase or summarize them. Use an empty string when there is nothing left to carry over.\n\
            - Choose at most ONE command. Never chain commands, never describe a sequence of steps, never answer the message yourself — you are only choosing one existing command or declining, nothing else.\n";
        assert_eq!(
            SHIPPED_TEMPLATE, expected,
            "templates/builtin/roles/radio-router.md drifted from the frozen model-facing \
             text (contract 6) — a deliberate edit updates both this golden and the file \
             together. (Compared against the SHIPPED template directly, not through \
             `crate::crew::loader::role_prompt`, which would resolve an operator's own \
             persona override at `~/.darkmux/crew/roles/radio-router.md` instead — see this \
             test's own doc.)"
        );
    }

    // ── validate_router_output — the five canned-output paths ───────────

    #[test]
    fn validate_router_output_valid_route() {
        let raw = "```json\n{\"command\": \"review\", \"args\": \"123\"}\n```";
        let decision = validate_router_output(raw, &fixture_catalog());
        assert_eq!(
            decision,
            RouteDecision::Route { command: "review".to_string(), args: "123".to_string() }
        );
    }

    #[test]
    fn validate_router_output_valid_route_case_insensitive_resolves_to_catalog_case() {
        // Mirrors `crate::acp_panel::route_command`'s own case-insensitive
        // rule — the resolved command is the CATALOG's own casing, never
        // the model's raw text.
        let catalog = vec![entry("Pr-View", "View a PR.", None)];
        let raw = "```json\n{\"command\": \"pr-view\", \"args\": \"\"}\n```";
        let decision = validate_router_output(raw, &catalog);
        assert_eq!(decision, RouteDecision::Route { command: "Pr-View".to_string(), args: String::new() });
    }

    #[test]
    fn validate_router_output_out_of_catalog_command_refuses() {
        let raw = "```json\n{\"command\": \"not-advertised\", \"args\": \"\"}\n```";
        let decision = validate_router_output(raw, &fixture_catalog());
        match decision {
            RouteDecision::Refuse { reason } => assert!(reason.contains("not-advertised"), "{reason}"),
            other => panic!("expected a refusal, got {other:?}"),
        }
    }

    #[test]
    fn validate_router_output_malformed_json_refuses() {
        let raw = "```json\n{not even json\n```";
        let decision = validate_router_output(raw, &fixture_catalog());
        match decision {
            RouteDecision::Refuse { reason } => assert!(reason.contains("valid JSON"), "{reason}"),
            other => panic!("expected a refusal, got {other:?}"),
        }
    }

    #[test]
    fn validate_router_output_no_fenced_block_and_not_bare_json_refuses() {
        let raw = "sure, I'll route this for you: /review";
        let decision = validate_router_output(raw, &fixture_catalog());
        match decision {
            RouteDecision::Refuse { reason } => assert!(reason.contains("valid JSON"), "{reason}"),
            other => panic!("expected a refusal, got {other:?}"),
        }
    }

    #[test]
    fn validate_router_output_explicit_refusal_token_relays_the_reason() {
        let raw = "```json\n{\"refuse\": \"this reads as a question about the codebase in general, not a command\"}\n```";
        let decision = validate_router_output(raw, &fixture_catalog());
        assert_eq!(
            decision,
            RouteDecision::Refuse {
                reason: "this reads as a question about the codebase in general, not a command".to_string()
            }
        );
    }

    /// Regression: a CONFUSED response carrying BOTH `command` AND `refuse`
    /// must refuse, never execute the route. Without `deny_unknown_fields`
    /// on [`RawRouterOutput`], `#[serde(untagged)]`'s first-match-wins
    /// semantics would let this deserialize as `Route` (its own fields are
    /// all present; the model's own `refuse` text would be silently
    /// discarded as an unrecognized field) — the fail-OPEN case wall 6
    /// exists to rule out. Neither shape may claim an object naming a field
    /// it doesn't declare.
    #[test]
    fn validate_router_output_object_with_both_command_and_refuse_keys_refuses() {
        let raw = "```json\n{\"command\": \"review\", \"refuse\": \"too ambiguous to route\"}\n```";
        let decision = validate_router_output(raw, &fixture_catalog());
        assert!(
            matches!(decision, RouteDecision::Refuse { .. }),
            "a confused response naming both `command` and `refuse` must refuse, not route: {decision:?}"
        );
    }

    #[test]
    fn validate_router_output_empty_refuses() {
        let decision = validate_router_output("", &fixture_catalog());
        match decision {
            RouteDecision::Refuse { reason } => assert!(reason.contains("no output"), "{reason}"),
            other => panic!("expected a refusal, got {other:?}"),
        }
        let decision = validate_router_output("   \n  ", &fixture_catalog());
        assert!(matches!(decision, RouteDecision::Refuse { .. }));
    }

    #[test]
    fn validate_router_output_bare_json_without_fence_still_parses() {
        // A forgiving fallback (`extract_json`'s bare-parse branch) — some
        // models skip the fence despite instructions.
        let raw = "{\"command\": \"pr-list\", \"args\": \"\"}";
        let decision = validate_router_output(raw, &fixture_catalog());
        assert_eq!(decision, RouteDecision::Route { command: "pr-list".to_string(), args: String::new() });
    }

    #[test]
    fn validate_router_output_single_line_fenced_block_still_parses() {
        // A small model that emits the fence and its content on ONE
        // physical line (no newlines inside the fence at all) — a
        // plausible shape the old line-by-line scanner would have missed
        // (opener and closer on the same "line" never matched the
        // "everything after the opener line" loop).
        let raw = "```json {\"command\": \"pr-list\", \"args\": \"\"} ```";
        let decision = validate_router_output(raw, &fixture_catalog());
        assert_eq!(decision, RouteDecision::Route { command: "pr-list".to_string(), args: String::new() });
    }

    /// (#1698 Packet B carry-list item 4 — fence extraction strictness)
    /// REFUSE-ON-MULTIPLE-BLOCKS: a response carrying a SECOND fenced json
    /// block after the first one closes must refuse, never silently pick
    /// the first block as authoritative — the model's own instructions
    /// promise exactly one block, and a response that breaks that promise
    /// is exactly the "in doubt" case wall 6 exists to fail closed on.
    #[test]
    fn validate_router_output_multiple_fenced_json_blocks_refuses() {
        let raw = "```json\n{\"command\": \"pr-list\", \"args\": \"\"}\n```\n\
                   On second thought:\n\
                   ```json\n{\"command\": \"review\", \"args\": \"\"}\n```";
        let decision = validate_router_output(raw, &fixture_catalog());
        match decision {
            RouteDecision::Refuse { reason } => assert!(reason.contains("valid JSON"), "{reason}"),
            other => panic!(
                "a response carrying more than one fenced json block must refuse, never guess \
                 which is authoritative: {other:?}"
            ),
        }
    }

    /// (#1698 Packet B2, scope H — the #1702 merge-gate carry item) The
    /// BEFORE-direction specimen: a bare scratch fence PRECEDING the real
    /// ```json block must ALSO refuse — the total-delimiter-count check
    /// (`raw.matches("```").count() > 2`) doesn't care which side of the
    /// real block the extra fence sits on.
    #[test]
    fn validate_router_output_bare_fence_before_the_json_block_refuses() {
        let raw = "```\nscratch thoughts here\n```\n```json\n{\"command\": \"pr-list\", \"args\": \"\"}\n```";
        let decision = validate_router_output(raw, &fixture_catalog());
        assert!(matches!(decision, RouteDecision::Refuse { .. }), "{decision:?}");
    }

    /// The inverted case (red-prove discipline): a SINGLE fenced block
    /// followed by ordinary trailing prose — no second fence anywhere —
    /// must still extract and route normally. Proves the strictness above
    /// fires on a genuine second BLOCK, not on any trailing text at all.
    #[test]
    fn validate_router_output_single_fenced_json_block_with_trailing_prose_still_routes() {
        let raw = "```json\n{\"command\": \"pr-list\", \"args\": \"\"}\n```\nHope that helps!";
        let decision = validate_router_output(raw, &fixture_catalog());
        assert_eq!(decision, RouteDecision::Route { command: "pr-list".to_string(), args: String::new() });
    }

    #[test]
    fn validate_router_output_object_matching_neither_shape_refuses() {
        let raw = "```json\n{\"foo\": \"bar\"}\n```";
        let decision = validate_router_output(raw, &fixture_catalog());
        match decision {
            RouteDecision::Refuse { reason } => assert!(reason.contains("route-or-refuse contract"), "{reason}"),
            other => panic!("expected a refusal, got {other:?}"),
        }
    }

    // ── route() — the empty-catalog short-circuit + the call seam ───────

    #[test]
    fn route_with_empty_catalog_refuses_without_invoking_call() {
        let mut called = false;
        let mut call = |_msg: &str| -> Result<String> {
            called = true;
            Ok("{\"refuse\": \"should never be reached\"}".to_string())
        };
        let decision = route("do something", &[], &mut call);
        assert!(matches!(decision, RouteDecision::Refuse { .. }));
        assert!(!called, "an empty catalog must never dispatch a model call");
    }

    #[test]
    fn route_with_empty_text_refuses_without_invoking_call() {
        let mut called = false;
        let mut call = |_msg: &str| -> Result<String> {
            called = true;
            Ok("{\"refuse\": \"should never be reached\"}".to_string())
        };
        let decision = route("   \n  ", &fixture_catalog(), &mut call);
        assert!(matches!(decision, RouteDecision::Refuse { .. }));
        assert!(!called, "empty/whitespace-only text must never dispatch a model call");
    }

    #[test]
    fn route_relays_a_valid_canned_route_through_the_call_seam() {
        let mut call = |_msg: &str| -> Result<String> { Ok("```json\n{\"command\": \"review\", \"args\": \"\"}\n```".to_string()) };
        let decision = route("please review this", &fixture_catalog(), &mut call);
        assert_eq!(decision, RouteDecision::Route { command: "review".to_string(), args: String::new() });
    }

    /// A routing dispatch that could not run is NOT a refusal. A refusal is
    /// the model declining; this is the model never being reached (no
    /// registry, no model, no server). Callers that fall through to the
    /// answering seat on a refusal would fail identically on an
    /// Unavailable, so the variant has to be distinct (first-run probes,
    /// 2026-08-28: every setup error printed twice and exited 0).
    #[test]
    fn route_turns_a_call_error_into_unavailable_not_a_refusal() {
        let mut call = |_msg: &str| -> Result<String> { Err(anyhow::anyhow!("dispatch failed: no model loaded")) };
        let decision = route("please review this", &fixture_catalog(), &mut call);
        match decision {
            RouteDecision::Unavailable { error } => assert!(error.contains("dispatch failed"), "{error}"),
            other => panic!("expected Unavailable, got {other:?}"),
        }
    }

    // ── compile_catalog (registry fixture) ───────────────────────────────

    #[test]
    #[serial_test::serial]
    fn compile_catalog_advertises_panel_blocked_configs_sorted_with_the_panel_description() {
        let tmp = tempfile::TempDir::new().unwrap();
        let prev = std::env::var("DARKMUX_CREW_DIR").ok();
        // SAFETY: this test is #[serial_test::serial].
        unsafe { std::env::set_var("DARKMUX_CREW_DIR", tmp.path()) };

        let dir = tmp.path().join("mission-configs");
        std::fs::create_dir_all(&dir).unwrap();
        // Two panel-blocked configs (radio-advertised). Each carries a LONG
        // top-level `description` (mimicking `review.json`'s real shape —
        // engineering provenance, not routing text) to prove the catalog
        // reads `panel.description`, never the top-level field.
        std::fs::write(
            dir.join("zzz-last.json"),
            serde_json::to_string(&serde_json::json!({
                "id": "zzz-last",
                "name": "ZZZ Last",
                "description": "Long top-level engineering provenance text, never read by the router.",
                "panel": {"description": "Short palette label", "hint": "some hint"},
                "phases": []
            }))
            .unwrap(),
        )
        .unwrap();
        std::fs::write(
            dir.join("aaa-first.json"),
            serde_json::to_string(&serde_json::json!({
                "id": "aaa-first",
                "name": "AAA First",
                "description": "Another long top-level description, also never read by the router.",
                "panel": {"description": "Another short label"},
                "phases": []
            }))
            .unwrap(),
        )
        .unwrap();
        // ... and one WITHOUT a panel block — must not be advertised.
        std::fs::write(
            dir.join("not-advertised.json"),
            serde_json::to_string(&serde_json::json!({
                "id": "not-advertised",
                "name": "Not Advertised",
                "description": "This config carries no panel block.",
                "phases": []
            }))
            .unwrap(),
        )
        .unwrap();

        // The built-in `review` config also declares a `panel` block (see
        // `templates/builtin/mission-configs/review.json`), so it's ALWAYS
        // merged in alongside the two user-tier fixtures above — this test
        // asserts the fixtures' own ordering/content/exclusion rather than
        // the full catalog contents, so it doesn't drift if a future
        // built-in also opts into the panel.
        let catalog = compile_catalog();
        assert!(
            !catalog.iter().any(|c| c.id == "not-advertised"),
            "a config without a `panel` block must never be advertised: {catalog:?}"
        );
        let ids: Vec<&str> = catalog.iter().map(|c| c.id.as_str()).collect();
        assert!(ids.is_sorted(), "catalog must be id-sorted: {ids:?}");
        let aaa = catalog.iter().find(|c| c.id == "aaa-first").expect("aaa-first must be advertised");
        assert_eq!(aaa.description, "Another short label", "must read panel.description, not the top-level field");
        assert_eq!(aaa.hint, None);
        let zzz = catalog.iter().find(|c| c.id == "zzz-last").expect("zzz-last must be advertised");
        assert_eq!(zzz.description, "Short palette label", "must read panel.description, not the top-level field");
        assert_eq!(zzz.hint.as_deref(), Some("some hint"));
        // `aaa-first` sorts before `zzz-last` (both are also confirmed
        // present above); the built-in `review` sorts between them.
        let aaa_pos = ids.iter().position(|id| *id == "aaa-first").unwrap();
        let zzz_pos = ids.iter().position(|id| *id == "zzz-last").unwrap();
        assert!(aaa_pos < zzz_pos, "aaa-first must sort before zzz-last: {ids:?}");

        // SAFETY: this test is #[serial_test::serial].
        unsafe {
            match prev {
                Some(v) => std::env::set_var("DARKMUX_CREW_DIR", v),
                None => std::env::remove_var("DARKMUX_CREW_DIR"),
            }
        }
    }

    #[test]
    #[serial_test::serial]
    fn compile_catalog_falls_back_to_the_configs_name_when_panel_description_is_absent() {
        let tmp = tempfile::TempDir::new().unwrap();
        let prev = std::env::var("DARKMUX_CREW_DIR").ok();
        // SAFETY: this test is #[serial_test::serial].
        unsafe { std::env::set_var("DARKMUX_CREW_DIR", tmp.path()) };

        let dir = tmp.path().join("mission-configs");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("no-panel-desc.json"),
            serde_json::to_string(&serde_json::json!({
                "id": "no-panel-desc",
                "name": "No Panel Desc",
                "description": "A long top-level description that must NOT be read here either.",
                "panel": {},
                "phases": []
            }))
            .unwrap(),
        )
        .unwrap();

        // The built-in `review` config is also always merged in (see the
        // sibling test's comment) — find this fixture's own entry rather
        // than asserting the catalog's total length. `PanelConfig::
        // description` absent falls back to `MissionConfig.name`
        // (`crate::acp_panel::list_panel_commands`'s own rule) — never to
        // the top-level `description`.
        let catalog = compile_catalog();
        let found = catalog.iter().find(|c| c.id == "no-panel-desc").expect("no-panel-desc must be advertised");
        assert_eq!(found.description, "No Panel Desc");

        // SAFETY: this test is #[serial_test::serial].
        unsafe {
            match prev {
                Some(v) => std::env::set_var("DARKMUX_CREW_DIR", v),
                None => std::env::remove_var("DARKMUX_CREW_DIR"),
            }
        }
    }
}
