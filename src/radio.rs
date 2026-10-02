//! The `radio` interpreter core (#1698 Packet A) — surface-neutral ENGINE
//! capability, never ACP-specific. `src/acp_panel.rs`'s ratified doc line:
//! "the interpreter is ENGINE capability; ACP is its first consumer, the
//! CLI its second." Two independent consumers exist (or will): the CLI verb
//! (`src/radio_cli.rs`, this packet) and the ACP no-slash channel
//! (Packet B). Both call INTO this module; neither owns any piece of it.
//! This module in turn calls INTO `crate::acp_panel` (catalog enumeration,
//! `plan_launch`, `run_ephemeral`) rather than duplicating that logic:
//! there is exactly one place that decides which mission configs are
//! launchable, and exactly one place that decides how a launch runs. A
//! routed command is a config id, and executing it is `mission launch <id>`
//! (the panel's `/mission launch <id>`).
//!
//! # The two-seat receiver architecture (issue #1698, "naming ratified")
//!
//! "You key the CHANNEL, never a model — who's tuned in is config." Two
//! seats exist per the role-family doctrine:
//!
//! - **The ROUTING seat** (this module, Packet A) — bounded classification:
//!   free text + the advertised catalog in, one command id + args (or a
//!   refusal) out. (#2914) A UTILITY job: it runs on the machine's one
//!   utility model (`internal.utility` in profiles.json) through the lean
//!   utility path (`crate::crew::utility::run_utility_single_shot`) — no
//!   profile, no session, no dispatch bookends, no run; just its usage
//!   record. The `radio-router` role (`role_family: "utility"`) supplies
//!   the frozen system prompt.
//! - **The ANSWERING seat** (Packet B) — reasoning-bearing grounded answers
//!   over session artifacts. Not built here. Still ordinary WORK: a full
//!   dispatch and a run, staffed through `radio.answerer_profile` /
//!   `role_profiles.radio-host`.
//!
//! **Staffing history, so the 4.0 shape is not re-litigated.** Packet A
//! resolved the routing seat like any other role (`role_profiles.
//! radio-router`, else `default_profile`); Packet B2 added
//! `radio.router_profile` on top. Both are REMOVED in 4.0 (#2914, a clean
//! break): routing is darkmux's own job, and darkmux's own jobs run on the
//! utility model, declared once with its window. `darkmux doctor` names a
//! leftover setting; `config set role_profiles.radio-router` is refused.
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
use darkmux_flow::payload::{RadioDecision, RadioRoutePayload};
use serde::Deserialize;

/// One entry in the model-facing command catalog — what the routing seat
/// is allowed to choose among. Compiled fresh per call by [`compile_catalog`]
/// (never cached across calls: the registry can change between the CLI
/// process starting and the router dispatch actually running).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CatalogEntry {
    /// The REGISTRY-RESOLVABLE id — see `crate::acp_panel::LaunchableConfig::id`'s
    /// own doc for why this is never the document body's own `id` field.
    pub id: String,
    /// The model-grounding text for THIS catalog: the config's one-line
    /// summary (`crate::acp_panel::config_summary`: the first sentence of its
    /// `description`, else its `name`), the same line `/mission list` prints.
    ///
    /// **Deliberately NOT the whole `MissionConfig.description`**: the
    /// built-in `review` config's is ~2KB of engineering provenance (crate
    /// paths, issue numbers, `StepKind` names), and CLAUDE.md's "Model-facing
    /// prompt construction" audit question ("what does this read as to a
    /// fresh-context model with no darkmux history?") answers badly for it.
    /// A config's description therefore leads with one plain sentence saying
    /// what it does for the user.
    pub description: String,
    /// An input hint shown to the router: `(no arguments)` for a command that
    /// takes no text after its id, none otherwise.
    pub hint: Option<String>,
    /// Whether the command takes text after its id
    /// (`mission_config::takes_panel_args`: the config reads
    /// `__panel_args__`). When `false`, [`validate_router_output`] DROPS
    /// whatever `args` the routing seat carried over rather than forwarding
    /// them.
    ///
    /// The seat is a 4B classifier reading one line of description text,
    /// and it copies trailing words by default: measured live, `review my
    /// working tree diff` routed to `review` carrying `"my working tree
    /// diff"` as its argument, to a config with nothing to receive it.
    /// Enforcing the declaration here, rather than only instructing the
    /// model, is what makes it a guarantee instead of a request: the model
    /// is free to keep getting this wrong and the command still runs
    /// correctly.
    pub accepts_args: bool,
}

/// The hint the router sees for a command that takes no text after its id.
const NO_ARGUMENTS_HINT: &str = "(no arguments)";

/// Compile the model-facing catalog from `crate::acp_panel::list_launchable`,
/// the SAME enumeration `/mission list` prints (single derivation of "what
/// can be launched"). A pure `map`: no second registry load, no divergent
/// filter. `list_launchable` sorts by id, and this preserves that order.
/// `Err` is the launch refusal that empties the listing (one stale user-tier
/// config blocks every launch): the catalog is empty and this is why.
pub fn compile_catalog() -> Result<Vec<CatalogEntry>> {
    Ok(crate::acp_panel::list_launchable()?
        .into_iter()
        .map(|config| CatalogEntry {
            id: config.id,
            description: config.summary,
            hint: (!config.accepts_args).then(|| NO_ARGUMENTS_HINT.to_string()),
            accepts_args: config.accepts_args,
        })
        .collect())
}

/// The routing seat's decision for one exchange — see this module's doc on
/// walls 2 and 6. There is no third variant and no `Result`: every failure
/// mode this module can encounter collapses into `Refuse`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RouteDecision {
    /// `command` is guaranteed to be one of `catalog`'s own `id` strings,
    /// copied from the CATALOG entry (never the model's raw text) after a
    /// case-insensitive match — see [`validate_router_output`]'s doc for
    /// why this matches config ids case-insensitively.
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
pub use darkmux_flow::payload::RadioSurface;

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
/// [`RouteDecision::Refuse`] — emitted AFTER the decision is known, a
/// single record per invocation, not a start/complete pair. (#2914) The
/// underlying model call is a UTILITY job on the lean path
/// ([`dispatch_router_call`] -> `crate::crew::utility::run_utility_single_shot`):
/// it leaves its `telemetry.tokens` usage record and NO dispatch bookends,
/// so this record is the only routing-level record there is — a record
/// about the routing OUTCOME, never a liveness pair (the amended contract
/// 2: bookends are for work executions).
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

/// (#2917) How long a routing call may run before the surface SAYS what is
/// happening. The router waits behind whatever occupies the one utility
/// instance (a compaction, by the operator's decision on #2914 — option
/// A); this bound is where the wait stops being silent, not where it
/// ends. Past it, `radio_cli.rs`/`acp.rs` print
/// `crate::radio_busy::router_wait_notice_live` once and keep waiting to
/// [`ROUTER_CALL_CEILING_SECONDS`].
///
/// **Why 10s.** A route alone measured 0.08s (#2914's table); a cold load
/// of a 4B utility model at its window is a few seconds; a routing call
/// queued behind a real compaction measured 13.9s live and 35–82s in the
/// bench. Ten seconds sits above the cold-load case, so a first call after
/// the utility model was unloaded never gets a false "busy" line, and
/// well under the shortest measured queue, so a genuine wait is named
/// early. **A constant, not a knob**, following this codebase's pattern for
/// an ANNOUNCE point (`radio_cli::FORWARD_SIGNAL_GRACE`): the knobs in
/// `docs/ENVIRONMENT.md` bound child processes (`DARKMUX_MODEL_LOAD_
/// TIMEOUT_SECONDS`, `DARKMUX_STEP_COMMAND_TIMEOUT_SECONDS`) and change
/// what happens; this changes only when the operator is told.
pub const ROUTER_SLOW_NOTICE_AFTER: std::time::Duration = std::time::Duration::from_secs(10);

/// The notice bound the surfaces actually use: [`ROUTER_SLOW_NOTICE_AFTER`],
/// unless the TEST hook `DARKMUX_TEST_RADIO_NOTICE_AFTER_MS` names a
/// shorter one. Not an operator knob (see the constant's doc for why not):
/// the hook exists so a CLI test can drive the real binary past the bound
/// in well under a second instead of ten, the same pattern as the other
/// `DARKMUX_TEST_*` hooks. An unparseable value is ignored.
pub fn router_slow_notice_after() -> std::time::Duration {
    std::env::var("DARKMUX_TEST_RADIO_NOTICE_AFTER_MS")
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .map(std::time::Duration::from_millis)
        .unwrap_or(ROUTER_SLOW_NOTICE_AFTER)
}

/// The routing call's hard ceiling (the curl `-m`) — [`dispatch_router_call`]
/// has always bounded the call here; named so the slow-notice copy can say
/// how long "keep waiting" is.
pub const ROUTER_CALL_CEILING_SECONDS: u32 = 300;

/// (#2917) [`route_and_record_bounded`]'s answer: the decision, or — when
/// the routing call is still running past the notice bound — the receiver
/// it will arrive on, so the caller can say what is happening and then
/// block on it.
pub enum RouteWait {
    Done(RouteDecision),
    Slow(std::sync::mpsc::Receiver<RouteDecision>),
}

/// (#2917) [`route_and_record`] on its own thread, handing back either the
/// decision (when it arrives within `notice_after`) or the still-pending
/// receiver. The routing call itself is unchanged — same catalog, same
/// record, same ceiling; only WHO waits moved, so the caller's thread is
/// free to speak. A routing thread that ends without a decision (a panic
/// inside the call) reads as [`RouteDecision::Unavailable`], never a hang:
/// wall 6 holds on this path too.
pub fn route_and_record_bounded(
    text: String,
    catalog: Vec<CatalogEntry>,
    surface: RadioSurface,
    call: impl Fn(&str) -> Result<String> + Send + 'static,
    notice_after: std::time::Duration,
) -> RouteWait {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let decision = route_and_record(&text, &catalog, surface, &mut |message: &str| call(message));
        let _ = tx.send(decision);
    });
    match rx.recv_timeout(notice_after) {
        Ok(decision) => RouteWait::Done(decision),
        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => RouteWait::Slow(rx),
        Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => RouteWait::Done(routing_thread_ended_early()),
    }
}

/// The decision a routing thread that died without sending one reads as.
pub fn routing_thread_ended_early() -> RouteDecision {
    RouteDecision::Unavailable { error: "the routing call ended without a decision".to_string() }
}

/// Build + write wall 4's flow record for one routed invocation. Best-
/// effort — a flow-write failure (e.g. an unwritable flows dir) must never
/// turn a successful route into a failed one, so the `Result` from
/// `crate::flow::record` is intentionally discarded here, same posture
/// every other flow-record emission site in this codebase takes.
fn emit_route_record(text: &str, surface: RadioSurface, decision: &RouteDecision) {
    let truncated: String = text.chars().take(SOURCE_TEXT_RECORD_CAP).collect();
    let mut payload = RadioRoutePayload {
        surface,
        source_text: truncated,
        decision: RadioDecision::Route,
        command: None,
        args: None,
        reason: None,
        error: None,
        step_id: None,
    };
    match decision {
        RouteDecision::Route { command, args } => {
            payload.decision = RadioDecision::Route;
            payload.command = Some(command.clone());
            payload.args = Some(args.clone());
        }
        RouteDecision::Refuse { reason } => {
            payload.decision = RadioDecision::Refuse;
            payload.reason = Some(reason.clone());
        }
        RouteDecision::Unavailable { error } => {
            payload.decision = RadioDecision::Unavailable;
            payload.error = Some(error.clone());
        }
    }
    let record = crate::crew::dispatch::build_session_record(
        crate::flow::Level::Info,
        crate::crew::loader::RADIO_ROUTER_ROLE_ID,
        &radio_session(crate::crew::loader::RADIO_ROUTER_ROLE_ID),
        None,
        darkmux_flow::Payload::RadioRoute(payload),
    );
    let _ = crate::flow::record(record);
}

/// A radio seat's session: an ad-hoc dispatch of `role` in the standalone
/// `radio` run. Radio has no mission instance, so its records carry no
/// `mission_id`; each seat's nonce keeps it unique.
pub(crate) fn radio_session(role: &str) -> crate::types::session_id::SessionId {
    let run = crate::types::session_id::RunId::standalone("radio").expect("a literal run id is never empty");
    crate::types::session_id::SessionId::adhoc(run, role, crate::crew::dispatch::fresh_nonce())
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
            // (wall 2) Case-insensitive match against the CATALOG — the
            // resolved `command` is the CATALOG entry's own (correctly-cased) id,
            // never the model's raw text, so a model that echoes back a
            // differently-cased id still resolves correctly while an
            // out-of-catalog id still refuses.
            match catalog.iter().find(|c| c.id.eq_ignore_ascii_case(&command)) {
                // (#2050) A command that takes no text after its id
                // (`accepts_args: false`) receives NO arguments, whatever the
                // seat carried over. Same posture as wall 2 one field across:
                // the model's claim is checked against the catalog rather
                // than trusted, here for `args` as there for `command`.
                //
                // **Silently, and deliberately: unlike `/mission launch
                // <id> text`**, which refuses text a config cannot take
                // (`acp_panel::map_launch_args`). These `args` are not text
                // the operator typed. They are the ROUTING SEAT'S extraction
                // from a free-text message, and the operator's own message is
                // neither discarded nor hidden: it is what produced the route,
                // and the caller has already echoed "routing to /mission
                // launch <id> — from your text". A notice here would report
                // the loss of something the operator never wrote and cannot
                // see, which is noise rather than provenance.
                Some(entry) if !entry.accepts_args => RouteDecision::Route {
                    command: entry.id.clone(),
                    args: String::new(),
                },
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
/// instructed output shape and the retired `mission propose` verb's `extract_json_block`'s
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
/// Deliberately simpler than the retired `mission propose` extractor (no
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

/// The production [`ModelCall`] implementation — ONE utility call on the
/// machine's utility model (#2914), through
/// `crate::crew::utility::run_utility_single_shot`: the `radio-router`
/// role's frozen system prompt, this module's assembled user message, a
/// bounded ceiling, and the binding's own model + window. LEAN by the
/// amended contract 2: the call leaves its `telemetry.tokens` usage record
/// (`purpose: utility`, `handle: radio-router`) and nothing else — no
/// session, no `dispatch start`/`complete`, no run. Wall 4's own flow
/// record (source text + chosen route/refusal + surface) is a SEPARATE,
/// higher-level record — see [`route_and_record`]/[`emit_route_record`]
/// above.
///
/// Pre-#2914 this rode `dispatch_local_single_shot` through the fleet
/// routing seam with a profile resolved from `radio.router_profile` /
/// `role_profiles.radio-router`, which put 54 `radio-router` runs on the
/// operator's runs board in a day and let the routing seat be staffed on
/// any model at all. Both knobs are gone; the binding is the staffing.
///
/// [`ROUTER_CALL_CEILING_SECONDS`] — a deliberately BOUNDED ceiling for a
/// bounded-classification call. A busy utility instance (a compaction in
/// flight on it) makes this WAIT, by the operator's decision on the issue;
/// #2915 shows why, and (#2917) past [`ROUTER_SLOW_NOTICE_AFTER`] the
/// surfaces say so.
pub fn dispatch_router_call(message: &str) -> Result<String> {
    let reply = crate::crew::utility::run_utility_single_shot(&crate::crew::utility::UtilityJob {
        role_id: crate::crew::loader::RADIO_ROUTER_ROLE_ID,
        message,
        timeout_seconds: ROUTER_CALL_CEILING_SECONDS,
        // The routing seat answers with one small fenced JSON object; the
        // work single-shot primitive's 4096 default was never needed here.
        max_tokens: 1024,
        config_path: None,
        base_url_override: None,
    })?;
    Ok(reply.content)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(id: &str, description: &str, hint: Option<&str>) -> CatalogEntry {
        CatalogEntry {
            id: id.to_string(),
            description: description.to_string(),
            hint: hint.map(str::to_string),
            // An entry whose config reads `__panel_args__` (#2050) — what
            // every config authored before that field existed compiles to.
            accepts_args: true,
        }
    }

    /// (#2050) A catalog entry for a command that declares
    /// `accepts_args: false` — the built-in `review`'s own shape.
    fn entry_taking_no_args(id: &str, description: &str, hint: Option<&str>) -> CatalogEntry {
        CatalogEntry { accepts_args: false, ..entry(id, description, hint) }
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
        // precedence every dispatch honors (`~/.darkmux/roles/
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
            - Match on MEANING, not wording. A description states what a command does in one phrasing; the user asks in their own. A message asking for what a description describes — in ordinary synonyms, a paraphrase, or a shorter or longer form of the same request — names that command, and should be routed to it.\n\
            - A message that names a listed command by its id or its title (for example \"launch the review mission on my branch\") asks for that command. Extra words around the name, such as a branch, a file, or a place, are not a reason to refuse.\n\
            - When in doubt, refuse. A wrong refusal costs the user one extra step; a wrong route runs the wrong command. Refusing is always the safer answer. \"In doubt\" means you cannot tell which command is being asked for, or whether any is — it does not mean the user's words differ from the description's.\n\
            - `args` is free text — copy the user's own words that follow the command's intent, don't paraphrase or summarize them. Use an empty string when there is nothing left to carry over.\n\
            - Choose at most ONE command. Never chain commands, never describe a sequence of steps, never answer the message yourself — you are only choosing one existing command or declining, nothing else.\n";
        assert_eq!(
            SHIPPED_TEMPLATE, expected,
            "templates/builtin/roles/radio-router.md drifted from the frozen model-facing \
             text (contract 6) — a deliberate edit updates both this golden and the file \
             together. (Compared against the SHIPPED template directly, not through \
             `crate::crew::loader::role_prompt`, which would resolve an operator's own \
             persona override at `~/.darkmux/roles/radio-router.md` instead — see this \
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

    // ── (#2050) `accepts_args: false` is enforced, not requested ───

    #[test]
    fn validate_router_output_drops_args_for_a_command_that_takes_none() {
        // Measured live on 3.7.1: `review my working tree diff` routed and
        // carried `"my working tree diff"` into a config whose hint reads
        // `(no arguments)`. The seat is free to keep doing that; the
        // command still runs correctly.
        let catalog = vec![entry_taking_no_args("review", "Code review of the current changes.", Some("(no arguments)"))];
        let raw = "```json\n{\"command\": \"review\", \"args\": \"my working tree diff\"}\n```";
        assert_eq!(
            validate_router_output(raw, &catalog),
            RouteDecision::Route { command: "review".to_string(), args: String::new() },
            "a command declaring it takes no arguments must receive none"
        );
    }

    #[test]
    fn validate_router_output_keeps_args_for_a_command_that_takes_them() {
        // The inverted case: a rule that cleared `args` unconditionally
        // would pass the test above just as happily, and would silently
        // break every command that actually reads its argument.
        let catalog = vec![entry("pr-view", "Show one pull request.", Some("a PR number"))];
        let raw = "```json\n{\"command\": \"pr-view\", \"args\": \"482\"}\n```";
        assert_eq!(
            validate_router_output(raw, &catalog),
            RouteDecision::Route { command: "pr-view".to_string(), args: "482".to_string() },
            "an `accepts_args` still forwards the seat's argument verbatim"
        );
    }

    #[test]
    fn validate_router_output_valid_route_case_insensitive_resolves_to_catalog_case() {
        // Config ids match case-insensitively:
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
            // flow-action-guard:allow — error prose, not an action
            RouteDecision::Unavailable { error } => assert!(error.contains("dispatch failed"), "{error}"),
            other => panic!("expected Unavailable, got {other:?}"),
        }
    }

    // ── compile_catalog (registry fixture) ───────────────────────────────

    /// (#2917) A routing call that outlives the notice bound hands back the
    /// receiver — the caller's cue to say what is happening — and the
    /// decision still arrives on it, unchanged. Wall-clock by construction
    /// (`recv_timeout`), so the margins are wide: the call holds for 20x
    /// the bound. No fixed timestamps anywhere: nothing here depends on
    /// what day or hour the suite runs.
    #[test]
    #[serial_test::serial]
    fn route_and_record_bounded_hands_back_the_receiver_when_the_call_outlives_the_bound() {
        let flows = tempfile::TempDir::new().unwrap();
        let prev = std::env::var("DARKMUX_FLOWS_DIR").ok();
        // SAFETY: this test is #[serial_test::serial].
        unsafe { std::env::set_var("DARKMUX_FLOWS_DIR", flows.path()) };

        let catalog = fixture_catalog();
        let slow = |_msg: &str| -> Result<String> {
            std::thread::sleep(std::time::Duration::from_millis(400));
            Ok("```json\n{\"command\": \"review\", \"args\": \"\"}\n```".to_string())
        };
        let wait = route_and_record_bounded(
            "review this".to_string(),
            catalog,
            RadioSurface::Cli,
            slow,
            std::time::Duration::from_millis(20),
        );
        let RouteWait::Slow(rx) = wait else {
            panic!("a call still running past the bound must hand back the receiver, not a decision");
        };
        assert_eq!(
            rx.recv().expect("the decision still arrives"),
            RouteDecision::Route { command: "review".to_string(), args: String::new() }
        );

        // SAFETY: this test is #[serial_test::serial].
        unsafe {
            match prev {
                Some(v) => std::env::set_var("DARKMUX_FLOWS_DIR", v),
                None => std::env::remove_var("DARKMUX_FLOWS_DIR"),
            }
        }
    }

    /// (#2917) The inverse: a call that answers inside the bound is handed
    /// back as the decision itself — no notice, no second wait. The bound
    /// here is 5s against a call that returns at once.
    #[test]
    #[serial_test::serial]
    fn route_and_record_bounded_hands_back_the_decision_when_the_call_is_prompt() {
        let flows = tempfile::TempDir::new().unwrap();
        let prev = std::env::var("DARKMUX_FLOWS_DIR").ok();
        // SAFETY: this test is #[serial_test::serial].
        unsafe { std::env::set_var("DARKMUX_FLOWS_DIR", flows.path()) };

        let prompt = |_msg: &str| -> Result<String> { Ok("```json\n{\"refuse\": \"no\"}\n```".to_string()) };
        let wait = route_and_record_bounded(
            "hello".to_string(),
            fixture_catalog(),
            RadioSurface::Cli,
            prompt,
            std::time::Duration::from_secs(5),
        );
        let RouteWait::Done(decision) = wait else {
            panic!("a prompt call must hand back its decision, not a receiver");
        };
        assert_eq!(decision, RouteDecision::Refuse { reason: "no".to_string() });

        // SAFETY: this test is #[serial_test::serial].
        unsafe {
            match prev {
                Some(v) => std::env::set_var("DARKMUX_FLOWS_DIR", v),
                None => std::env::remove_var("DARKMUX_FLOWS_DIR"),
            }
        }
    }

    /// (#2917) Wall 6 on the threaded path: a routing thread that dies
    /// without a decision reads as `Unavailable`, never a hang on `recv`.
    #[test]
    #[serial_test::serial]
    fn route_and_record_bounded_reads_a_dead_routing_thread_as_unavailable() {
        let flows = tempfile::TempDir::new().unwrap();
        let prev = std::env::var("DARKMUX_FLOWS_DIR").ok();
        // SAFETY: this test is #[serial_test::serial].
        unsafe { std::env::set_var("DARKMUX_FLOWS_DIR", flows.path()) };

        let dies = |_msg: &str| -> Result<String> { panic!("the routing call blew up") };
        let wait = route_and_record_bounded(
            "hello".to_string(),
            fixture_catalog(),
            RadioSurface::Cli,
            dies,
            std::time::Duration::from_secs(5),
        );
        let RouteWait::Done(decision) = wait else {
            panic!("a dead routing thread must resolve, not hand back a receiver nobody will send on");
        };
        assert!(matches!(decision, RouteDecision::Unavailable { .. }), "{decision:?}");

        // SAFETY: this test is #[serial_test::serial].
        unsafe {
            match prev {
                Some(v) => std::env::set_var("DARKMUX_FLOWS_DIR", v),
                None => std::env::remove_var("DARKMUX_FLOWS_DIR"),
            }
        }
    }

    /// Every config the registry can load is in the catalog, sorted by id,
    /// described by the first sentence of its `description`, and carrying the
    /// no-arguments hint exactly when it reads no `__panel_args__` task.
    #[test]
    #[serial_test::serial]
    fn compile_catalog_lists_every_launchable_config_sorted_with_its_summary_sentence() {
        let tmp = tempfile::TempDir::new().unwrap();
        let prev = std::env::var("DARKMUX_HOME").ok();
        // SAFETY: this test is #[serial_test::serial].
        unsafe { std::env::set_var("DARKMUX_HOME", tmp.path()) };

        let dir = tmp.path().join("mission-configs");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("zzz-last.json"),
            serde_json::to_string(&serde_json::json!({
                "id": "zzz-last",
                "name": "ZZZ Last",
                "description": "Does the last thing. Then a long provenance paragraph the router must never read.",
                "phases": [{"id": "p", "tasks": [
                    {"id": "t", "reads": ["__panel_args__"], "steps": [{"id": "s", "kind": "procedural.noop"}]}
                ]}]
            }))
            .unwrap(),
        )
        .unwrap();
        std::fs::write(
            dir.join("aaa-first.json"),
            serde_json::to_string(&serde_json::json!({
                "id": "aaa-first",
                "name": "AAA First",
                "description": "Does the first thing. More provenance.",
                "phases": []
            }))
            .unwrap(),
        )
        .unwrap();

        let catalog = compile_catalog().expect("no stale user files in this fixture");
        let ids: Vec<&str> = catalog.iter().map(|c| c.id.as_str()).collect();
        assert!(ids.is_sorted(), "catalog must be id-sorted: {ids:?}");
        for builtin in ["review", "machine-status"] {
            assert!(ids.contains(&builtin), "the built-in `{builtin}` must be launchable: {ids:?}");
        }
        let aaa = catalog.iter().find(|c| c.id == "aaa-first").expect("aaa-first must be listed");
        assert_eq!(aaa.description, "Does the first thing.");
        assert_eq!(aaa.hint.as_deref(), Some("(no arguments)"));
        assert!(!aaa.accepts_args);
        let zzz = catalog.iter().find(|c| c.id == "zzz-last").expect("zzz-last must be listed");
        assert_eq!(zzz.description, "Does the last thing.");
        assert_eq!(zzz.hint, None, "a config that reads `__panel_args__` takes text");
        assert!(zzz.accepts_args);

        // SAFETY: this test is #[serial_test::serial].
        unsafe {
            match prev {
                Some(v) => std::env::set_var("DARKMUX_HOME", v),
                None => std::env::remove_var("DARKMUX_HOME"),
            }
        }
    }

    /// (#2918) "Which models are loaded?" was refused because nothing in
    /// the catalog answered it: `darkmux machine status` does, but it was
    /// not a catalog entry. The built-in `machine-status` config
    /// (`templates/builtin/mission-configs/machine-status.json`) is in every
    /// catalog like any other launchable config, never a special case in the
    /// router prompt, so this holds with NO user-tier configs at all.
    #[test]
    #[serial_test::serial]
    fn compile_catalog_advertises_the_built_in_machine_status_command() {
        let tmp = tempfile::TempDir::new().unwrap();
        let prev = std::env::var("DARKMUX_HOME").ok();
        // SAFETY: this test is #[serial_test::serial].
        unsafe { std::env::set_var("DARKMUX_HOME", tmp.path()) };

        let catalog = compile_catalog().expect("no stale user files in this fixture");
        let entry = catalog
            .iter()
            .find(|c| c.id == "machine-status")
            .unwrap_or_else(|| panic!("the built-in machine-status command must be advertised (#2918): {catalog:?}"));
        assert!(
            entry.description.to_ascii_lowercase().contains("loaded"),
            "the router reads the config summary, which must name the question it answers: {}",
            entry.description
        );
        assert!(!entry.accepts_args, "`machine status` takes no arguments");

        // SAFETY: this test is #[serial_test::serial].
        unsafe {
            match prev {
                Some(v) => std::env::set_var("DARKMUX_HOME", v),
                None => std::env::remove_var("DARKMUX_HOME"),
            }
        }
    }

    /// (F12) The router's whole view of the built-in commands: a phrase that
    /// names a mission by its id or title must find that mission's line, and
    /// the line must say what the mission does in plain words (not clipped
    /// before the point, not leading with graph internals).
    #[test]
    #[serial_test::serial]
    fn the_router_message_names_each_builtin_mission_by_id_and_in_plain_words() {
        let tmp = tempfile::TempDir::new().unwrap();
        let prev = std::env::var("DARKMUX_HOME").ok();
        // SAFETY: this test is #[serial_test::serial].
        unsafe { std::env::set_var("DARKMUX_HOME", tmp.path()) };
        let catalog = compile_catalog().expect("no stale user files in this fixture");
        let message = build_router_message("launch the review mission on my branch", &catalog);
        let line = |id: &str| {
            let prefix = format!("- {id}: ");
            message.lines().find(|l| l.starts_with(&prefix)).unwrap_or_else(|| panic!("no `{id}` line: {message}")).to_ascii_lowercase()
        };
        assert!(line("review").contains("review"), "{}", line("review"));
        let coder = line("coder-phase");
        assert!(coder.contains("failing test") && coder.contains("code"), "a fix-the-test request must find coder-phase: {coder}");
        assert!(!coder.contains("3-task") && !coder.contains('\u{2026}'), "no graph internals, not clipped: {coder}");
        // SAFETY: this test is #[serial_test::serial].
        unsafe {
            match prev {
                Some(v) => std::env::set_var("DARKMUX_HOME", v),
                None => std::env::remove_var("DARKMUX_HOME"),
            }
        }
    }

    /// (F12) A removed verb must not appear anywhere a model reads the command
    /// surface: the router catalog message or the verb index the answering seat
    /// is grounded in (the dogfood suggested the removed `lab eval`).
    #[test]
    #[serial_test::serial]
    fn no_retired_verb_appears_in_the_router_catalog_or_the_verb_index() {
        let tmp = tempfile::TempDir::new().unwrap();
        let prev = std::env::var("DARKMUX_HOME").ok();
        // SAFETY: this test is #[serial_test::serial].
        unsafe { std::env::set_var("DARKMUX_HOME", tmp.path()) };
        let catalog = compile_catalog().expect("no stale user files in this fixture");
        let index = crate::radio_index::render_verb_index(&crate::radio_index::build_verb_index(&<crate::cli::Cli as clap::CommandFactory>::command()));
        let surface = format!("{}\n{index}", build_router_message("x", &catalog));
        let retired = crate::retired_verbs::retired_spellings();
        assert!(retired.iter().any(|s| s == "darkmux lab eval"), "the table must include lab eval: {retired:?}");
        for spelling in retired {
            assert!(!surface.contains(&spelling), "`{spelling}` is retired but the model-facing surface offers it");
        }
        // SAFETY: this test is #[serial_test::serial].
        unsafe {
            match prev {
                Some(v) => std::env::set_var("DARKMUX_HOME", v),
                None => std::env::remove_var("DARKMUX_HOME"),
            }
        }
    }

    #[test]
    #[serial_test::serial]
    fn compile_catalog_falls_back_to_the_configs_name_when_it_has_no_description() {
        let tmp = tempfile::TempDir::new().unwrap();
        let prev = std::env::var("DARKMUX_HOME").ok();
        // SAFETY: this test is #[serial_test::serial].
        unsafe { std::env::set_var("DARKMUX_HOME", tmp.path()) };

        let dir = tmp.path().join("mission-configs");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("no-desc.json"),
            serde_json::to_string(&serde_json::json!({"id": "no-desc", "name": "No Desc", "phases": []})).unwrap(),
        )
        .unwrap();

        let catalog = compile_catalog().expect("no stale user files in this fixture");
        let found = catalog.iter().find(|c| c.id == "no-desc").expect("no-desc must be listed");
        assert_eq!(found.description, "No Desc");

        // SAFETY: this test is #[serial_test::serial].
        unsafe {
            match prev {
                Some(v) => std::env::set_var("DARKMUX_HOME", v),
                None => std::env::remove_var("DARKMUX_HOME"),
            }
        }
    }
}
