//! (#2093) The flow-record hook sink's doctor rows: whether hooks are
//! enabled (with provenance), one row per configured rule, and any outbox
//! file no current rule owns. Every rule-level problem is attached to the
//! rule it names (`hooks.rule.<index>`), with the overview row (`hooks`)
//! carrying the worst status across them.

use crate::{Check, Status};

/// (#2093) Surface the flow-record hook sink's resolved state: whether it's
/// enabled (with provenance), and — when it is — every configured rule's
/// match + URL + undelivered-line count, flagging a rule whose match is
/// empty (Warn — matches nothing, likely an operator forgot to fill it in)
/// or whose URL isn't loopback (Fail — `HookSink::new` refuses the whole
/// sink over this, so it's a hard block, not a suggestion).
///
/// (#2093 merge-gate finding 14) Returns ONE `Check` per flagged rule
/// (`hooks.rule.<index>`), not a single aggregate — so a flag attaches
/// to the rule it names in the checks list itself, the same shape
/// `eureka_checks()` already established for a check family with more
/// than one member. Provenance distinguishes `env` / `config.json` /
/// `default` (mirrors `check_step_command_timeout`'s own three-way
/// provenance) — previously any non-`env` case was reported as
/// `config.json` even when NEITHER tier actually set it.
pub(crate) fn check_hooks() -> Vec<Check> {
    // (#2450 review) Provenance comes from `config_access`, which owns the
    // `env > config.json > default` ladder for every setting. The local copy
    // this replaces asked the config tier via `DarkmuxConfig::load_resolved()`,
    // which has no #811 test seam and so read the operator's REAL config.json
    // from inside the unit tests — see `hooks_enabled_provenance`'s own doc.
    let provenance = darkmux_types::config_access::hooks_enabled_provenance();
    let enabled = darkmux_types::config_access::hooks_enabled();
    let rules = darkmux_types::config_access::hooks_rules();
    let outbox_dir = darkmux_types::config_access::hooks_outbox_dir();
    let today_actions = crate::today_flow_actions();
    build_hooks_check(enabled, provenance, &rules, &outbox_dir, &today_actions, &crate::resolved_config_path())
}

/// The literal `action=<value>` predicate from a `describe_match`
/// rendering, when present — the same string-based extraction
/// `hooks_match_risks_observing_the_observer` already performs against
/// this rendered form (`HookRuleSummary` carries only the description,
/// not the structured `HookMatch`; good enough for a doctor Warn, not a
/// security boundary). `describe_match` always emits `action=...` FIRST
/// when present, so a leading-prefix match is sufficient.
fn action_from_match_desc(match_desc: &str) -> Option<&str> {
    let rest = match_desc.strip_prefix("action=")?;
    Some(rest.split(", ").next().unwrap_or(rest))
}

/// (#2093 merge-gate finding 17) True when a rule's match risks the
/// observer joining the observed (this project's own doctrine,
/// CLAUDE.md's "The observer must not join the observed") — matching
/// `telemetry.*` / category `telemetry`, or a bare `*` action with no other
/// predicate, which (among everything else) catches every telemetry record.
/// Reads the typed match, so a payload value that merely spells one of
/// these is not mistaken for it.
fn hooks_match_risks_observing_the_observer(m: &darkmux_types::config::HookMatch) -> bool {
    let bare_star =
        m.action.as_deref() == Some("*") && darkmux_types::config::HookMatch { action: None, ..m.clone() }.is_empty();
    m.category.as_deref() == Some("telemetry")
        || m.action.as_deref().is_some_and(|a| a.starts_with("telemetry."))
        || bare_star
}

/// (#2093 merge-gate finding 15) `*.outbox.jsonl` files in `outbox_dir`
/// whose key (the content-hash `rule_key` — see `darkmux_flow::hooks`'
/// own doc) matches no CURRENTLY-configured rule. Belongs to a rule
/// since removed from config (or edited enough to change its
/// `match`/`http`) — the outbox still holds whatever was undelivered
/// when that happened, and nothing will ever drain it again unless the
/// rule comes back verbatim.
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

fn stray_outbox_files(rules: &[darkmux_types::config::HookRule], outbox_dir: &std::path::Path) -> Vec<StrayOutbox> {
    // (#2183) Reuse `summarize_configured_rules`'s OWN key derivation
    // (`.key`) rather than recomputing `rule_key` by hand from `r.http`
    // alone — a `file`-transport rule has no `http`, so hand-rolling this
    // from `r.http.unwrap_or_default()` would key every `file` rule on
    // the empty string and misreport its real outbox as stray.
    let current_keys: std::collections::HashSet<String> =
        darkmux_flow::hooks::summarize_configured_rules(rules, outbox_dir).into_iter().map(|s| s.key).collect();
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

/// The pure rollup `check_hooks()` delegates to — split out so it's testable
/// against synthetic rules without the global config/env tier (#811 empties
/// `config()` in test builds, so there's no way to inject `hooks.rules`
/// through the real accessor path in a unit test).
fn build_hooks_check(
    enabled: bool,
    provenance: &str,
    rules: &[darkmux_types::config::HookRule],
    outbox_dir: &std::path::Path,
    today_actions: &std::collections::HashSet<String>,
    config_path: &std::path::Path,
) -> Vec<Check> {
    let name = "hooks";
    if !enabled {
        return vec![Check { name: name.into(), status: Status::Pass, message: format!("disabled ({provenance})"), hint: None }];
    }
    if rules.is_empty() {
        return vec![Check {
            name: name.into(),
            status: Status::Warn,
            message: format!("enabled ({provenance}) but no rules configured — outbox_dir={}", outbox_dir.display()),
            hint: Some(
                "Add a rule to config.json's `hooks.rules`, e.g. `darkmux config set hooks.rules \
                 '[{\"match\":{\"action\":\"dispatch.tool\",\"payload.tool_name\":\"create_finding\",\
                 \"payload.ok\":true},\"http\":\"http://127.0.0.1:8790/events\"}]'`."
                    .into(),
            ),
        }];
    }

    let summaries = darkmux_flow::hooks::summarize_configured_rules(rules, outbox_dir);
    let mut worst = Status::Pass;
    let mut overview_lines = Vec::with_capacity(summaries.len());
    let mut rule_checks = Vec::with_capacity(summaries.len());

    for (s, rule) in summaries.iter().zip(rules) {
        let rule_match = rule.r#match.clone().unwrap_or_default();
        let mut flags = Vec::new();
        // (#2196 fix-round 4, MUST FIX G at the doctor surface) The
        // receiver's own reason text NEVER joins `message`. It rides
        // these pre-bounded hint lines instead — see the `receiver_rejected_total`
        // block below for why, and `darkmux_flow::hooks::rejection_reason_display_lines`
        // for the budget.
        let mut reason_hint_lines: Vec<String> = Vec::new();
        let mut rule_status = Status::Pass;
        if s.is_empty_match {
            flags.push("EMPTY MATCH — matches nothing".to_string());
            rule_status = Status::Warn;
        }
        // (#2135 option 2) A URL satisfying NEITHER the loopback nor the
        // tailnet policy is what `HookSink::new` refuses the whole sink
        // over — a valid tailnet rule (`is_tailnet: true`) is NOT this
        // case and must not read as broken.
        //
        // A rule with both or neither of `http`/`file` is refused for its
        // destination FIELDS, not its URL, and says so.
        if let Some(problem) = s.destination_problem {
            flags.push(format!("DESTINATION REFUSED — {}; refused at load", problem.describe()));
            rule_status = Status::Fail;
        } else if s.is_refused {
            flags.push("URL REFUSED — neither loopback nor a Tailscale address; refused at load".to_string());
            rule_status = Status::Fail;
        }
        // (#2135 option 2) An unsigned TAILNET target is fine inside the
        // tailnet (WireGuard already authenticates + encrypts the peer),
        // but the receiver has no way to attribute the record's sender
        // beyond the body itself — worth a Warn, not a Fail.
        if s.is_tailnet && !s.signed {
            flags.push(
                "TAILNET TARGET, UNSIGNED — attribution is unsigned; fine inside the tailnet, required beyond it"
                    .to_string(),
            );
            if rule_status == Status::Pass {
                rule_status = Status::Warn;
            }
        }
        // (#2093 merge-gate finding 9) A rule that's been dropping writes
        // (over the outbox cap, or an append failure) is a Warn — not a
        // Fail, since delivery for every OTHER pending line keeps working.
        if s.dropped_appends > 0 {
            flags.push(format!(
                "{} write(s) dropped so far (over the outbox cap, or an append failure)",
                s.dropped_appends
            ));
            if rule_status == Status::Pass {
                rule_status = Status::Warn;
            }
        }
        // (fix-round finding 1) A STALLED rule has stopped attempting
        // deliveries entirely — surfaced loudly, same severity as the
        // other operational (not config-validation) flags here.
        if s.stalled {
            flags.push(format!(
                "STALLED — {} consecutive cursor-write failure(s); the drainer has stopped attempting new \
                 deliveries for this rule until its cursor file becomes writable again",
                s.cursor_write_failures
            ));
            if rule_status == Status::Pass {
                rule_status = Status::Warn;
            }
        }
        // (fix-round finding 7) Quarantined (invalid-JSON) lines are
        // never redelivered — worth naming, same as a dropped append.
        if s.quarantined_lines > 0 {
            flags.push(format!("{} line(s) quarantined (invalid JSON — never redelivered)", s.quarantined_lines));
            if rule_status == Status::Pass {
                rule_status = Status::Warn;
            }
        }
        // (#2273) The receiver accepted a delivery's HTTP request (2xx)
        // but its own response body reported it rejected some or all of
        // the record(s) inside it — a THIRD outcome, distinct from a
        // transport failure and a clean accept. darkmux never retries
        // this: a receiver-side content rejection is (per
        // `DeliveryOutcome::Success`'s own doc) usually permanent, so
        // retrying would just repeat it forever — the line is consumed
        // same as a clean delivery. This is where an operator who missed
        // the `hook.fired` flow record (now emitted at Warn, not Info,
        // for exactly this case) still finds out it happened.
        //
        // (#2273 fix-round finding 1) Keyed on the CUMULATIVE
        // `receiver_rejected_total`, never on `last_receiver_rejected`.
        // The latter lives on the `.last` sidecar, which every terminal
        // outcome truncate-replaces in full — so 400 rejections followed
        // by ONE clean delivery leaves it `None`, and a check keyed on it
        // reports the rule clean seconds after those losses. The total is
        // a counter sidecar of its own (`<key>.rejected`), never reset —
        // the same substrate `dropped_appends` uses, for the same reason.
        // The last delivery's own count is still named when present, as
        // context.
        //
        // Describing only, per this project's stance: names the count and
        // the actor, never characterizes the receiver as misconfigured.
        if s.receiver_rejected_total > 0 {
            // (#2196) The receiver's own stated reason(s) for that last
            // rejection, when its body carried any — the count alone
            // tells an operator SOMETHING was thrown away, never WHY, so
            // this is what saves a replay against a scratch receiver or a
            // trip through the receiver's own log to find out.
            //
            // (#2196 fix-round 4, MUST FIX G at the doctor surface) The
            // reason text is deliberately NOT interpolated into
            // `message`, which is where it lived through fix-round 3.
            // Two independent reasons, both structural:
            //
            //  * `message` is word-wrapped by `render_check_block` to
            //    `output_width()` and its CONTINUATIONS are indented —
            //    but `output_width()` reads `COLUMNS`, and `COLUMNS` is
            //    not exported to child processes by zsh or bash
            //    (measured). So doctor renders at its 100-column DEFAULT
            //    however wide the operator's terminal really is, the
            //    terminal re-wraps every line past its own width, and the
            //    continuation lands at column 0 — where doctor's three
            //    flush-left rows live (the `darkmux doctor — N checks`
            //    header, the `●` verdict banner, and the summary). Proven
            //    against the real renderer: a receiver could put
            //    `● ok — every check passed` at column 0 of a 60-column
            //    terminal, under a genuine `broken` banner.
            //  * `verdict_banner_at` quotes the worst check's whole
            //    `message` onto a FLUSH-LEFT line of its own. Anything in
            //    `message` is one status away from being printed at
            //    column 0 with no indent at all.
            //
            // The hint path has neither problem: every hint line is
            // printed behind `"        → "` or ten spaces, and the lines
            // below are pre-bounded so the finished row stays under the
            // narrowest supported terminal width.
            let last_clause = match s.last_receiver_rejected {
                Some(n) if !s.last_receiver_rejected_reasons.is_empty() => {
                    reason_hint_lines = darkmux_flow::hooks::rejection_reason_display_lines(
                        &s.last_receiver_rejected_reasons,
                        darkmux_flow::hooks::REJECTION_REASON_HINT_INDENT,
                    );
                    format!("; {n} on the last delivery — the receiver's reason(s) below")
                }
                Some(n) => format!("; {n} on the last delivery"),
                None => String::new(),
            };
            flags.push(format!(
                "{} record(s) reported rejected by the receiver so far (request accepted, content \
                 rejected — consumed, not retried){last_clause}",
                s.receiver_rejected_total
            ));
            if rule_status == Status::Pass {
                rule_status = Status::Warn;
            }
        }
        if hooks_match_risks_observing_the_observer(&rule_match) {
            flags.push(
                "matches telemetry / a bare `*` action — the observer must not join the observed".to_string(),
            );
            if rule_status == Status::Pass {
                rule_status = Status::Warn;
            }
        }
        // (silent-miss audit, 2026-09-06) A rule with ZERO deliveries ever
        // (nothing currently undelivered, and no terminal outcome has ever
        // landed) reads as merely quiet — a healthy rule waiting for a
        // matching record is indistinguishable from one that has NEVER
        // matched a single record because it was written against the
        // wrong bookend spelling (`HookMatch::action` is a literal glob;
        // it does NOT tolerate both spellings the way `darkmux_flow`'s
        // shared matchers do). If today's flow day file holds at least
        // one record carrying the OTHER spelling of this rule's configured
        // action, that silence has an explanation worth naming instead of
        // leaving the operator to notice only when nothing ever arrives.
        if s.undelivered == 0 && s.last_delivery_ts.is_none() {
            if let Some(configured) = action_from_match_desc(&s.match_desc) {
                if let Some(other) = crate::other_bookend_spelling(configured) {
                    if today_actions.contains(other) {
                        flags.push(format!(
                            "NEVER MATCHED (zero deliveries) — configured for action=\"{configured}\", but \
                             today's flow records use \"{other}\" instead; this looks like a bookend-spelling \
                             mismatch, not a quiet rule"
                        ));
                        if rule_status == Status::Pass {
                            rule_status = Status::Warn;
                        }
                    }
                }
            }
        }
        if worst == Status::Pass && rule_status != Status::Pass {
            worst = rule_status;
        } else if rule_status == Status::Fail {
            worst = Status::Fail;
        }

        // (#2183) A `transform` that failed to load is a load-time
        // refusal SCOPED TO THIS RULE (`HookSink::new` disables just this
        // rule, the rest of the sink keeps running) — Fail here too, so
        // the row that's actually broken is the one operator sees red,
        // without the whole `hooks` check reading as catastrophic.
        let transform_suffix = match (&s.transform_name, &s.transform_status) {
            (Some(name), Some(Ok(hash))) => format!(", transform: {name} (blake3:{hash})"),
            (Some(name), Some(Err(reason))) => {
                flags.push(format!("TRANSFORM `{name}` FAILED TO LOAD — {reason}"));
                rule_status = Status::Fail;
                format!(", transform: {name} [FAILED]")
            }
            _ => String::new(),
        };
        if worst == Status::Pass && rule_status != Status::Pass {
            worst = rule_status;
        } else if rule_status == Status::Fail {
            worst = Status::Fail;
        }

        let flag_str = if flags.is_empty() { String::new() } else { format!(" [{}]", flags.join("; ")) };
        // (#2135 option 2) `loopback`/`tailnet`/`refused` + `signed`/
        // `unsigned` — the visibility the operator's design asked for in
        // place of a config gate: the URL is the decision, this row is
        // what makes it legible. (#2183) `file` names the no-network
        // testing-tier transport instead — there's no URL policy or
        // signature to report for it.
        let target_kind = if s.destination_problem.is_some() {
            "refused"
        } else if s.is_file {
            "file"
        } else if s.is_loopback {
            "loopback"
        } else if s.is_tailnet {
            "tailnet"
        } else {
            "refused"
        };
        let signed = if s.is_file { "n/a" } else if s.signed { "signed" } else { "unsigned" };
        let message = format!(
            "{} -> {} [{target_kind}, {signed}]{transform_suffix} (undelivered: {}){flag_str}",
            s.match_desc, s.url, s.undelivered
        );
        overview_lines.push(format!("  #{}: {message}", s.index));
        rule_checks.push(Check {
            name: format!("hooks.rule.{}", s.index),
            status: rule_status,
            message,
            hint: {
                // (#2196 fix-round 4) The receiver's quoted reason(s)
                // ride here as their own lines, ahead of the config
                // remedy — they are EVIDENCE, and an operator reading a
                // rejection wants the receiver's words before any advice
                // about the local config. Each line is already bounded so
                // that doctor's hint prefix plus the line stays under the
                // narrowest supported terminal width; doctor's own
                // `wrap_hanging` only ever narrows a line further, so the
                // bound survives whatever `output_width()` resolves to.
                let mut hint_lines: Vec<String> = Vec::new();
                if !reason_hint_lines.is_empty() {
                    hint_lines.push("the receiver's stated reason(s) for the last rejection:".into());
                    hint_lines.extend(reason_hint_lines.iter().cloned());
                }
                if !flags.is_empty() {
                    hint_lines.push(format!(
                        "Fix this rule in {} (or `darkmux config set hooks.rules ...`).",
                        config_path.display()
                    ));
                }
                if hint_lines.is_empty() {
                    None
                } else {
                    Some(hint_lines.join("\n"))
                }
            },
        });
    }

    let overview = Check {
        name: name.into(),
        status: worst,
        message: format!(
            "enabled ({provenance}) — {} rule(s), outbox_dir={}\n{}",
            summaries.len(),
            outbox_dir.display(),
            overview_lines.join("\n")
        ),
        hint: if worst != Status::Pass {
            Some("See the individual `hooks.rule.*` checks below for which rule(s).".into())
        } else {
            None
        },
    };
    let mut out = vec![overview];
    out.extend(rule_checks);

    // (#2093 merge-gate finding 15) A file that belongs to no CURRENT
    // rule — named so, rather than silently taking up disk forever.
    let stray = stray_outbox_files(rules, outbox_dir);
    if !stray.is_empty() {
        // (fix-round finding 6) Name each stray file's undelivered line
        // count and its sibling sidecars — an operator deciding whether
        // it's "safe to delete" needs both, not just the outbox name.
        let details: Vec<String> = stray
            .iter()
            .map(|s| {
                let name = s.path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                let siblings =
                    if s.siblings.is_empty() { String::new() } else { format!("; siblings: {}", s.siblings.join(", ")) };
                format!("{name} ({} undelivered line(s){siblings})", s.undelivered)
            })
            .collect();
        out.push(Check {
            name: "hooks.stray".into(),
            status: Status::Warn,
            message: format!("{} outbox file(s) belong to no currently-configured rule: {}", stray.len(), details.join(", ")),
            hint: Some(
                "A rule was removed or edited since these were written. `darkmux flow drain --file <path> \
                 --to <loopback url>` delivers a stray file's undelivered lines before you delete it; once \
                 undelivered is 0, it (and its sibling sidecars) are safe to remove."
                    .into(),
            ),
        });
    }

    out
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
                r#match: Some(HookMatch { action: Some("crawl.*".to_string()), ..Default::default() }),
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
        let checks = build_hooks_check(true, "config.json", &rules, tmp.path(), &std::collections::HashSet::new(), std::path::Path::new(TEST_CONFIG_PATH));
        assert_eq!(checks.len(), 4, "1 overview + 3 per-rule checks");

        let overview = checks.iter().find(|c| c.name == "hooks").unwrap();
        assert_eq!(overview.status, Status::Fail, "worst of the three rules — a non-loopback rule is a hard block");

        let healthy = checks.iter().find(|c| c.name == "hooks.rule.0").unwrap();
        assert_eq!(healthy.status, Status::Pass, "{}", healthy.message);
        assert!(healthy.message.contains("crawl.*"), "{}", healthy.message);
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
            r#match: Some(HookMatch { action: Some("crawl.*".to_string()), ..Default::default() }),
            http: Some("http://100.64.1.2:8790/events".to_string()),
            signing_secret_keychain_item: None,
            file: None,
            transform: None,
            headers: None,
            attribution_headers: None,
            extras: Default::default(),
        }];
        let checks = build_hooks_check(true, "config.json", &rules, tmp.path(), &std::collections::HashSet::new(), std::path::Path::new(TEST_CONFIG_PATH));
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
            r#match: Some(HookMatch { action: Some("crawl.*".to_string()), ..Default::default() }),
            http: Some("http://100.64.1.2:8790/events".to_string()),
            signing_secret_keychain_item: Some("darkmux-hook-0".to_string()),
            file: None,
            transform: None,
            headers: None,
            attribution_headers: None,
            extras: Default::default(),
        }];
        let checks = build_hooks_check(true, "config.json", &rules, tmp.path(), &std::collections::HashSet::new(), std::path::Path::new(TEST_CONFIG_PATH));
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
        let checks = build_hooks_check(true, "config.json", &rules, tmp.path(), &std::collections::HashSet::new(), std::path::Path::new(TEST_CONFIG_PATH));
        let telemetry = checks.iter().find(|c| c.name == "hooks.rule.0").unwrap();
        assert_eq!(telemetry.status, Status::Warn, "{}", telemetry.message);
        assert!(telemetry.message.contains("observer must not join the observed"), "{}", telemetry.message);

        let bare_star = checks.iter().find(|c| c.name == "hooks.rule.1").unwrap();
        assert_eq!(bare_star.status, Status::Warn, "{}", bare_star.message);
        assert!(bare_star.message.contains("observer must not join the observed"), "{}", bare_star.message);
    }

    /// (silent-miss audit, 2026-09-06) A rule configured for the DOTTED
    /// spelling (`dispatch.complete`) that has NEVER delivered anything
    /// (fresh outbox dir: `undelivered == 0`, `last_delivery_ts == None`)
    /// reads as merely quiet — UNTIL today's flow day file is shown to
    /// carry the SPACED spelling instead, which is exactly the
    /// bookend-spelling mismatch `HookMatch::action`'s literal glob
    /// cannot tolerate (unlike `darkmux_flow`'s shared matchers). Both
    /// spellings must be named.
    #[test]
    fn hooks_check_warns_when_rule_never_matched_but_todays_records_use_the_other_spelling() {
        use darkmux_types::config::{HookMatch, HookRule};
        let tmp = tempfile::TempDir::new().unwrap();
        let rules = vec![HookRule {
            r#match: Some(HookMatch { action: Some("dispatch.complete".to_string()), ..Default::default() }),
            http: Some("http://127.0.0.1:8790/events".to_string()),
            signing_secret_keychain_item: None,
            file: None,
            transform: None,
            headers: None,
            attribution_headers: None,
            extras: Default::default(),
        }];
        let mut today_actions = std::collections::HashSet::new();
        today_actions.insert("dispatch complete".to_string());

        let checks = build_hooks_check(true, "config.json", &rules, tmp.path(), &today_actions, std::path::Path::new(TEST_CONFIG_PATH));
        let rule = checks.iter().find(|c| c.name == "hooks.rule.0").unwrap();
        assert_eq!(rule.status, Status::Warn, "{}", rule.message);
        assert!(rule.message.contains("NEVER MATCHED"), "{}", rule.message);
        assert!(rule.message.contains("dispatch.complete"), "must name the CONFIGURED spelling: {}", rule.message);
        assert!(rule.message.contains("dispatch complete"), "must name the OTHER spelling seen: {}", rule.message);
    }

    /// The negative space around the test above: with NOTHING in today's
    /// flow day file naming the other spelling, the same never-delivered
    /// rule stays Pass — a genuinely quiet, correctly-configured rule
    /// (e.g. one waiting for its first matching dispatch of the day) must
    /// not be flagged.
    #[test]
    fn hooks_check_no_alias_warn_when_todays_actions_dont_carry_the_other_spelling() {
        use darkmux_types::config::{HookMatch, HookRule};
        let tmp = tempfile::TempDir::new().unwrap();
        let rules = vec![HookRule {
            r#match: Some(HookMatch { action: Some("dispatch.complete".to_string()), ..Default::default() }),
            http: Some("http://127.0.0.1:8790/events".to_string()),
            signing_secret_keychain_item: None,
            file: None,
            transform: None,
            headers: None,
            attribution_headers: None,
            extras: Default::default(),
        }];
        // Empty today_actions: no evidence of the alias, so no warn.
        let checks = build_hooks_check(true, "config.json", &rules, tmp.path(), &std::collections::HashSet::new(), std::path::Path::new(TEST_CONFIG_PATH));
        let rule = checks.iter().find(|c| c.name == "hooks.rule.0").unwrap();
        assert_eq!(rule.status, Status::Pass, "{}", rule.message);
        assert!(!rule.message.contains("NEVER MATCHED"), "{}", rule.message);
    }

    /// (round-2 audit, 2026-09-06 — C4) The other half of the negative
    /// space: a rule that HAS actually delivered (a real `.last` sidecar
    /// from a genuine terminal outcome, the same shape the drainer
    /// writes) must stay Pass even when today's flow day file ALSO
    /// happens to carry the other spelling of its configured action —
    /// the alias-drift Warn is specifically for a rule that has NEVER
    /// matched anything; a rule that clearly HAS matched (and delivered)
    /// is not that case, whatever else today's records contain. Red-proved
    /// by replacing the `undelivered == 0 && last_delivery_ts.is_none()`
    /// gate with `if true`: this test then fails because it would warn
    /// regardless of the genuine prior delivery.
    #[test]
    fn hooks_check_no_alias_warn_when_the_rule_has_actually_delivered() {
        use darkmux_types::config::{HookMatch, HookRule};
        let tmp = tempfile::TempDir::new().unwrap();
        let m = HookMatch { action: Some("dispatch.complete".to_string()), ..Default::default() };
        let url = "http://127.0.0.1:8790/events".to_string();
        let rules = vec![HookRule {
            r#match: Some(m.clone()),
            http: Some(url.clone()),
            signing_secret_keychain_item: None,
            file: None,
            transform: None,
            headers: None,
            attribution_headers: None,
            extras: Default::default(),
        }];
        // A genuine prior delivery: the `.last` sidecar the drainer
        // itself writes on a terminal outcome (`write_last_status`).
        let key = darkmux_flow::hooks::rule_key(&m, &url);
        std::fs::write(tmp.path().join(format!("{key}.last")), r#"{"ts":"2026-01-01T00:00:00Z","ok":true}"#).unwrap();

        let mut today_actions = std::collections::HashSet::new();
        today_actions.insert("dispatch complete".to_string()); // the other spelling, ALSO present today

        let checks = build_hooks_check(true, "config.json", &rules, tmp.path(), &today_actions, std::path::Path::new(TEST_CONFIG_PATH));
        let rule = checks.iter().find(|c| c.name == "hooks.rule.0").unwrap();
        assert_eq!(rule.status, Status::Pass, "{}", rule.message);
        assert!(
            !rule.message.contains("NEVER MATCHED"),
            "a rule with an actual prior delivery must not be flagged, even with the other \
             spelling also present today: {}",
            rule.message
        );
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

        let checks = build_hooks_check(true, "config.json", &rules, tmp.path(), &std::collections::HashSet::new(), std::path::Path::new(TEST_CONFIG_PATH));
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
        let m = HookMatch { action: Some("crawl.*".to_string()), ..Default::default() };
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

        let checks = build_hooks_check(true, "config.json", &rules, tmp.path(), &std::collections::HashSet::new(), std::path::Path::new(TEST_CONFIG_PATH));
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

        let checks = build_hooks_check(true, "config.json", &rules, tmp.path(), &std::collections::HashSet::new(), std::path::Path::new(TEST_CONFIG_PATH));
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

        let checks = build_hooks_check(true, "config.json", &rules, tmp.path(), &std::collections::HashSet::new(), std::path::Path::new(TEST_CONFIG_PATH));
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

        let checks = build_hooks_check(true, "config.json", &rules, tmp.path(), &std::collections::HashSet::new(), std::path::Path::new(TEST_CONFIG_PATH));
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

        let checks = build_hooks_check(true, "config.json", &rules, tmp.path(), &std::collections::HashSet::new(), std::path::Path::new(TEST_CONFIG_PATH));
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
            r#match: Some(HookMatch { action: Some("crawl.*".to_string()), ..Default::default() }),
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

        let checks = build_hooks_check(true, "config.json", &rules, tmp.path(), &std::collections::HashSet::new(), std::path::Path::new(TEST_CONFIG_PATH));
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
            r#match: Some(HookMatch { action: Some("crawl.*".to_string()), ..Default::default() }),
            http: Some("http://127.0.0.1:8790/events".to_string()),
            signing_secret_keychain_item: None,
            file: None,
            transform: None,
            headers: None,
            attribution_headers: None,
            extras: Default::default(),
        }];
        let checks = build_hooks_check(true, "config.json", &rules, tmp.path(), &std::collections::HashSet::new(), std::path::Path::new(TEST_CONFIG_PATH));
        assert!(checks.iter().all(|c| c.name != "hooks.stray"), "no stray files → no stray check emitted");
    }


    // ─── characterization: every branch of `build_hooks_check`, pinned ──────

    const LOOPBACK: &str = "http://127.0.0.1:8790/events";
    /// Stands in for `resolved_config_path()` — deliberately not under `~`,
    /// so a remedy that names `~/.darkmux/config.json` regardless of
    /// `DARKMUX_HOME` fails here.
    const TEST_CONFIG_PATH: &str = "/darkmux-root/config.json";
    const REMEDY: &str = "Fix this rule in /darkmux-root/config.json (or `darkmux config set hooks.rules ...`).";

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
        build_hooks_check(true, "config.json", rules, dir, &std::collections::HashSet::new(), std::path::Path::new(TEST_CONFIG_PATH))
    }

    fn named<'a>(checks: &'a [Check], name: &str) -> &'a Check {
        checks.iter().find(|c| c.name == name).unwrap_or_else(|| panic!("no `{name}` check in {checks:?}"))
    }

    #[test]
    fn disabled_is_one_pass_row_even_with_rules_configured() {
        let tmp = tempfile::TempDir::new().unwrap();
        let rules = vec![hook_rule(None, Some("http://10.0.0.5/x"))];
        let checks = build_hooks_check(false, "env", &rules, tmp.path(), &std::collections::HashSet::new(), std::path::Path::new(TEST_CONFIG_PATH));
        assert_eq!(checks.len(), 1, "{checks:?}");
        assert_eq!(checks[0].name, "hooks");
        assert_eq!(checks[0].status, Status::Pass);
        assert_eq!(checks[0].message, "disabled (env)");
        assert!(checks[0].hint.is_none());
    }

    #[test]
    fn enabled_with_no_rules_names_the_outbox_dir_and_an_example_rule() {
        let tmp = tempfile::TempDir::new().unwrap();
        let checks = build_hooks_check(true, "config.json", &[], tmp.path(), &std::collections::HashSet::new(), std::path::Path::new(TEST_CONFIG_PATH));
        assert_eq!(checks.len(), 1, "{checks:?}");
        assert_eq!(checks[0].status, Status::Warn);
        assert_eq!(
            checks[0].message,
            format!("enabled (config.json) but no rules configured — outbox_dir={}", tmp.path().display())
        );
        let hint = checks[0].hint.as_deref().unwrap();
        assert!(hint.starts_with("Add a rule to config.json's `hooks.rules`"), "{hint}");
        assert!(hint.contains("darkmux config set hooks.rules"), "{hint}");
    }

    #[test]
    fn a_healthy_loopback_rule_is_one_clean_row_and_a_clean_overview() {
        let tmp = tempfile::TempDir::new().unwrap();
        let checks = checks_for(&[hook_rule(Some("crawl.*"), Some(LOOPBACK))], tmp.path());
        assert_eq!(checks.len(), 2, "{checks:?}");
        let row = named(&checks, "hooks.rule.0");
        assert_eq!(row.status, Status::Pass);
        assert_eq!(row.message, format!("action=crawl.* -> {LOOPBACK} [loopback, unsigned] (undelivered: 0)"));
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
        let rule = hook_rule(Some("crawl.*"), Some(LOOPBACK));
        std::fs::write(tmp.path().join(format!("{}.dropped", key_of(&rule))), "2").unwrap();
        let checks = checks_for(&[rule], tmp.path());
        let row = named(&checks, "hooks.rule.0");
        assert_eq!(row.status, Status::Warn);
        assert!(
            row.message.ends_with("[2 write(s) dropped so far (over the outbox cap, or an append failure)]"),
            "{}",
            row.message
        );
        assert_eq!(row.hint.as_deref(), Some(REMEDY));
        let overview = named(&checks, "hooks");
        assert_eq!(overview.status, Status::Warn);
        assert_eq!(overview.hint.as_deref(), Some("See the individual `hooks.rule.*` checks below for which rule(s)."));
    }

    #[test]
    fn a_stalled_rule_warns_with_its_failure_count() {
        let tmp = tempfile::TempDir::new().unwrap();
        let rule = hook_rule(Some("crawl.*"), Some(LOOPBACK));
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
        let rule = hook_rule(Some("crawl.*"), Some(LOOPBACK));
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
        let mut m = darkmux_types::config::HookMatch { action: Some("crawl.finding".into()), ..Default::default() };
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
            level: Some("warn".into()),
            ..Default::default()
        });
        let checks = checks_for(&[rule], tmp.path());
        assert_eq!(named(&checks, "hooks.rule.0").status, Status::Pass);
    }

    #[test]
    fn a_file_rule_reports_its_path_and_no_url_policy() {
        let tmp = tempfile::TempDir::new().unwrap();
        let mut rule = hook_rule(Some("crawl.*"), None);
        rule.file = Some("/tmp/darkmux-hook-sink.jsonl".into());
        let checks = checks_for(&[rule], tmp.path());
        let row = named(&checks, "hooks.rule.0");
        assert_eq!(row.status, Status::Pass, "{}", row.message);
        assert_eq!(row.message, "action=crawl.* -> file:///tmp/darkmux-hook-sink.jsonl [file, n/a] (undelivered: 0)");
    }

    #[test]
    fn a_failed_transform_fails_only_its_own_rule_row() {
        let tmp = tempfile::TempDir::new().unwrap();
        let mut broken = hook_rule(Some("crawl.*"), Some(LOOPBACK));
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
        let mut rule = hook_rule(Some("crawl.*"), Some(LOOPBACK));
        rule.transform = Some("ok.jq".into());
        let checks = checks_for(&[rule], state.path());
        let row = named(&checks, "hooks.rule.0");
        assert_eq!(row.status, Status::Pass, "{}", row.message);
        assert!(row.message.contains(&format!(", transform: ok.jq (blake3:{hash})")), "the hash is BLAKE3: {}", row.message);
    }

    #[test]
    fn a_later_warn_rule_never_downgrades_an_earlier_fail() {
        let tmp = tempfile::TempDir::new().unwrap();
        let rules = [hook_rule(Some("crawl.*"), Some("http://10.0.0.5/x")), hook_rule(None, Some(LOOPBACK))];
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
        let mut rule = hook_rule(Some("crawl.*"), Some(LOOPBACK));
        rule.file = Some("/tmp/x.jsonl".into());
        let checks = checks_for(&[rule], tmp.path());
        let row = named(&checks, "hooks.rule.0");
        assert_eq!(row.status, Status::Fail, "{}", row.message);
        assert_eq!(
            row.message,
            format!(
                "action=crawl.* -> {LOOPBACK} [refused, unsigned] (undelivered: 0) [DESTINATION REFUSED — names \
                 BOTH `http` and `file` — a rule needs exactly one destination; refused at load]"
            ),
            "the URL itself is fine; what is refused is naming two destinations"
        );
    }

    #[test]
    fn a_rule_with_no_destination_is_refused() {
        let tmp = tempfile::TempDir::new().unwrap();
        let checks = checks_for(&[hook_rule(Some("crawl.*"), None)], tmp.path());
        let row = named(&checks, "hooks.rule.0");
        assert_eq!(row.status, Status::Fail, "{}", row.message);
        assert_eq!(
            row.message,
            "action=crawl.* ->  [refused, unsigned] (undelivered: 0) [DESTINATION REFUSED — has no \
             destination — set exactly one of `http` or `file`; refused at load]",
            "there is no URL to refuse; what is missing is a destination"
        );
    }

    #[test]
    fn receiver_reasons_lead_the_hint_and_the_remedy_closes_it() {
        let tmp = tempfile::TempDir::new().unwrap();
        let rule = hook_rule(Some("crawl.*"), Some(LOOPBACK));
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
        assert_eq!(lines.last().copied(), Some(REMEDY));
    }

    #[test]
    fn a_stray_outbox_counts_undelivered_lines_from_its_own_cursor() {
        let tmp = tempfile::TempDir::new().unwrap();
        let stray = "127.0.0.1-9999-0123456789abcdef";
        std::fs::write(tmp.path().join(format!("{stray}.outbox.jsonl")), "{\"a\":1}\n{\"a\":2}\n{\"a\":3}\n").unwrap();
        std::fs::write(tmp.path().join(format!("{stray}.cursor")), "8").unwrap();
        let checks = checks_for(&[hook_rule(Some("crawl.*"), Some(LOOPBACK))], tmp.path());
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

    #[test]
    fn a_configured_rules_own_outbox_is_never_stray() {
        let tmp = tempfile::TempDir::new().unwrap();
        let rule = hook_rule(Some("crawl.*"), Some(LOOPBACK));
        std::fs::write(tmp.path().join(format!("{}.outbox.jsonl", key_of(&rule))), "{\"a\":1}\n").unwrap();
        let checks = checks_for(&[rule], tmp.path());
        assert!(checks.iter().all(|c| c.name != "hooks.stray"), "{checks:?}");
        assert!(named(&checks, "hooks.rule.0").message.contains("(undelivered: 1)"));
    }
}
