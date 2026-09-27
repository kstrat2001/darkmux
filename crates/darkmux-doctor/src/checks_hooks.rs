//! (#2093) The flow-record hook sink's doctor rows: whether hooks are
//! enabled (with provenance), one row per configured rule, and any outbox
//! file no current rule owns. Every rule-level problem is attached to the
//! rule it names (`hooks.rule.<index>`), with the overview row (`hooks`)
//! carrying the worst status across them.

use crate::{Check, Status};
use darkmux_flow::hooks::HookRuleSummary;
use darkmux_flow::{FlowAction, FlowScope};
use darkmux_types::config::{HookMatch, HookRule};
use std::collections::HashSet;
use std::path::Path;

/// (#2093) Surface the flow-record hook sink's resolved state: whether it's
/// enabled (with provenance), and — when it is — one row per configured rule
/// naming its match, destination, and undelivered-line count, with every
/// problem flagged on the row of the rule it belongs to (the same shape
/// `eureka_checks()` established for a check family with more than one
/// member). Provenance distinguishes `env` / `config.json` / `default`.
pub(crate) fn check_hooks() -> Vec<Check> {
    // (#2450 review) Provenance comes from `config_access`, which owns the
    // `env > config.json > default` ladder for every setting and has the
    // #811 test seam a direct `DarkmuxConfig::load_resolved()` read lacks.
    let provenance = darkmux_types::config_access::hooks_enabled_provenance();
    let enabled = darkmux_types::config_access::hooks_enabled();
    let rules = darkmux_types::config_access::hooks_rules();
    let outbox_dir = darkmux_types::config_access::hooks_outbox_dir();
    build_hooks_check(enabled, provenance, &rules, &outbox_dir, &crate::resolved_config_path())
}

/// The pure rollup `check_hooks()` delegates to — split out so it's testable
/// against synthetic rules without the global config/env tier (#811 empties
/// `config()` in test builds, so there's no way to inject `hooks.rules`
/// through the real accessor path in a unit test).
fn build_hooks_check(
    enabled: bool,
    provenance: &str,
    rules: &[HookRule],
    outbox_dir: &Path,
    config_path: &Path,
) -> Vec<Check> {
    if !enabled {
        return vec![Check {
            name: "hooks".into(),
            status: Status::Pass,
            message: format!("disabled ({provenance})"),
            hint: None,
        }];
    }
    if rules.is_empty() {
        return vec![no_rules_check(provenance, outbox_dir, config_path)];
    }
    let summaries = darkmux_flow::hooks::summarize_configured_rules(rules, outbox_dir);
    let rule_checks: Vec<Check> = summaries
        .iter()
        .zip(rules)
        .map(|(s, rule)| rule_check(s, &rule.r#match.clone().unwrap_or_default(), config_path))
        .collect();
    let mut out = vec![overview_check(provenance, outbox_dir, &summaries, &rule_checks)];
    out.extend(rule_checks);
    let current_keys: HashSet<&str> = summaries.iter().map(|s| s.key.as_str()).collect();
    out.extend(stray_check(stray_outbox_files(&current_keys, outbox_dir)));
    out
}

fn no_rules_check(provenance: &str, outbox_dir: &Path, config_path: &Path) -> Check {
    Check {
        name: "hooks".into(),
        status: Status::Warn,
        message: format!("enabled ({provenance}) but no rules configured — outbox_dir={}", outbox_dir.display()),
        hint: Some(format!(
            "Add a rule to {}'s `hooks.rules`, e.g. `darkmux config set hooks.rules \
             '[{{\"match\":{{\"action\":\"dispatch.tool\",\"payload.tool_name\":\"create_finding\",\
             \"payload.ok\":true}},\"http\":\"http://127.0.0.1:8790/events\"}}]'`.",
            config_path.display()
        )),
    }
}

/// The `hooks` row: the worst rule status, and every rule's own message
/// listed under it.
fn overview_check(provenance: &str, outbox_dir: &Path, summaries: &[HookRuleSummary], rule_checks: &[Check]) -> Check {
    let worst = rule_checks.iter().map(|c| c.status).max().unwrap_or(Status::Pass);
    let lines: Vec<String> =
        summaries.iter().zip(rule_checks).map(|(s, c)| format!("  #{}: {}", s.index, c.message)).collect();
    Check {
        name: "hooks".into(),
        status: worst,
        message: format!(
            "enabled ({provenance}) — {} rule(s), outbox_dir={}\n{}",
            summaries.len(),
            outbox_dir.display(),
            lines.join("\n")
        ),
        hint: (worst != Status::Pass).then(|| "See the individual `hooks.rule.*` checks below for which rule(s).".into()),
    }
}

/// One problem on one rule: what the row says, how bad it is, and where
/// its cure lives.
struct RuleFlag {
    text: String,
    status: Status,
    cause: FlagCause,
}

/// Whether editing the rule's config is the cure, which decides the hint.
#[derive(Clone, Copy, PartialEq, Eq)]
enum FlagCause {
    /// The rule as written is the problem; the config remedy applies.
    Config,
    /// Delivery went wrong at run time (drops, a stall, quarantined lines,
    /// receiver rejections, give-ups); editing config will not clear it.
    Delivery,
}

impl RuleFlag {
    fn warn(text: impl Into<String>) -> Self {
        Self { text: text.into(), status: Status::Warn, cause: FlagCause::Config }
    }
    fn fail(text: impl Into<String>) -> Self {
        Self { text: text.into(), status: Status::Fail, cause: FlagCause::Config }
    }
    fn delivery(self) -> Self {
        Self { cause: FlagCause::Delivery, ..self }
    }
}

/// The hint line a delivery-side flag carries in place of the config remedy.
const DELIVERY_HINT: &str = "`darkmux flow status` shows this rule's delivery history.";

/// The `hooks.rule.<index>` row. Its status is the worst of its flags.
fn rule_check(s: &HookRuleSummary, rule_match: &HookMatch, config_path: &Path) -> Check {
    let flags = rule_flags(s, rule_match);
    let status = flags.iter().map(|f| f.status).max().unwrap_or(Status::Pass);
    let flag_str = if flags.is_empty() {
        String::new()
    } else {
        format!(" [{}]", flags.iter().map(|f| f.text.as_str()).collect::<Vec<_>>().join("; "))
    };
    let message = format!(
        "{} -> {} [{}, {}]{} (undelivered: {}){flag_str}",
        s.match_desc,
        darkmux_flow::hooks::display_url(&s.url),
        target_kind(s),
        signing(s),
        transform_suffix(s),
        s.undelivered
    );
    Check {
        name: format!("hooks.rule.{}", s.index),
        status,
        message,
        hint: rule_hint(&receiver_reason_lines(s), &last_error_lines(s), &flags, config_path),
    }
}

/// Every flag on one rule, in the order the row prints them.
fn rule_flags(s: &HookRuleSummary, rule_match: &HookMatch) -> Vec<RuleFlag> {
    [
        empty_match_flag(s),
        refusal_flag(s),
        tailnet_unsigned_flag(s),
        dropped_writes_flag(s),
        stalled_flag(s),
        giving_up_flag(s),
        quarantined_flag(s),
        receiver_rejected_flag(s),
        observer_flag(rule_match),
        cannot_match_flag(rule_match),
        old_spelling_flag(rule_match),
        transform_failed_flag(s),
    ]
    .into_iter()
    .flatten()
    .collect()
}

fn empty_match_flag(s: &HookRuleSummary) -> Option<RuleFlag> {
    s.is_empty_match.then(|| RuleFlag::warn("EMPTY MATCH — matches nothing"))
}

/// A refused rule is refused at load. A rule with both or neither of
/// `http`/`file` is refused for its destination FIELDS, not its URL, and
/// says so; otherwise the URL satisfied neither the loopback nor the
/// tailnet policy (#2135 option 2) — a valid tailnet rule is not this case.
fn refusal_flag(s: &HookRuleSummary) -> Option<RuleFlag> {
    if let Some(problem) = s.destination_problem {
        return Some(RuleFlag::fail(format!("DESTINATION REFUSED — {}; refused at load", problem.describe())));
    }
    s.is_refused.then(|| RuleFlag::fail("URL REFUSED — neither loopback nor a Tailscale address; refused at load"))
}

/// (#2135 option 2) An unsigned TAILNET target is fine inside the tailnet
/// (WireGuard already authenticates + encrypts the peer), but the receiver
/// has no way to attribute the record's sender beyond the body itself.
fn tailnet_unsigned_flag(s: &HookRuleSummary) -> Option<RuleFlag> {
    (s.is_tailnet && !s.signed).then(|| {
        RuleFlag::warn(
            "TAILNET TARGET, UNSIGNED — attribution is unsigned; fine inside the tailnet, required beyond it",
        )
    })
}

/// (#2093 merge-gate finding 9) Dropped writes (over the outbox cap, or an
/// append failure) are a Warn, not a Fail: delivery for every OTHER pending
/// line keeps working.
fn dropped_writes_flag(s: &HookRuleSummary) -> Option<RuleFlag> {
    (s.dropped_appends > 0).then(|| {
        RuleFlag::warn(format!(
            "{} write(s) dropped so far (over the outbox cap, or an append failure)",
            s.dropped_appends
        ))
        .delivery()
    })
}

/// (fix-round finding 1) A STALLED rule has stopped attempting deliveries.
fn stalled_flag(s: &HookRuleSummary) -> Option<RuleFlag> {
    s.stalled.then(|| {
        RuleFlag::warn(format!(
            "STALLED — {} consecutive cursor-write failure(s); the drainer has stopped attempting new \
             deliveries for this rule until its cursor file becomes writable again",
            s.cursor_write_failures
        ))
        .delivery()
    })
}

/// (fix-round finding 7) Quarantined (invalid-JSON) lines are never
/// redelivered.
fn quarantined_flag(s: &HookRuleSummary) -> Option<RuleFlag> {
    (s.quarantined_lines > 0)
        .then(|| RuleFlag::warn(format!("{} line(s) quarantined (invalid JSON — never redelivered)", s.quarantined_lines)).delivery())
}

/// (#2273) The receiver accepted a delivery's HTTP request (2xx) but its
/// response body reported it rejected some or all of the record(s) inside —
/// a third outcome, distinct from a transport failure and a clean accept.
/// darkmux never retries it (a receiver-side content rejection is usually
/// permanent), so the line is consumed; this row is where an operator who
/// missed the `hook.fired` Warn record still finds out.
///
/// Keyed on the CUMULATIVE `receiver_rejected_total`, never on
/// `last_receiver_rejected`: the latter lives on the `.last` sidecar, which
/// every terminal outcome replaces whole, so one clean delivery after 400
/// rejections would erase it. The last delivery's own count is named as
/// context when present. Describes the count and the actor; never calls
/// the receiver misconfigured.
fn receiver_rejected_flag(s: &HookRuleSummary) -> Option<RuleFlag> {
    if s.receiver_rejected_total == 0 {
        return None;
    }
    let last_clause = match s.last_receiver_rejected {
        Some(n) if !s.last_receiver_rejected_reasons.is_empty() => {
            format!("; {n} on the last delivery — the receiver's reason(s) below")
        }
        Some(n) => format!("; {n} on the last delivery"),
        None => String::new(),
    };
    Some(
        RuleFlag::warn(format!(
            "{} record(s) reported rejected by the receiver so far (request accepted, content \
             rejected — consumed, not retried){last_clause}",
            s.receiver_rejected_total
        ))
        .delivery(),
    )
}

/// (#2196) The receiver's own stated reason(s) for its last rejection, as
/// pre-bounded hint lines — empty when there is no rejection or no reason.
///
/// (#2196 fix-round 4) Receiver text NEVER joins a row's `message`, for two
/// structural reasons: `message` is word-wrapped to `output_width()`, which
/// reads `COLUMNS` — not exported to child processes by zsh or bash — so
/// doctor renders at its 100-column default, the terminal re-wraps, and a
/// continuation lands at column 0 where doctor's flush-left rows (header,
/// verdict banner, summary) live; and `verdict_banner_at` quotes the worst
/// check's whole `message` onto a flush-left line of its own. Every hint line
/// is printed behind an indent, and these lines are bounded so the finished
/// row stays under the narrowest supported terminal width.
fn receiver_reason_lines(s: &HookRuleSummary) -> Vec<String> {
    if s.receiver_rejected_total == 0 || s.last_receiver_rejected.is_none() {
        return Vec::new();
    }
    if s.last_receiver_rejected_reasons.is_empty() {
        return Vec::new();
    }
    darkmux_flow::hooks::rejection_reason_display_lines(
        &s.last_receiver_rejected_reasons,
        darkmux_flow::hooks::REJECTION_REASON_HINT_INDENT,
    )
}

/// The rule's last terminal outcome was a give-up (`last_error` is cleared by
/// the next successful delivery, so this is the CURRENT state). The error
/// itself rides the hint via [`last_error_lines`], never the message: it is
/// not all darkmux's own voice (a refused redirect embeds the receiver's
/// `Location`), and `message` reaches flush-left rows — see
/// [`receiver_reason_lines`].
fn giving_up_flag(s: &HookRuleSummary) -> Option<RuleFlag> {
    s.last_error.as_ref().map(|_| RuleFlag::warn("deliveries are giving up — the last error is below").delivery())
}

/// The last give-up's error as sanitized, width-bounded hint lines, under
/// a label — the same treatment `flow status` gives this field (#2694).
fn last_error_lines(s: &HookRuleSummary) -> Vec<String> {
    let Some(err) = &s.last_error else { return Vec::new() };
    let mut lines = vec!["the last delivery's error:".to_string()];
    lines.extend(darkmux_flow::hooks::untrusted_display_lines(err, darkmux_flow::hooks::REJECTION_REASON_HINT_INDENT));
    lines
}

fn observer_flag(rule_match: &HookMatch) -> Option<RuleFlag> {
    hooks_match_risks_observing_the_observer(rule_match)
        .then(|| RuleFlag::warn("matches telemetry / a bare `*` action — the observer must not join the observed"))
}

/// A rule whose `action` pattern matches no action darkmux writes can never
/// deliver, however quiet it looks. Decided from the vocabulary
/// ([`darkmux_flow::hooks::action_pattern_can_match`], the same test
/// `HookSink::new` warns with), never from what today's records happen to
/// carry. The hint names the dotted twin only when that twin can match.
fn cannot_match_flag(rule_match: &HookMatch) -> Option<RuleFlag> {
    let configured = rule_match.action.as_deref()?;
    if darkmux_flow::hooks::action_pattern_can_match(configured) {
        return None;
    }
    let hint = darkmux_flow::hooks::matching_dotted_twin(configured)
        .map(|t| format!("; `{t}` matches the dotted actions, and more than the old spelling did"))
        .unwrap_or_default();
    Some(RuleFlag::warn(format!(
        "CANNOT MATCH — action=\"{configured}\" matches no action darkmux writes \
         (actions are spelled `<scope>.<event>`){hint}"
    )))
}

/// A rule written against a pre-4.0 spelling still delivers: the sink reads
/// it as its current action ([`darkmux_flow::hooks::effective_action_pattern`]).
/// Named so the config can be updated to what the sink actually matches.
fn old_spelling_flag(rule_match: &HookMatch) -> Option<RuleFlag> {
    let configured = rule_match.action.as_deref()?;
    let effective = darkmux_flow::hooks::effective_action_pattern(configured);
    (effective != configured).then(|| {
        RuleFlag::warn(format!(
            "OLD SPELLING — action=\"{configured}\" is read as \"{effective}\"; write \"{effective}\" instead"
        ))
    })
}

/// (#2183) A `transform` that failed to load refuses THIS rule only
/// (`HookSink::new` disables it, the rest of the sink keeps running), so it
/// fails this row without the whole sink reading as broken.
fn transform_failed_flag(s: &HookRuleSummary) -> Option<RuleFlag> {
    match (&s.transform_name, &s.transform_status) {
        (Some(name), Some(Err(reason))) => Some(RuleFlag::fail(format!("TRANSFORM `{name}` FAILED TO LOAD — {reason}"))),
        _ => None,
    }
}

/// `, transform: <name> (blake3:<hash>)`, `[FAILED]` in place of the hash
/// when it did not load, or nothing when the rule has no transform.
fn transform_suffix(s: &HookRuleSummary) -> String {
    match (&s.transform_name, &s.transform_status) {
        (Some(name), Some(Ok(hash))) => format!(", transform: {name} (blake3:{hash})"),
        (Some(name), Some(Err(_))) => format!(", transform: {name} [FAILED]"),
        _ => String::new(),
    }
}

/// (#2135 option 2) What the destination is — the URL is the policy
/// decision, and this makes it legible. (#2183) `file` is the no-network
/// transport, with no URL policy or signature to report.
fn target_kind(s: &HookRuleSummary) -> &'static str {
    if s.destination_problem.is_some() {
        "refused"
    } else if s.is_file {
        "file"
    } else if s.is_loopback {
        "loopback"
    } else if s.is_tailnet {
        "tailnet"
    } else {
        "refused"
    }
}

/// `n/a` where no request is ever signed: a `file` rule, or a rule refused
/// for its destination fields.
fn signing(s: &HookRuleSummary) -> &'static str {
    if s.is_file || s.destination_problem.is_some() {
        "n/a"
    } else if s.signed {
        "signed"
    } else {
        "unsigned"
    }
}

/// Evidence first — the receiver's reason(s), then the last give-up's error:
/// an operator wants the other side's words before advice — then the cure for
/// each KIND of flag on the row: the config remedy only when a config-caused
/// flag is present, and the `flow status` pointer when a delivery-side one is.
fn rule_hint(reason_lines: &[String], error_lines: &[String], flags: &[RuleFlag], config_path: &Path) -> Option<String> {
    let mut lines: Vec<String> = Vec::new();
    if !reason_lines.is_empty() {
        lines.push("the receiver's stated reason(s) for the last rejection:".into());
        lines.extend(reason_lines.iter().cloned());
    }
    lines.extend(error_lines.iter().cloned());
    if flags.iter().any(|f| f.cause == FlagCause::Config) {
        lines.push(format!("Fix this rule in {} (or `darkmux config set hooks.rules ...`).", config_path.display()));
    }
    if flags.iter().any(|f| f.cause == FlagCause::Delivery) {
        lines.push(DELIVERY_HINT.into());
    }
    (!lines.is_empty()).then(|| lines.join("\n"))
}

/// The telemetry samples darkmux writes (`Category::Telemetry`): every
/// action in the `telemetry` scope (the dispatch instruments) and the host
/// sampler's `machine.telemetry`. Read off [`FlowAction`], so an instrument
/// added to the vocabulary is covered without touching this check.
fn is_telemetry_sample(action: &FlowAction) -> bool {
    action.scope() == Some(FlowScope::Telemetry) || *action == FlowAction::MachineTelemetry
}

/// Every telemetry sample's wire spelling (see [`is_telemetry_sample`]).
fn telemetry_actions() -> impl Iterator<Item = &'static str> {
    FlowAction::KNOWN_WIRE
        .iter()
        .copied()
        .filter(|wire| FlowAction::parse_known(wire).is_ok_and(|a| is_telemetry_sample(&a)))
}

/// (#2093 merge-gate finding 17) True when a rule would deliver a telemetry
/// record — the observer joining the observed (this project's own doctrine,
/// CLAUDE.md's "The observer must not join the observed"). Decided by the
/// sink's own [`darkmux_flow::hooks::hook_match`] against synthetic
/// telemetry records, so the check and the matcher are one rule: however a
/// rule spells its category or glob, it is flagged exactly when it would
/// match. A rule narrowed by a payload predicate or an id never matches a
/// synthetic record, so it is not flagged.
fn hooks_match_risks_observing_the_observer(m: &HookMatch) -> bool {
    // Each sample is tried at `info` and at `warn`, the level detector
    // firings are written at.
    telemetry_actions().flat_map(|action| ["info", "warn"].map(|level| (action, level))).any(|(action, level)| {
        let record: darkmux_flow::FlowRecord = serde_json::from_value(serde_json::json!({
            "ts": "", "level": level, "category": "telemetry", "tier": "local",
            "stage": "dispatch", "action": action, "handle": ""
        }))
        .expect("a synthetic telemetry record is a valid FlowRecord");
        darkmux_flow::hooks::hook_match(m, &record)
    })
}

/// (#2093 merge-gate finding 15) The `hooks.stray` row, when any outbox file
/// belongs to no current rule — named, rather than silently taking up disk
/// forever. (fix-round finding 6) Each file carries its undelivered line
/// count and its sibling sidecars: an operator deciding "safe to delete?"
/// needs both.
fn stray_check(mut stray: Vec<StrayOutbox>) -> Option<Check> {
    if stray.is_empty() {
        return None;
    }
    // Name order, not `read_dir` order, so the row is stable run to run.
    stray.sort_by(|a, b| a.path.cmp(&b.path));
    let details: Vec<String> = stray
        .iter()
        .map(|s| {
            let name = s.path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
            let siblings =
                if s.siblings.is_empty() { String::new() } else { format!("; siblings: {}", s.siblings.join(", ")) };
            format!("{name} ({} undelivered line(s){siblings})", s.undelivered)
        })
        .collect();
    Some(Check {
        name: "hooks.stray".into(),
        status: Status::Warn,
        message: format!("{} outbox file(s) belong to no currently-configured rule: {}", stray.len(), details.join(", ")),
        hint: Some(
            "A rule was removed or edited since these were written. `darkmux flow drain --file <path> \
             --to <loopback url>` delivers a stray file's undelivered lines before you delete it; once \
             undelivered is 0, it (and its sibling sidecars) are safe to remove."
                .into(),
        ),
    })
}

/// (fix-round finding 6) One stray `*.outbox.jsonl` file — one whose
/// owning rule no longer exists in current config — plus the detail an
/// operator deciding "safe to delete?" actually needs: how many lines
/// were never delivered, and which sibling sidecar files (all sharing
/// the same content-hash key) go with it.
struct StrayOutbox {
    path: std::path::PathBuf,
    undelivered: usize,
    siblings: Vec<String>,
}

/// Sibling sidecar suffixes a stray outbox's key can carry — see
/// `darkmux_flow::hooks`'s per-rule file layout (`outbox_paths`,
/// `last_status_path`, `dropped_appends_path`, `receiver_rejected_path`,
/// `drain_lock_path`, `quarantine_path`). This list is what an operator
/// deciding "safe to delete?" reads, so a NEW per-rule sidecar belongs
/// here in the same change that introduces it.
const HOOK_SIDECAR_SUFFIXES: &[&str] =
    &[".cursor", ".last", ".dropped", ".rejected", ".drain.lock", ".outbox.jsonl.quarantine"];

/// (#2093 merge-gate finding 15) `*.outbox.jsonl` files in `outbox_dir`
/// whose key (the content-hash `rule_key`) matches no currently configured
/// rule: a rule since removed, or edited enough to change its key. Nothing
/// drains such a file again unless the rule comes back verbatim.
///
/// `current_keys` are the configured rules' `HookRuleSummary::key`s — the
/// summary's own key derivation, never one recomputed here: a `file` rule
/// has no `http`, so keying on `http` alone would misreport its real outbox
/// as stray (#2183).
fn stray_outbox_files(
    current_keys: &HashSet<&str>,
    outbox_dir: &Path,
) -> Vec<StrayOutbox> {
    let Ok(entries) = std::fs::read_dir(outbox_dir) else {
        return Vec::new();
    };
    entries
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter_map(|path| {
            let name = path.file_name().and_then(|n| n.to_str())?;
            let key = name.strip_suffix(".outbox.jsonl")?;
            if current_keys.contains(key) {
                return None;
            }
            // The stray file's own `.cursor` sidecar (if it survived
            // alongside it) still names the true last-delivered offset;
            // falling back to 0 (nothing ever delivered) only overcounts
            // when that sidecar is itself missing.
            let cursor = std::fs::read_to_string(outbox_dir.join(format!("{key}.cursor")))
                .ok()
                .and_then(|s| s.trim().parse::<u64>().ok())
                .unwrap_or(0);
            let undelivered = darkmux_flow::hooks::undelivered_line_count(&path, cursor);
            let siblings: Vec<String> = HOOK_SIDECAR_SUFFIXES
                .iter()
                .map(|suffix| format!("{key}{suffix}"))
                .filter(|sibling_name| outbox_dir.join(sibling_name).exists())
                .collect();
            Some(StrayOutbox { path, undelivered, siblings })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::render_check_block;
    use crate::tests::strip_ansi;

    // ─── (#2093) check_hooks — flow-record hooks ───────────────────────────

    #[serial_test::serial]
    #[test]
    fn check_hooks_disabled_by_default_is_pass() {
        let prev = std::env::var("DARKMUX_HOOKS_ENABLED").ok();
        unsafe { std::env::remove_var("DARKMUX_HOOKS_ENABLED"); }
        let checks = check_hooks();
        assert_eq!(checks.len(), 1, "disabled → the one overview check, no per-rule checks");
        let check = &checks[0];
        assert_eq!(check.status, Status::Pass, "{}", check.message);
        assert!(check.message.contains("disabled"), "{}", check.message);
        // (#2093 merge-gate finding 14) No env, no config tier in test
        // builds (#811) → provenance is `default`, not silently `config.json`.
        assert!(check.message.contains("default"), "{}", check.message);
        unsafe {
            match prev {
                Some(v) => std::env::set_var("DARKMUX_HOOKS_ENABLED", v),
                None => std::env::remove_var("DARKMUX_HOOKS_ENABLED"),
            }
        }
    }

    #[serial_test::serial]
    #[test]
    fn check_hooks_enabled_with_no_rules_warns() {
        let prev = std::env::var("DARKMUX_HOOKS_ENABLED").ok();
        unsafe { std::env::set_var("DARKMUX_HOOKS_ENABLED", "true"); }
        let checks = check_hooks();
        assert_eq!(checks.len(), 1);
        let check = &checks[0];
        assert_eq!(check.status, Status::Warn, "{}", check.message);
        assert!(check.message.contains("no rules"), "{}", check.message);
        assert!(check.hint.is_some());
        // env DID set it here, so provenance must say `env`, not `default`.
        assert!(check.message.contains("env"), "{}", check.message);
        unsafe {
            match prev {
                Some(v) => std::env::set_var("DARKMUX_HOOKS_ENABLED", v),
                None => std::env::remove_var("DARKMUX_HOOKS_ENABLED"),
            }
        }
    }

    /// `check_hooks` names the config file doctor actually reads — under
    /// `DARKMUX_HOME`, not `~/.darkmux` — in the remedy it prints.
    #[serial_test::serial]
    #[test]
    fn check_hooks_names_the_config_file_under_darkmux_home() {
        let state = darkmux_types::test_isolation::IsolatedState::new();
        let prev = std::env::var("DARKMUX_HOOKS_ENABLED").ok();
        unsafe { std::env::set_var("DARKMUX_HOOKS_ENABLED", "true") };
        let hint = check_hooks().remove(0).hint;
        unsafe {
            match prev {
                Some(v) => std::env::set_var("DARKMUX_HOOKS_ENABLED", v),
                None => std::env::remove_var("DARKMUX_HOOKS_ENABLED"),
            }
        }
        let want = state.path().join("config.json");
        let hint = hint.unwrap();
        assert!(hint.starts_with(&format!("Add a rule to {}'s `hooks.rules`", want.display())), "{hint}");
    }

    /// (#2093 merge-gate finding 14) `build_hooks_check` now returns ONE
    /// `Check` per flagged rule (`hooks.rule.<index>`) plus one overview
    /// (`hooks`) — so a flag attaches to the RULE it names, not to an
    /// aggregate message an operator has to cross-reference by hand.
    /// Exercised against `build_hooks_check` directly with synthetic
    /// rules, since the global `config()` tier is empty by construction
    /// in test builds (#811) — there is no way to inject a populated
    /// `hooks.rules` through `check_hooks()`'s normal env/config path.
    #[test]
    fn hooks_check_rollup_flags_attach_to_the_right_rule() {
        use darkmux_types::config::{HookMatch, HookRule};
        let tmp = tempfile::TempDir::new().unwrap();
        let rules = vec![
            HookRule {
                r#match: Some(HookMatch { action: Some(live_action()), ..Default::default() }),
                http: Some("http://127.0.0.1:8790/events".to_string()),
                signing_secret_keychain_item: None,
                file: None,
                transform: None,
                headers: None,
                attribution_headers: None,
                extras: Default::default(),
            },
            HookRule {
                r#match: None,
                http: Some("http://127.0.0.1:9000/x".to_string()),
                signing_secret_keychain_item: None,
                file: None,
                transform: None,
                headers: None,
                attribution_headers: None,
                extras: Default::default(),
            },
            HookRule {
                r#match: Some(HookMatch { action: Some("*".to_string()), ..Default::default() }),
                http: Some("http://10.0.0.5:8790/x".to_string()),
                signing_secret_keychain_item: None,
                file: None,
                transform: None,
                headers: None,
                attribution_headers: None,
                extras: Default::default(),
            },
        ];
        let checks = build_hooks_check(true, "config.json", &rules, tmp.path(), std::path::Path::new(TEST_CONFIG_PATH));
        assert_eq!(checks.len(), 4, "1 overview + 3 per-rule checks");

        let overview = checks.iter().find(|c| c.name == "hooks").unwrap();
        assert_eq!(overview.status, Status::Fail, "worst of the three rules — a non-loopback rule is a hard block");

        let healthy = checks.iter().find(|c| c.name == "hooks.rule.0").unwrap();
        assert_eq!(healthy.status, Status::Pass, "{}", healthy.message);
        assert!(healthy.message.contains(&live_action()), "{}", healthy.message);
        assert!(healthy.message.contains("undelivered"), "{}", healthy.message);

        let empty_match = checks.iter().find(|c| c.name == "hooks.rule.1").unwrap();
        assert_eq!(empty_match.status, Status::Warn, "{}", empty_match.message);
        assert!(empty_match.message.contains("EMPTY MATCH"), "{}", empty_match.message);
        assert!(!empty_match.message.contains("REFUSED"), "rule 1's own flags only: {}", empty_match.message);

        // 10.0.0.5 is neither loopback nor a Tailscale address (not in
        // 100.64.0.0/10, no `.ts.net` suffix) — refused (#2135 option 2).
        let refused = checks.iter().find(|c| c.name == "hooks.rule.2").unwrap();
        assert_eq!(refused.status, Status::Fail, "{}", refused.message);
        assert!(refused.message.contains("URL REFUSED"), "{}", refused.message);
        assert!(!refused.message.contains("EMPTY MATCH"), "rule 2's own flags only: {}", refused.message);
    }

    /// (#2135 option 2) A tailnet target (`100.64.0.0/10`) is accepted by
    /// URL policy alone — no config gate — and is NOT the `is_refused`
    /// case a plain non-tailnet non-loopback host is. Unsigned (no
    /// `signing_secret_keychain_item`) still Warns.
    #[test]
    fn hooks_check_accepts_tailnet_target_and_warns_when_unsigned() {
        use darkmux_types::config::{HookMatch, HookRule};
        let tmp = tempfile::TempDir::new().unwrap();
        let rules = vec![HookRule {
            r#match: Some(HookMatch { action: Some(live_action()), ..Default::default() }),
            http: Some("http://100.64.1.2:8790/events".to_string()),
            signing_secret_keychain_item: None,
            file: None,
            transform: None,
            headers: None,
            attribution_headers: None,
            extras: Default::default(),
        }];
        let checks = build_hooks_check(true, "config.json", &rules, tmp.path(), std::path::Path::new(TEST_CONFIG_PATH));
        let rule = checks.iter().find(|c| c.name == "hooks.rule.0").unwrap();
        assert_eq!(rule.status, Status::Warn, "{}", rule.message);
        assert!(!rule.message.contains("URL REFUSED"), "a valid tailnet target is not refused: {}", rule.message);
        assert!(rule.message.contains("[tailnet, unsigned]"), "{}", rule.message);
        assert!(rule.message.contains("TAILNET TARGET, UNSIGNED"), "{}", rule.message);
    }

    /// (#2135 option 2) The same tailnet target, but signed — no Warn.
    #[test]
    fn hooks_check_tailnet_target_signed_is_pass() {
        use darkmux_types::config::{HookMatch, HookRule};
        let tmp = tempfile::TempDir::new().unwrap();
        let rules = vec![HookRule {
            r#match: Some(HookMatch { action: Some(live_action()), ..Default::default() }),
            http: Some("http://100.64.1.2:8790/events".to_string()),
            signing_secret_keychain_item: Some("darkmux-hook-0".to_string()),
            file: None,
            transform: None,
            headers: None,
            attribution_headers: None,
            extras: Default::default(),
        }];
        let checks = build_hooks_check(true, "config.json", &rules, tmp.path(), std::path::Path::new(TEST_CONFIG_PATH));
        let rule = checks.iter().find(|c| c.name == "hooks.rule.0").unwrap();
        assert_eq!(rule.status, Status::Pass, "{}", rule.message);
        assert!(rule.message.contains("[tailnet, signed]"), "{}", rule.message);
    }

    /// (#2093 merge-gate finding 17) A rule matching `telemetry.*` (or the
    /// `telemetry` category) or a bare `*` action risks the observer
    /// joining the observed — this project's own doctrine (CLAUDE.md
    /// "The observer must not join the observed"). Doctor names it.
    #[test]
    fn hooks_check_warns_on_telemetry_or_bare_star_match() {
        use darkmux_types::config::{HookMatch, HookRule};
        let tmp = tempfile::TempDir::new().unwrap();
        let rules = vec![
            HookRule {
                r#match: Some(HookMatch { action: Some("telemetry.tokens".to_string()), ..Default::default() }),
                http: Some("http://127.0.0.1:8790/a".to_string()),
                signing_secret_keychain_item: None,
                file: None,
                transform: None,
                headers: None,
                attribution_headers: None,
                extras: Default::default(),
            },
            HookRule {
                r#match: Some(HookMatch { action: Some("*".to_string()), ..Default::default() }),
                http: Some("http://127.0.0.1:8790/b".to_string()),
                signing_secret_keychain_item: None,
                file: None,
                transform: None,
                headers: None,
                attribution_headers: None,
                extras: Default::default(),
            },
        ];
        let checks = build_hooks_check(true, "config.json", &rules, tmp.path(), std::path::Path::new(TEST_CONFIG_PATH));
        let telemetry = checks.iter().find(|c| c.name == "hooks.rule.0").unwrap();
        assert_eq!(telemetry.status, Status::Warn, "{}", telemetry.message);
        assert!(telemetry.message.contains("observer must not join the observed"), "{}", telemetry.message);

        let bare_star = checks.iter().find(|c| c.name == "hooks.rule.1").unwrap();
        assert_eq!(bare_star.status, Status::Warn, "{}", bare_star.message);
        assert!(bare_star.message.contains("observer must not join the observed"), "{}", bare_star.message);
    }

    fn one_rule_matching(action: &str) -> Vec<darkmux_types::config::HookRule> {
        vec![hook_rule(Some(action), Some("http://127.0.0.1:8790/events"))]
    }

    fn rule_row(action: &str) -> Check {
        let tmp = tempfile::TempDir::new().unwrap();
        let checks = build_hooks_check(true, "config.json", &one_rule_matching(action), tmp.path(), std::path::Path::new(TEST_CONFIG_PATH));
        named(&checks, "hooks.rule.0").clone()
    }

    /// The CANNOT MATCH hint names a dotted twin only when the twin can
    /// match: `dispatchh *` and `sprint *` get none.
    #[test]
    fn cannot_match_names_only_a_twin_that_can_match() {
        let flag = cannot_match_flag(&one_rule_matching("dispatch *")[0].r#match.clone().unwrap()).unwrap();
        assert!(flag.text.contains("`dispatch.*` matches"), "{}", flag.text);
        for pattern in ["dispatchh *", "sprint *"] {
            let flag = cannot_match_flag(&one_rule_matching(pattern)[0].r#match.clone().unwrap()).unwrap();
            assert!(flag.text.contains("CANNOT MATCH"), "{}", flag.text);
            assert!(!flag.text.contains(".*`"), "no twin hint for {pattern}: {}", flag.text);
        }
    }

    /// (4.0) A rule written against an old spelling still delivers (the
    /// hook layer reads it as its current action) and warns with what to
    /// write instead; a spaced glob whose dotted twin would widen it cannot
    /// match, and says so.
    #[test]
    fn hooks_check_warns_on_a_rule_written_against_a_retired_spelling() {
        // flow-action-guard:allow — an old spelling is this test's input
        let rule = rule_row("dispatch complete");
        assert_eq!(rule.status, Status::Warn, "{}", rule.message);
        assert!(rule.message.contains("OLD SPELLING"), "{}", rule.message);
        assert!(!rule.message.contains("CANNOT MATCH"), "an upgraded spelling delivers: {}", rule.message);
        let current = FlowAction::DispatchComplete.as_str();
        assert!(rule.message.contains(&format!("write \"{current}\" instead")), "names the current one: {}", rule.message);
        let rule = rule_row("dispatch *");
        assert!(rule.message.contains("CANNOT MATCH"), "{}", rule.message);
        assert!(rule.message.contains("`dispatch.*` matches"), "{}", rule.message);
        assert!(!rule.message.contains("OLD SPELLING"), "a pattern that cannot match is not an old spelling: {}", rule.message);
    }

    /// A glob that matches no action at all (a typo, an invented scope, a
    /// retired action) warns too, without a suggestion it does not have.
    /// The retired case is deliberate: `crawl.*` names only
    /// [`darkmux_flow::legacy::RetiredAction`]s, which the reader knows and
    /// no writer emits, so a rule for them can never deliver.
    #[test]
    fn hooks_check_warns_on_a_glob_that_matches_no_action() {
        let retired = darkmux_flow::legacy::RetiredAction::CrawlFinding.as_str();
        let retired_glob = format!("{}.*", retired.split('.').next().unwrap());
        for pattern in ["dispatchh.*", retired, retired_glob.as_str()] {
            let rule = rule_row(pattern);
            assert_eq!(rule.status, Status::Warn, "{pattern}: {}", rule.message);
            assert!(rule.message.contains("CANNOT MATCH"), "{pattern}: {}", rule.message);
            assert!(!rule.message.contains("instead"), "{pattern}: {}", rule.message);
        }
    }

    /// The inverse: a current action, and a glob that reaches some, stay
    /// Pass, whether or not the rule has ever delivered. (A bare `*` warns
    /// for another reason, the observer check, never for CANNOT MATCH.)
    #[test]
    fn hooks_check_passes_a_rule_that_can_match() {
        let action = live_action();
        let glob = format!("{}.*", action.split('.').next().unwrap());
        for pattern in [action.as_str(), glob.as_str()] {
            let rule = rule_row(pattern);
            assert_eq!(rule.status, Status::Pass, "{pattern}: {}", rule.message);
        }
        let rule = rule_row("*");
        assert!(!rule.message.contains("CANNOT MATCH"), "{}", rule.message);
    }

    /// (#2196 fix-round 4) Every line `print_report` emits FLUSH LEFT
    /// (column 0). Derived by enumerating every `println!` in that
    /// function plus the two it delegates to (`render_check_block`,
    /// `verdict_banner_at`); there are exactly THREE shapes, against
    /// four indented ones.
    ///
    /// | Row | Column |
    /// |---|---|
    /// | `darkmux doctor — {n} checks` (header) | 0 |
    /// | `● ok …` / `● needs attention — …` / `● broken — …` (verdict banner) | 0 |
    /// | `all {n} checks passed…` / `{n} pass, {m} warn — workable but worth a look` / `{n} pass, {m} warn, {k} fail — fix failures before running darkmux end-to-end` (summary) | 0 |
    /// | `  {marker} {name:<22} {message}` (check first line) | 2 |
    /// | message continuation | `head` (>= 27) |
    /// | `        → {hint}` (hint first line) | 8 |
    /// | hint continuation | 10 |
    ///
    /// The verdict banner and the summary are the two that matter: the
    /// banner is the FIRST line an operator reads and the summary is the
    /// LAST, and between them they are the whole verdict. A forged
    /// `"33 pass, 2 warn — workable but worth a look"` under a genuine
    /// `broken` banner is a receiver telling the operator the machine is
    /// fine.
    const DOCTOR_FLUSH_LEFT_ROWS: &[&str] =
        &["darkmux doctor —", "● ok", "● needs attention —", "● broken —", "all ", " pass, "];

    /// The forgery payloads: the verbatim vocabulary of doctor's three
    /// flush-left row shapes, each sized to a plausible whole row.
    const DOCTOR_FORGERY_PAYLOADS: &[&str] = &[
        "33 pass, 2 warn — workable but worth a look",
        "● needs attention — everything looks fine here",
        "● ok — every check passed",
        "darkmux doctor — 35 checks",
        "all 35 checks passed",
    ];

    /// Simulate a terminal `width` columns wide wrapping `lines`, and
    /// return every VISUAL line that is a CONTINUATION — the only lines
    /// receiver text can reach column 0 through.
    fn doctor_wrapped_continuations(lines: &[String], width: usize) -> Vec<String> {
        let mut out = Vec::new();
        for line in lines {
            let visible = strip_ansi(line);
            let chars: Vec<char> = visible.chars().collect();
            let mut start = width;
            while start < chars.len() {
                out.push(chars[start..].iter().take(width).collect::<String>());
                start += width;
            }
        }
        out
    }

    /// Render `hooks.rule.0`'s check block for a receiver rejection whose
    /// reason is `reason`, at doctor's REAL default render width.
    ///
    /// 100 is not an arbitrary fixture choice: `output_width()` reads
    /// `COLUMNS`, and **`COLUMNS` is not exported to child processes** by
    /// either zsh or bash on this machine (measured —
    /// `zsh -i -c 'printenv COLUMNS'` exits 1, as do the bash form and
    /// this process's own environment). So `darkmux doctor` run from an
    /// ordinary shell falls through to the 100-column default however
    /// wide the operator's terminal actually is, which is exactly the
    /// mismatch the forgery needs: on an 80-column terminal doctor emits
    /// lines up to 100 columns and the terminal wraps them.
    fn doctor_rejection_check(reason: &str) -> Check {
        let tmp = tempfile::TempDir::new().unwrap();
        let (rule_cfg, key) = rejection_fixture_rule();
        let rules = vec![rule_cfg];
        std::fs::write(
            tmp.path().join(format!("{key}.last")),
            serde_json::json!({
                "ts": "2026-01-01T00:00:00Z",
                "ok": true,
                "last_receiver_rejected": 1,
                "last_receiver_rejected_reasons": [reason],
            })
            .to_string(),
        )
        .unwrap();
        std::fs::write(tmp.path().join(format!("{key}.rejected")), "1").unwrap();

        let checks = build_hooks_check(true, "config.json", &rules, tmp.path(), std::path::Path::new(TEST_CONFIG_PATH));
        checks.into_iter().find(|c| c.name == "hooks.rule.0").unwrap()
    }

    fn doctor_rejection_block(reason: &str) -> Vec<String> {
        render_check_block(&doctor_rejection_check(reason), 100)
    }

    /// The pre-fix INLINE rendering, reconstructed against the REAL rule
    /// message rather than a synthetic stand-in.
    ///
    /// Reconstructed rather than hard-coded on purpose: a hand-copied
    /// replica of the old message would drift the moment the surrounding
    /// text changed, and the filler search would then be tuning against a
    /// line the renderer never produced — the precondition would still
    /// "pass" while proving nothing about the real surface. Taking the
    /// live check and putting the quoted reason back into its `message`
    /// reproduces the pre-fix geometry and stays correct as the message
    /// around it evolves.
    fn doctor_inline_block(reason: &str) -> Vec<String> {
        let live = doctor_rejection_check(reason);
        let quoted =
            darkmux_flow::hooks::format_rejection_reasons_for_display(std::slice::from_ref(&reason.to_string()));
        render_check_block(
            &Check { message: format!("{} ({quoted})", live.message), ..live },
            100,
        )
    }

    /// (#2196 fix-round 4) `doctor` is the surface an operator reads to
    /// decide whether the system is HEALTHY, which makes a forged row
    /// here worth more to an attacker than any row in `flow status`.
    ///
    /// SELF-PROVING, the same shape as
    /// `flow_status_reason_cannot_forge_a_flush_left_row`: it first
    /// SEARCHES for a (payload, width, filler) combination that makes the
    /// forgery genuinely land at column 0 in the INLINE form, asserts at
    /// least one exists — then asserts the SHIPPED renderer produces no
    /// such continuation for any of them.
    ///
    /// The search is what makes the precondition honest. Doctor's layout
    /// is `"  {marker} {name:<22} {message}"` word-wrapped at 100, so the
    /// column a payload lands on is not something a fixture can assume;
    /// it has to be found. A fixture that guessed wrong would pass while
    /// proving nothing — the exact failure this PR already made once in
    /// `status.rs`.
    ///
    /// Not every pair is forgeable, and the reason is structural: doctor
    /// caps its OWN lines at `output_width()`, so the continuation window
    /// a terminal `w` columns wide exposes is only `output_width() - w`
    /// columns. At the measured default of 100 that is 40 columns on a
    /// 60-column terminal and 20 on an 80-column one — too narrow for the
    /// 43-column summary row, wide enough for a verdict banner. The
    /// search records which pairs are real rather than assuming a grid.
    ///
    /// Red-proves by name: put the quoted reason back into the rule
    /// check's `message` (the `last_clause` that carried
    /// `format_rejection_reasons_for_display` before this fix) and the
    /// post-fix assertion fails on every pair the precondition found.
    #[test]
    fn doctor_reason_cannot_forge_a_flush_left_row() {
        let widths = [60usize, 72, 80, 100, 120];
        let mut proven: Vec<(String, usize, usize)> = Vec::new();

        for payload in DOCTOR_FORGERY_PAYLOADS {
            for width in widths {
                // The sanitizer bounds a reason to 118 columns, so a
                // filler past that destroys the payload rather than
                // placing it — search only the range that can actually
                // carry a whole payload.
                let max_filler =
                    darkmux_flow::hooks::MAX_REJECTION_REASON_DISPLAY_WIDTH.saturating_sub(payload.chars().count() + 3);
                for filler in 0..=max_filler {
                    let reason = format!("{} {payload}", "z".repeat(filler));
                    if doctor_wrapped_continuations(&doctor_inline_block(&reason), width)
                        .iter()
                        .any(|c| c.starts_with(payload))
                    {
                        proven.push(((*payload).to_string(), width, filler));
                        break;
                    }
                }
            }
        }

        assert!(
            !proven.is_empty(),
            "the precondition found no forgeable (payload, width, filler) at all — this test would prove nothing"
        );
        println!("doctor inline forgeries proven (payload, terminal width, filler): {proven:#?}");

        // Every combination the precondition PROVED must now be closed by
        // the shipped renderer — and not merely for its own payload: no
        // continuation may begin with ANY of doctor's flush-left rows.
        for (payload, _width, filler) in &proven {
            let reason = format!("{} {payload}", "z".repeat(*filler));
            let block = doctor_rejection_block(&reason);
            for w in widths {
                for continuation in doctor_wrapped_continuations(&block, w) {
                    for row in DOCTOR_FLUSH_LEFT_ROWS {
                        assert!(
                            !continuation.starts_with(row),
                            "width {w}: a wrapped continuation forges doctor's flush-left row {row:?} \
                             (payload {payload:?}, filler {filler}): {continuation:?}"
                        );
                    }
                }
            }
        }
    }

    /// (#2196 fix-round 4, the mechanism asserted independently of any
    /// forged vocabulary) Every line doctor emits that carries
    /// receiver-controlled text must be BOTH indented and strictly
    /// narrower than the narrowest supported terminal width — so a row a
    /// future revision adds is closed by construction rather than by
    /// matching a string.
    ///
    /// Red-proves by name: put the reason back in `message` and the
    /// indent assertion fails (doctor's first check line starts at column
    /// 2 with the marker, not at the reason's indent); widen
    /// `REJECTION_REASON_HINT_CONTENT_BUDGET` past 49 and the
    /// strict-inequality assertion fails at 60 columns.
    #[test]
    fn every_doctor_reason_line_is_indented_and_narrower_than_the_supported_width() {
        // Three shapes at once: an unbroken run with no wrap opportunity,
        // wide (2-column) characters, and a realistic multi-word reason.
        let reasons =
            ["q".repeat(400), "漢".repeat(200), "payload field \"file\" must be a non-empty string".to_string()];
        let min_width = darkmux_flow::hooks::MIN_SUPPORTED_TERMINAL_WIDTH;

        for reason in reasons {
            let block = doctor_rejection_block(&reason);
            let mut reason_lines = 0usize;
            for line in &block {
                let visible = strip_ansi(line);
                // The reason lines are exactly the ones carrying a quote —
                // `format_rejection_reasons_for_display` always quotes, and
                // no other row in this block emits one.
                if !visible.contains('"') {
                    continue;
                }
                reason_lines += 1;
                assert!(
                    visible.starts_with("        "),
                    "doctor reason line is not indented: {visible:?}"
                );
                let w = darkmux_flow::hooks::display_columns(&visible);
                assert!(w < min_width, "doctor reason line is {w} columns, must stay under {min_width}: {visible:?}");
            }
            assert!(reason_lines > 0, "the fixture must actually produce reason lines: {block:?}");
        }
    }

    /// One `HookRule` plus its `rule_key`, for the receiver-rejection
    /// fixtures below — all three stage sidecar files by hand under a
    /// tempdir standing in for the outbox dir.
    fn rejection_fixture_rule() -> (darkmux_types::config::HookRule, String) {
        use darkmux_types::config::{HookMatch, HookRule};
        let m = HookMatch { action: Some(live_action()), ..Default::default() };
        let url = "http://127.0.0.1:8790/events".to_string();
        let key = darkmux_flow::hooks::rule_key(&m, &url);
        (
            HookRule {
                r#match: Some(m),
                http: Some(url),
                signing_secret_keychain_item: None,
                file: None,
                transform: None,
                headers: None,
                attribution_headers: None,
                extras: Default::default(),
            },
            key,
        )
    }

    /// (#2273) A receiver that answered 2xx but reported it rejected
    /// content must surface as a Warn on the rule's own check row — this
    /// is the doctor-side half of the fix, since the `hook.fired` flow
    /// record that first reported it is a point-in-time event on the
    /// stream, not something a separate `darkmux doctor` invocation can
    /// see after the fact.
    #[test]
    fn hooks_check_warns_on_receiver_rejected_last_delivery() {
        let tmp = tempfile::TempDir::new().unwrap();
        let (rule_cfg, key) = rejection_fixture_rule();
        let rules = vec![rule_cfg];
        std::fs::write(
            tmp.path().join(format!("{key}.last")),
            r#"{"ts":"2026-01-01T00:00:00Z","ok":true,"last_receiver_rejected":3}"#,
        )
        .unwrap();
        std::fs::write(tmp.path().join(format!("{key}.rejected")), "3").unwrap();

        let checks = build_hooks_check(true, "config.json", &rules, tmp.path(), std::path::Path::new(TEST_CONFIG_PATH));
        let rule = checks.iter().find(|c| c.name == "hooks.rule.0").unwrap();
        assert_eq!(rule.status, Status::Warn, "{}", rule.message);
        assert!(rule.message.contains("3 record(s) reported rejected by the receiver"), "{}", rule.message);
        assert!(rule.message.contains("3 on the last delivery"), "{}", rule.message);
    }

    /// (#2196) When the `.last` sidecar also carries the receiver's own
    /// stated reason(s) for the rejection, `doctor` must name them next
    /// to the count — the count alone tells an operator SOMETHING was
    /// thrown away, never WHY.
    #[test]
    fn hooks_check_names_the_receivers_last_rejection_reason_when_present() {
        let tmp = tempfile::TempDir::new().unwrap();
        let (rule_cfg, key) = rejection_fixture_rule();
        let rules = vec![rule_cfg];
        std::fs::write(
            tmp.path().join(format!("{key}.last")),
            r#"{"ts":"2026-01-01T00:00:00Z","ok":true,"last_receiver_rejected":1,"last_receiver_rejected_reasons":["payload field \"file\" must be a non-empty string"]}"#,
        )
        .unwrap();
        std::fs::write(tmp.path().join(format!("{key}.rejected")), "1").unwrap();

        let checks = build_hooks_check(true, "config.json", &rules, tmp.path(), std::path::Path::new(TEST_CONFIG_PATH));
        let rule = checks.iter().find(|c| c.name == "hooks.rule.0").unwrap();
        assert_eq!(rule.status, Status::Warn, "{}", rule.message);
        // (#2196 fix-round 2, MUST FIX C) The fixture's 47-column raw
        // text is written straight into the `.last` sidecar, bypassing
        // the producer's own `truncate_reason` call — this proves the
        // render-time re-sanitization/re-bounding
        // (`darkmux_flow::hooks::format_rejection_reasons_for_display`)
        // ALSO applies at read time: quoted, with the reason's OWN
        // internal `"` backslash-escaped, and — since 47 columns is well
        // under the fix-round-2 budget of 118 — surviving WHOLE rather
        // than losing the word "string" to the old 40-column cap.
        //
        // (#2196 fix-round 4, MUST FIX G at the doctor surface) The
        // reason moved OUT of `message` and into the HINT. `message` is
        // word-wrapped to `output_width()` and re-quoted whole by the
        // flush-left verdict banner, both of which put receiver text at
        // column 0; the hint path is indented on every line. The
        // DISCLOSURE is unchanged — same text, same quoting, same
        // escaping — so both halves are asserted: the count still names
        // the rejection on `message`, and the receiver's words are still
        // present, now on the hint.
        assert!(
            rule.message.contains("1 on the last delivery"),
            "the count must still ride the message: {}",
            rule.message
        );
        assert!(
            !rule.message.contains("payload field"),
            "receiver text must NOT ride the message any more: {}",
            rule.message
        );
        let hint = rule.hint.as_deref().unwrap_or_default();
        // Rejoined before matching: the hint carries the reason as its
        // own WRAPPED lines (bounded so doctor's hint prefix plus the
        // line stays under the narrowest supported terminal width), so
        // the text is complete but not contiguous. Asserting on the
        // rejoined form proves the disclosure survived the move whole —
        // asserting on a raw substring would only prove where the wrap
        // happened to fall.
        let rejoined = hint.split_whitespace().collect::<Vec<_>>().join(" ");
        assert!(
            rejoined.contains("\"payload field \\\"file\\\" must be a non-empty string\""),
            "the receiver's own reason must still be named, quoted and escaped: {hint}"
        );
    }

    /// (#2196 inverted case) A rejection with no reason on record (the
    /// receiver's body carried a count but no `results` detail) must not
    /// print an empty or garbled reason clause — the plain count-only
    /// message from before this fix.
    #[test]
    fn hooks_check_omits_the_reason_clause_when_none_was_recorded() {
        let tmp = tempfile::TempDir::new().unwrap();
        let (rule_cfg, key) = rejection_fixture_rule();
        let rules = vec![rule_cfg];
        std::fs::write(
            tmp.path().join(format!("{key}.last")),
            r#"{"ts":"2026-01-01T00:00:00Z","ok":true,"last_receiver_rejected":3}"#,
        )
        .unwrap();
        std::fs::write(tmp.path().join(format!("{key}.rejected")), "3").unwrap();

        let checks = build_hooks_check(true, "config.json", &rules, tmp.path(), std::path::Path::new(TEST_CONFIG_PATH));
        let rule = checks.iter().find(|c| c.name == "hooks.rule.0").unwrap();
        assert_eq!(rule.message.matches("3 on the last delivery").count(), 1, "{}", rule.message);
        assert!(!rule.message.contains("()"), "no empty parens when there's no reason: {}", rule.message);
    }

    /// (#2273 fix-round finding 1) The BLOCKER: `last_receiver_rejected`
    /// lives on the `.last` sidecar, which every terminal outcome
    /// truncate-replaces in full — so a clean delivery lands `ok: true`
    /// with NO rejection field and, if the check keyed on that field,
    /// erased the signal. On a live rule that is seconds after the loss.
    ///
    /// The fixture is the exact post-erasure state: many rejections
    /// counted, and a `.last` document from the clean delivery that
    /// followed them. `doctor` must still warn.
    #[test]
    fn hooks_check_still_warns_after_a_later_clean_delivery_erased_the_last_value() {
        let tmp = tempfile::TempDir::new().unwrap();
        let (rule_cfg, key) = rejection_fixture_rule();
        let rules = vec![rule_cfg];
        // What one clean delivery leaves behind after 400 rejected ones.
        std::fs::write(tmp.path().join(format!("{key}.last")), r#"{"ts":"2026-01-01T00:00:00Z","ok":true}"#).unwrap();
        std::fs::write(tmp.path().join(format!("{key}.rejected")), "400").unwrap();

        let checks = build_hooks_check(true, "config.json", &rules, tmp.path(), std::path::Path::new(TEST_CONFIG_PATH));
        let rule = checks.iter().find(|c| c.name == "hooks.rule.0").unwrap();
        assert_eq!(
            rule.status,
            Status::Warn,
            "400 rejections must not be erased by the one clean delivery that followed them: {}",
            rule.message
        );
        assert!(rule.message.contains("400 record(s) reported rejected by the receiver"), "{}", rule.message);
        assert!(
            !rule.message.contains("on the last delivery"),
            "the last delivery was clean — the message must not claim otherwise: {}",
            rule.message
        );
    }

    /// (#2273 inverted case) A rule that has never seen a rejection
    /// (`ok: true`, no rejection field, no counter sidecar) must NOT warn
    /// — the guard has to key on a count actually being non-zero, never
    /// on the rule merely having a delivery history at all. Without this,
    /// a red-prove of the warn guard by deleting its condition entirely
    /// could pass by accident if every fixture in the suite happened to
    /// carry a rejection.
    #[test]
    fn hooks_check_stays_quiet_when_last_delivery_was_cleanly_accepted() {
        let tmp = tempfile::TempDir::new().unwrap();
        let (rule_cfg, key) = rejection_fixture_rule();
        let rules = vec![rule_cfg];
        std::fs::write(tmp.path().join(format!("{key}.last")), r#"{"ts":"2026-01-01T00:00:00Z","ok":true}"#).unwrap();

        let checks = build_hooks_check(true, "config.json", &rules, tmp.path(), std::path::Path::new(TEST_CONFIG_PATH));
        let rule = checks.iter().find(|c| c.name == "hooks.rule.0").unwrap();
        assert_eq!(rule.status, Status::Pass, "{}", rule.message);
        assert!(!rule.message.contains("rejected"), "{}", rule.message);
    }

    /// (#2093 merge-gate finding 15) A `*.outbox.jsonl` file that belongs
    /// to no CURRENTLY-configured rule — the artifact of a rule since
    /// removed (or, before content-hash keying, silently reassigned by a
    /// reorder) — is named, not silently ignored.
    #[test]
    fn hooks_check_warns_on_stray_outbox_file() {
        use darkmux_types::config::{HookMatch, HookRule};
        let tmp = tempfile::TempDir::new().unwrap();
        let rules = vec![HookRule {
            r#match: Some(HookMatch { action: Some(live_action()), ..Default::default() }),
            http: Some("http://127.0.0.1:8790/events".to_string()),
            signing_secret_keychain_item: None,
            file: None,
            transform: None,
            headers: None,
            attribution_headers: None,
            extras: Default::default(),
        }];
        // A stray file belonging to a rule that's since been removed from
        // config — its key can't match any CURRENT rule's `rule_key`.
        std::fs::write(tmp.path().join("127.0.0.1-9999-deadbeefdeadbeef.outbox.jsonl"), "").unwrap();

        // (#2273 fix-round finding 1) The new per-rule counter sidecar is
        // one of the files an operator deciding "safe to delete?" has to
        // be shown — a sidecar missing from `HOOK_SIDECAR_SUFFIXES` is
        // silently left behind by whoever acts on this listing.
        std::fs::write(tmp.path().join("127.0.0.1-9999-deadbeefdeadbeef.rejected"), "5").unwrap();

        let checks = build_hooks_check(true, "config.json", &rules, tmp.path(), std::path::Path::new(TEST_CONFIG_PATH));
        let stray = checks.iter().find(|c| c.name == "hooks.stray").expect("a stray-file check must be present");
        assert_eq!(stray.status, Status::Warn, "{}", stray.message);
        assert!(stray.message.contains("127.0.0.1-9999-deadbeefdeadbeef"), "{}", stray.message);
        assert!(stray.message.contains("127.0.0.1-9999-deadbeefdeadbeef.rejected"), "{}", stray.message);
    }

    #[test]
    fn hooks_check_no_stray_file_check_when_nothing_stray() {
        use darkmux_types::config::{HookMatch, HookRule};
        let tmp = tempfile::TempDir::new().unwrap();
        let rules = vec![HookRule {
            r#match: Some(HookMatch { action: Some(live_action()), ..Default::default() }),
            http: Some("http://127.0.0.1:8790/events".to_string()),
            signing_secret_keychain_item: None,
            file: None,
            transform: None,
            headers: None,
            attribution_headers: None,
            extras: Default::default(),
        }];
        let checks = build_hooks_check(true, "config.json", &rules, tmp.path(), std::path::Path::new(TEST_CONFIG_PATH));
        assert!(checks.iter().all(|c| c.name != "hooks.stray"), "no stray files → no stray check emitted");
    }


    // ─── characterization: every branch of `build_hooks_check`, pinned ──────

    const LOOPBACK: &str = "http://127.0.0.1:8790/events";
    /// Stands in for `resolved_config_path()` — deliberately not under `~`,
    /// so a remedy that names `~/.darkmux/config.json` regardless of
    /// `DARKMUX_HOME` fails here.
    const TEST_CONFIG_PATH: &str = "/darkmux-root/config.json";
    const REMEDY: &str = "Fix this rule in /darkmux-root/config.json (or `darkmux config set hooks.rules ...`).";

    /// The action the fixtures' rules match: one darkmux writes today, taken
    /// from its [`FlowAction`] so a rename moves the fixtures with it rather
    /// than leaving every rule matching nothing.
    fn live_action() -> String {
        FlowAction::StepComplete.as_str().to_string()
    }

    fn hook_rule(action: Option<&str>, http: Option<&str>) -> darkmux_types::config::HookRule {
        darkmux_types::config::HookRule {
            r#match: action.map(|a| darkmux_types::config::HookMatch { action: Some(a.to_string()), ..Default::default() }),
            http: http.map(str::to_string),
            signing_secret_keychain_item: None,
            file: None,
            transform: None,
            headers: None,
            attribution_headers: None,
            extras: Default::default(),
        }
    }

    /// The per-rule sidecar key a rule's files are named by.
    fn key_of(r: &darkmux_types::config::HookRule) -> String {
        darkmux_flow::hooks::summarize_configured_rules(std::slice::from_ref(r), std::path::Path::new("/nonexistent"))
            .remove(0)
            .key
    }

    fn checks_for(rules: &[darkmux_types::config::HookRule], dir: &std::path::Path) -> Vec<Check> {
        build_hooks_check(true, "config.json", rules, dir, std::path::Path::new(TEST_CONFIG_PATH))
    }

    fn named<'a>(checks: &'a [Check], name: &str) -> &'a Check {
        checks.iter().find(|c| c.name == name).unwrap_or_else(|| panic!("no `{name}` check in {checks:?}"))
    }

    #[test]
    fn disabled_is_one_pass_row_even_with_rules_configured() {
        let tmp = tempfile::TempDir::new().unwrap();
        let rules = vec![hook_rule(None, Some("http://10.0.0.5/x"))];
        let checks = build_hooks_check(false, "env", &rules, tmp.path(), std::path::Path::new(TEST_CONFIG_PATH));
        assert_eq!(checks.len(), 1, "{checks:?}");
        assert_eq!(checks[0].name, "hooks");
        assert_eq!(checks[0].status, Status::Pass);
        assert_eq!(checks[0].message, "disabled (env)");
        assert!(checks[0].hint.is_none());
    }

    #[test]
    fn enabled_with_no_rules_names_the_outbox_dir_and_an_example_rule() {
        let tmp = tempfile::TempDir::new().unwrap();
        let checks = build_hooks_check(true, "config.json", &[], tmp.path(), std::path::Path::new(TEST_CONFIG_PATH));
        assert_eq!(checks.len(), 1, "{checks:?}");
        assert_eq!(checks[0].status, Status::Warn);
        assert_eq!(
            checks[0].message,
            format!("enabled (config.json) but no rules configured — outbox_dir={}", tmp.path().display())
        );
        let hint = checks[0].hint.as_deref().unwrap();
        assert!(hint.starts_with("Add a rule to /darkmux-root/config.json's `hooks.rules`"), "{hint}");
        assert!(hint.contains("darkmux config set hooks.rules"), "{hint}");
    }

    #[test]
    fn a_healthy_loopback_rule_is_one_clean_row_and_a_clean_overview() {
        let tmp = tempfile::TempDir::new().unwrap();
        let checks = checks_for(&[hook_rule(Some(&live_action()), Some(LOOPBACK))], tmp.path());
        assert_eq!(checks.len(), 2, "{checks:?}");
        let row = named(&checks, "hooks.rule.0");
        assert_eq!(row.status, Status::Pass);
        assert_eq!(row.message, format!("action={} -> {LOOPBACK} [loopback, unsigned] (undelivered: 0)", live_action()));
        assert!(row.hint.is_none(), "a clean rule carries no remedy");
        let overview = named(&checks, "hooks");
        assert_eq!(overview.status, Status::Pass);
        assert_eq!(
            overview.message,
            format!(
                "enabled (config.json) — 1 rule(s), outbox_dir={}\n  #0: {}",
                tmp.path().display(),
                row.message
            )
        );
        assert!(overview.hint.is_none());
    }

    #[test]
    fn dropped_writes_warn_with_their_count() {
        let tmp = tempfile::TempDir::new().unwrap();
        let rule = hook_rule(Some(&live_action()), Some(LOOPBACK));
        std::fs::write(tmp.path().join(format!("{}.dropped", key_of(&rule))), "2").unwrap();
        let checks = checks_for(&[rule], tmp.path());
        let row = named(&checks, "hooks.rule.0");
        assert_eq!(row.status, Status::Warn);
        assert!(
            row.message.ends_with("[2 write(s) dropped so far (over the outbox cap, or an append failure)]"),
            "{}",
            row.message
        );
        assert_eq!(row.hint.as_deref(), Some(DELIVERY_HINT), "dropped writes are not a config problem");
        let overview = named(&checks, "hooks");
        assert_eq!(overview.status, Status::Warn);
        assert_eq!(overview.hint.as_deref(), Some("See the individual `hooks.rule.*` checks below for which rule(s)."));
    }

    #[test]
    fn a_stalled_rule_warns_with_its_failure_count() {
        let tmp = tempfile::TempDir::new().unwrap();
        let rule = hook_rule(Some(&live_action()), Some(LOOPBACK));
        std::fs::write(
            tmp.path().join(format!("{}.last", key_of(&rule))),
            r#"{"ts":"2026-01-01T00:00:00Z","ok":false,"cursor_write_failures":3,"stalled":true}"#,
        )
        .unwrap();
        let checks = checks_for(&[rule], tmp.path());
        let row = named(&checks, "hooks.rule.0");
        assert_eq!(row.status, Status::Warn);
        assert!(row.message.contains("[STALLED — 3 consecutive cursor-write failure(s);"), "{}", row.message);
    }

    #[test]
    fn quarantined_lines_warn_with_their_count() {
        let tmp = tempfile::TempDir::new().unwrap();
        let rule = hook_rule(Some(&live_action()), Some(LOOPBACK));
        std::fs::write(tmp.path().join(format!("{}.outbox.jsonl.quarantine", key_of(&rule))), "x\ny\n").unwrap();
        let checks = checks_for(&[rule], tmp.path());
        let row = named(&checks, "hooks.rule.0");
        assert_eq!(row.status, Status::Warn);
        assert!(row.message.contains("[2 line(s) quarantined (invalid JSON — never redelivered)]"), "{}", row.message);
    }

    #[test]
    fn a_telemetry_category_match_warns_about_the_observer() {
        let tmp = tempfile::TempDir::new().unwrap();
        let mut rule = hook_rule(None, Some(LOOPBACK));
        rule.r#match =
            Some(darkmux_types::config::HookMatch { category: Some("telemetry".into()), ..Default::default() });
        let checks = checks_for(&[rule], tmp.path());
        let row = named(&checks, "hooks.rule.0");
        assert_eq!(row.status, Status::Warn);
        assert!(row.message.contains("observer must not join the observed"), "{}", row.message);
    }

    /// The observer check reads the rule's typed match, not its rendered
    /// description: a payload VALUE that happens to spell
    /// `category=telemetry` is not a telemetry match.
    #[test]
    fn a_payload_value_spelling_the_telemetry_category_is_not_the_observer_warning() {
        let tmp = tempfile::TempDir::new().unwrap();
        let mut m = darkmux_types::config::HookMatch { action: Some(live_action()), ..Default::default() };
        m.extras.insert("payload.note".into(), serde_json::json!("category=telemetry"));
        let mut rule = hook_rule(None, Some(LOOPBACK));
        rule.r#match = Some(m);
        let checks = checks_for(&[rule], tmp.path());
        let row = named(&checks, "hooks.rule.0");
        assert_eq!(row.status, Status::Pass, "{}", row.message);
    }

    #[test]
    fn a_bare_star_narrowed_by_another_predicate_is_not_the_observer_warning() {
        let tmp = tempfile::TempDir::new().unwrap();
        let mut rule = hook_rule(None, Some(LOOPBACK));
        rule.r#match = Some(darkmux_types::config::HookMatch {
            action: Some("*".into()),
            category: Some("work".into()),
            ..Default::default()
        });
        let checks = checks_for(&[rule], tmp.path());
        assert_eq!(named(&checks, "hooks.rule.0").status, Status::Pass);
    }

    fn observer_row(m: darkmux_types::config::HookMatch) -> Check {
        let tmp = tempfile::TempDir::new().unwrap();
        let mut rule = hook_rule(None, Some(LOOPBACK));
        rule.r#match = Some(m);
        named(&checks_for(&[rule], tmp.path()), "hooks.rule.0").clone()
    }

    /// The observer check IS the matcher: whatever `hook_match` would send a
    /// telemetry record to is flagged, however the rule spells it — a padded,
    /// capitalized category; a glob that reaches `machine.telemetry`; a
    /// bare `*` narrowed only to the level detector firings are written at.
    #[test]
    fn the_observer_check_flags_whatever_the_matcher_would_send_telemetry_to() {
        use darkmux_types::config::HookMatch;
        for m in [
            HookMatch { category: Some(" Telemetry ".into()), ..Default::default() },
            HookMatch { action: Some("machine.*".into()), ..Default::default() },
            HookMatch { action: Some("*".into()), level: Some("warn".into()), ..Default::default() },
        ] {
            let row = observer_row(m);
            assert!(row.message.contains("observer must not join the observed"), "{}", row.message);
        }
    }

    #[test]
    fn a_file_rule_reports_its_path_and_no_url_policy() {
        let tmp = tempfile::TempDir::new().unwrap();
        let mut rule = hook_rule(Some(&live_action()), None);
        rule.file = Some("/tmp/darkmux-hook-sink.jsonl".into());
        let checks = checks_for(&[rule], tmp.path());
        let row = named(&checks, "hooks.rule.0");
        assert_eq!(row.status, Status::Pass, "{}", row.message);
        assert_eq!(row.message, format!("action={} -> file:///tmp/darkmux-hook-sink.jsonl [file, n/a] (undelivered: 0)", live_action()));
    }

    #[test]
    fn a_failed_transform_fails_only_its_own_rule_row() {
        let tmp = tempfile::TempDir::new().unwrap();
        let mut broken = hook_rule(Some(&live_action()), Some(LOOPBACK));
        broken.transform = Some("no-such-adapter-for-this-test.jq".into());
        let checks = checks_for(&[broken, hook_rule(Some("dispatch.*"), Some(LOOPBACK))], tmp.path());
        let row = named(&checks, "hooks.rule.0");
        assert_eq!(row.status, Status::Fail);
        assert!(row.message.contains(", transform: no-such-adapter-for-this-test.jq [FAILED]"), "{}", row.message);
        assert!(row.message.contains("[TRANSFORM `no-such-adapter-for-this-test.jq` FAILED TO LOAD — "), "{}", row.message);
        assert_eq!(named(&checks, "hooks.rule.1").status, Status::Pass);
        assert_eq!(named(&checks, "hooks").status, Status::Fail);
    }

    #[test]
    #[serial_test::serial]
    fn a_loaded_transform_names_its_content_hash() {
        let state = darkmux_types::test_isolation::IsolatedState::new();
        let adapters = darkmux_types::config_access::hooks_adapters_dir();
        assert!(adapters.starts_with(state.path()), "the adapters dir must be the isolated one");
        std::fs::create_dir_all(&adapters).unwrap();
        std::fs::write(adapters.join("ok.jq"), ".").unwrap();
        let hash = darkmux_flow::hook_transform::load_adapter(&adapters, "ok.jq").unwrap().short_hash;
        let mut rule = hook_rule(Some(&live_action()), Some(LOOPBACK));
        rule.transform = Some("ok.jq".into());
        let checks = checks_for(&[rule], state.path());
        let row = named(&checks, "hooks.rule.0");
        assert_eq!(row.status, Status::Pass, "{}", row.message);
        assert!(row.message.contains(&format!(", transform: ok.jq (blake3:{hash})")), "the hash is BLAKE3: {}", row.message);
    }

    #[test]
    fn a_later_warn_rule_never_downgrades_an_earlier_fail() {
        let tmp = tempfile::TempDir::new().unwrap();
        let rules = [hook_rule(Some(&live_action()), Some("http://10.0.0.5/x")), hook_rule(None, Some(LOOPBACK))];
        let checks = checks_for(&rules, tmp.path());
        assert_eq!(named(&checks, "hooks.rule.0").status, Status::Fail);
        assert_eq!(named(&checks, "hooks.rule.1").status, Status::Warn);
        assert_eq!(named(&checks, "hooks").status, Status::Fail);
    }

    #[test]
    fn a_refused_url_with_an_empty_match_stays_fail_and_names_both() {
        let tmp = tempfile::TempDir::new().unwrap();
        let checks = checks_for(&[hook_rule(None, Some("http://10.0.0.5/x"))], tmp.path());
        let row = named(&checks, "hooks.rule.0");
        assert_eq!(row.status, Status::Fail);
        assert!(row.message.contains("[EMPTY MATCH — matches nothing; URL REFUSED —"), "{}", row.message);
        assert!(row.message.contains("[refused, unsigned]"), "{}", row.message);
    }

    #[test]
    fn a_rule_with_both_http_and_file_is_refused() {
        let tmp = tempfile::TempDir::new().unwrap();
        let mut rule = hook_rule(Some(&live_action()), Some(LOOPBACK));
        rule.file = Some("/tmp/x.jsonl".into());
        let checks = checks_for(&[rule], tmp.path());
        let row = named(&checks, "hooks.rule.0");
        assert_eq!(row.status, Status::Fail, "{}", row.message);
        assert_eq!(
            row.message,
            format!(
                "action={} -> {LOOPBACK} [refused, n/a] (undelivered: 0) [DESTINATION REFUSED — names \
                 BOTH `http` and `file` — a rule needs exactly one destination; refused at load]",
                live_action()
            ),
            "the URL itself is fine; what is refused is naming two destinations"
        );
    }

    /// A rule refused for its destination fields has no transport, so no
    /// transport flag either: a tailnet URL on it is never dialed, and
    /// "unsigned" says nothing about it.
    #[test]
    fn a_both_destinations_rule_with_a_tailnet_url_gets_no_transport_flags() {
        let tmp = tempfile::TempDir::new().unwrap();
        let mut rule = hook_rule(Some(&live_action()), Some("http://100.64.1.2:8790/e"));
        rule.file = Some("/tmp/x.jsonl".into());
        let checks = checks_for(&[rule], tmp.path());
        let row = named(&checks, "hooks.rule.0");
        assert_eq!(
            row.message,
            format!(
                "action={} -> http://100.64.1.2:8790/e [refused, n/a] (undelivered: 0) [DESTINATION REFUSED — \
                 names BOTH `http` and `file` — a rule needs exactly one destination; refused at load]",
                live_action()
            )
        );
    }

    #[test]
    fn a_rule_with_no_destination_is_refused() {
        let tmp = tempfile::TempDir::new().unwrap();
        let checks = checks_for(&[hook_rule(Some(&live_action()), None)], tmp.path());
        let row = named(&checks, "hooks.rule.0");
        assert_eq!(row.status, Status::Fail, "{}", row.message);
        assert_eq!(
            row.message,
            format!(
                "action={} -> (no destination) [refused, n/a] (undelivered: 0) [DESTINATION REFUSED — has no \
                 destination — set exactly one of `http` or `file`; refused at load]",
                live_action()
            ),
            "there is no URL to refuse; what is missing is a destination"
        );
    }

    #[test]
    fn receiver_reasons_lead_the_hint_and_the_delivery_hint_closes_it() {
        let tmp = tempfile::TempDir::new().unwrap();
        let rule = hook_rule(Some(&live_action()), Some(LOOPBACK));
        let key = key_of(&rule);
        std::fs::write(
            tmp.path().join(format!("{key}.last")),
            r#"{"ts":"2026-01-01T00:00:00Z","ok":true,"last_receiver_rejected":1,"last_receiver_rejected_reasons":["bad"]}"#,
        )
        .unwrap();
        std::fs::write(tmp.path().join(format!("{key}.rejected")), "1").unwrap();
        let checks = checks_for(&[rule], tmp.path());
        let hint = named(&checks, "hooks.rule.0").hint.clone().unwrap();
        let lines: Vec<&str> = hint.lines().collect();
        assert_eq!(lines.first().copied(), Some("the receiver's stated reason(s) for the last rejection:"));
        assert!(lines[1..lines.len() - 1].iter().any(|l| l.contains("\"bad\"")), "{hint}");
        assert_eq!(lines.last().copied(), Some(DELIVERY_HINT), "a receiver rejection is not a config problem");
    }

    /// Reasons are only quoted as "the last rejection's" when the last
    /// delivery actually carried a rejection count; otherwise nothing on the
    /// row says which delivery they belong to.
    #[test]
    fn reasons_without_a_last_delivery_count_are_not_quoted() {
        let tmp = tempfile::TempDir::new().unwrap();
        let rule = hook_rule(Some(&live_action()), Some(LOOPBACK));
        let key = key_of(&rule);
        std::fs::write(
            tmp.path().join(format!("{key}.last")),
            r#"{"ts":"2026-01-01T00:00:00Z","ok":true,"last_receiver_rejected_reasons":["stale"]}"#,
        )
        .unwrap();
        std::fs::write(tmp.path().join(format!("{key}.rejected")), "4").unwrap();
        let checks = checks_for(&[rule], tmp.path());
        let row = named(&checks, "hooks.rule.0");
        assert_eq!(row.status, Status::Warn);
        assert!(!row.message.contains("on the last delivery"), "{}", row.message);
        assert_eq!(row.hint.as_deref(), Some(DELIVERY_HINT), "no reason block without a count to attach it to");
    }

    /// A rule whose last terminal outcome was a give-up is not healthy: it
    /// warns, names the error in the hint (never the message — the text is
    /// not all darkmux's own, see `receiver_reason_lines`), and points at
    /// `flow status`, not at config.
    #[test]
    fn a_rule_whose_deliveries_give_up_warns_with_the_last_error() {
        let tmp = tempfile::TempDir::new().unwrap();
        let rule = hook_rule(Some(&live_action()), Some(LOOPBACK));
        std::fs::write(
            tmp.path().join(format!("{}.last", key_of(&rule))),
            r#"{"ts":"2026-01-01T00:00:00Z","ok":false,"error":"connection refused (os error 61)"}"#,
        )
        .unwrap();
        let checks = checks_for(&[rule], tmp.path());
        let row = named(&checks, "hooks.rule.0");
        assert_eq!(row.status, Status::Warn, "{}", row.message);
        assert!(row.message.ends_with("[deliveries are giving up — the last error is below]"), "{}", row.message);
        assert!(!row.message.contains("connection refused"), "{}", row.message);
        let hint = row.hint.clone().unwrap();
        let lines: Vec<&str> = hint.lines().collect();
        assert_eq!(lines.first().copied(), Some("the last delivery's error:"), "{hint}");
        assert!(lines.iter().any(|l| l.contains("connection refused (os error 61)")), "{hint}");
        assert_eq!(lines.last().copied(), Some(DELIVERY_HINT), "{hint}");
        assert!(!hint.contains("Fix this rule"), "{hint}");
    }

    /// The last error is not all darkmux's text: a raw newline or an over-wide
    /// value must not reach column 0 when doctor renders the row.
    #[test]
    fn a_give_up_error_renders_indented_and_bounded() {
        let tmp = tempfile::TempDir::new().unwrap();
        let rule = hook_rule(Some(&live_action()), Some(LOOPBACK));
        let err = format!("redirect refused\n● ok — every check passed {}", "q".repeat(300));
        std::fs::write(
            tmp.path().join(format!("{}.last", key_of(&rule))),
            serde_json::json!({"ts": "2026-01-01T00:00:00Z", "ok": false, "error": err}).to_string(),
        )
        .unwrap();
        let checks = checks_for(&[rule], tmp.path());
        let block = render_check_block(named(&checks, "hooks.rule.0"), 100);
        let min = darkmux_flow::hooks::MIN_SUPPORTED_TERMINAL_WIDTH;
        let mut error_lines = 0;
        for line in block.iter().map(|l| strip_ansi(l)) {
            assert!(!line.starts_with('●'), "{line:?}");
            if line.contains("qqqq") || line.contains("redirect refused") {
                error_lines += 1;
                assert!(line.starts_with("        "), "not indented: {line:?}");
                assert!(darkmux_flow::hooks::display_columns(&line) < min, "too wide: {line:?}");
            }
        }
        assert!(error_lines > 1, "{block:?}");
    }

    /// The text of the first `<pre><code>` block after `after` in `html`,
    /// with the handful of entities the guide uses decoded.
    fn guide_block(html: &str, after: &str) -> String {
        let from = html.find(after).unwrap_or_else(|| panic!("the guide has no {after:?}"));
        let open = from + html[from..].find("<pre><code>").unwrap() + "<pre><code>".len();
        let close = open + html[open..].find("</code></pre>").unwrap();
        html[open..close].replace("&lt;", "<").replace("&gt;", ">").replace("&quot;", "\"").replace("&amp;", "&")
    }

    /// The guide's `darkmux doctor` example is the row doctor prints for the
    /// guide's own Jira rule — the rule is parsed from the page, not rebuilt
    /// here, and only the adapter hash is a placeholder. The example cannot
    /// drift from either the rule above it or the renderer.
    #[test]
    #[serial_test::serial]
    fn the_guide_shows_the_row_doctor_prints_for_its_jira_rule() {
        let guide = include_str!("../../../docs/guide/crawl-and-hooks.html");
        let rule: darkmux_types::config::HookRule =
            serde_json::from_str(&guide_block(guide, "<h3>Transforms: reshaping a record")).expect("the Jira rule parses");
        let example = guide_block(guide, "<h4><code>darkmux doctor</code></h4>");

        let state = darkmux_types::test_isolation::IsolatedState::new();
        let adapters = darkmux_types::config_access::hooks_adapters_dir();
        std::fs::create_dir_all(&adapters).unwrap();
        let adapter = rule.transform.clone().expect("the guide's rule names a transform");
        std::fs::write(adapters.join(&adapter), ".").unwrap();
        let hash = darkmux_flow::hook_transform::load_adapter(&adapters, &adapter).unwrap().short_hash;
        let checks = checks_for(&[rule], state.path());
        let row = named(&checks, "hooks.rule.0").message.replace(&hash, "a1b2c3d4e5f6a7b8");
        assert_eq!(example, format!("#0: {row}"));
    }

    /// A clean last delivery after earlier give-ups clears the flag: the
    /// `.last` sidecar holds only the latest terminal outcome.
    #[test]
    fn a_clean_last_delivery_is_not_giving_up() {
        let tmp = tempfile::TempDir::new().unwrap();
        let rule = hook_rule(Some(&live_action()), Some(LOOPBACK));
        std::fs::write(tmp.path().join(format!("{}.last", key_of(&rule))), r#"{"ts":"2026-01-01T00:00:00Z","ok":true}"#)
            .unwrap();
        let checks = checks_for(&[rule], tmp.path());
        assert_eq!(named(&checks, "hooks.rule.0").status, Status::Pass);
    }

    /// Config-caused and delivery-side flags on one rule each bring their own
    /// hint: the config remedy for the one, `flow status` for the other.
    #[test]
    fn a_config_flag_and_a_delivery_flag_each_bring_their_own_hint() {
        let tmp = tempfile::TempDir::new().unwrap();
        let rule = hook_rule(None, Some(LOOPBACK));
        std::fs::write(tmp.path().join(format!("{}.dropped", key_of(&rule))), "1").unwrap();
        let checks = checks_for(&[rule], tmp.path());
        assert_eq!(named(&checks, "hooks.rule.0").hint.as_deref(), Some(format!("{REMEDY}\n{DELIVERY_HINT}").as_str()));
        let only_config = checks_for(&[hook_rule(None, Some(LOOPBACK))], &tmp.path().join("fresh"));
        assert_eq!(named(&only_config, "hooks.rule.0").hint.as_deref(), Some(REMEDY));
    }

    #[test]
    fn a_stray_outbox_counts_undelivered_lines_from_its_own_cursor() {
        let tmp = tempfile::TempDir::new().unwrap();
        let stray = "127.0.0.1-9999-0123456789abcdef";
        std::fs::write(tmp.path().join(format!("{stray}.outbox.jsonl")), "{\"a\":1}\n{\"a\":2}\n{\"a\":3}\n").unwrap();
        std::fs::write(tmp.path().join(format!("{stray}.cursor")), "8").unwrap();
        let checks = checks_for(&[hook_rule(Some(&live_action()), Some(LOOPBACK))], tmp.path());
        let row = named(&checks, "hooks.stray");
        assert_eq!(row.status, Status::Warn);
        assert_eq!(
            row.message,
            format!(
                "1 outbox file(s) belong to no currently-configured rule: {stray}.outbox.jsonl (2 undelivered \
                 line(s); siblings: {stray}.cursor)"
            )
        );
        assert!(row.hint.as_deref().unwrap().starts_with("A rule was removed or edited since these were written."));
    }

    /// The stray row lists files in name order, whatever order the
    /// filesystem returned them in, so two runs of doctor print the same row.
    #[test]
    fn stray_files_are_listed_in_name_order() {
        let stray = |name: &str| StrayOutbox { path: format!("/o/{name}").into(), undelivered: 0, siblings: Vec::new() };
        let check = stray_check(vec![stray("b.outbox.jsonl"), stray("a.outbox.jsonl")]).unwrap();
        assert!(
            check.message.ends_with(": a.outbox.jsonl (0 undelivered line(s)), b.outbox.jsonl (0 undelivered line(s))"),
            "{}",
            check.message
        );
    }

    /// A fresh install has no outbox dir yet: that is not a problem, and not
    /// a stray.
    #[test]
    fn a_missing_outbox_dir_is_not_a_stray_row() {
        let tmp = tempfile::TempDir::new().unwrap();
        let missing = tmp.path().join("never-created");
        let checks = checks_for(&[hook_rule(Some(&live_action()), Some(LOOPBACK))], &missing);
        assert_eq!(checks.len(), 2, "{checks:?}");
        assert_eq!(named(&checks, "hooks").status, Status::Pass);
    }

    #[test]
    fn a_configured_rules_own_outbox_is_never_stray() {
        let tmp = tempfile::TempDir::new().unwrap();
        let rule = hook_rule(Some(&live_action()), Some(LOOPBACK));
        std::fs::write(tmp.path().join(format!("{}.outbox.jsonl", key_of(&rule))), "{\"a\":1}\n").unwrap();
        let checks = checks_for(&[rule], tmp.path());
        assert!(checks.iter().all(|c| c.name != "hooks.stray"), "{checks:?}");
        assert!(named(&checks, "hooks.rule.0").message.contains("(undelivered: 1)"));
    }
}
