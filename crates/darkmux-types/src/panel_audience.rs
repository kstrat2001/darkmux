//! What a console panel shows a viewer that is not this machine (5.0).
//!
//! `darkmux serve`'s `GET /panel/:id` runs an allowlisted CLI verb and serves
//! its output to the viewer. A caller that is neither this machine nor holding
//! the serve token is never refused a panel for being remote: it is served the
//! panel REDACTED. Two layers, one rule:
//!
//! - **The verb renders its remote form.** The daemon sets [`AUDIENCE_ENV`] to
//!   [`REMOTE`] on the child, and a verb whose output carries this machine's
//!   execution surface shapes its own data before printing: `config list`
//!   withholds every `config.json` value [`config_verdict`] does not show,
//!   `doctor` withholds its fleet listener, identity and allow-list rows'
//!   detail, and `flow status` its directories and targets. A withheld value
//!   reads [`WITHHELD`]. Shaping happens on the data, before the verb wraps
//!   and colors it, so no fact can be split across a line break and slip
//!   through.
//! - **The daemon redacts every panel's text.** Roster addresses, tailnet
//!   names, IP literals and home directories (`darkmux-serve`'s shared
//!   redaction), plus every value in [`Withheld`]: the addresses, paths,
//!   endpoint URLs and credential pointers this machine's config, profile
//!   registry and environment name. stderr is never shown.
//!
//! Where anything was withheld the response carries ONE plain notice
//! ([`notice`]), the same sentence for every panel. Doctor still runs every
//! check; only what it prints is shaped. This machine, and a caller with the
//! serve token, read every panel unchanged.

use serde_json::Value;

/// Set by `darkmux serve` on a console-panel child, never by an operator.
pub const AUDIENCE_ENV: &str = "DARKMUX_PANEL_AUDIENCE";
/// [`AUDIENCE_ENV`]'s value for a viewer that is not this machine.
pub const REMOTE: &str = "remote";
/// What a withheld value reads in a remote viewer's output.
pub const WITHHELD: &str = "(shown on this machine only)";

/// Whether this process renders a console panel for a viewer that is not this
/// machine.
pub fn remote() -> bool {
    std::env::var(AUDIENCE_ENV).is_ok_and(|v| v == REMOTE)
}

/// The one notice a remote viewer reads where a panel withheld something.
/// `command` is the panel's command line (`darkmux doctor`); `machine` is
/// this machine's name, when it has one.
pub fn notice(command: &str, machine: Option<&str>) -> String {
    let on = machine.map(|m| format!("on {m} itself")).unwrap_or_else(|| "on that machine itself".to_string());
    format!("shown on this machine only: run `{command}` {on}, or over ssh, for the full output")
}

/// What kind of fact a withheld `config.json` value is. Every kind is withheld
/// from a remote viewer's `config list`; [`Kind::scrubbed`] says which are
/// also hidden wherever else they appear.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// A host, an IP or a bind address.
    Address,
    /// A filesystem path.
    Path,
    /// A URL or an origin.
    Url,
    /// Where a secret lives (a Keychain item, an environment variable's name)
    /// or a literal header value.
    Credential,
    /// A network identity (an allow-list entry's node id).
    Identity,
    /// Part of the fleet's execution surface that is not a distinctive string
    /// (a port, a busy policy, an allow-list entry's scope).
    ExecutionSurface,
    /// Operator-written free text (a jq transform, a note) that may hold any
    /// of the above.
    FreeText,
}

impl Kind {
    /// Whether a value of this kind is distinctive enough to hide wherever it
    /// appears in a panel's text. A port or a role name is not: hiding `8766`
    /// or `coder` everywhere would hide counts and ordinary words, so those
    /// are withheld where they are printed as what they are instead.
    pub fn scrubbed(self) -> bool {
        matches!(self, Kind::Address | Kind::Path | Kind::Url | Kind::Credential | Kind::Identity)
    }
}

/// `config.json` paths a remote viewer is not shown, each with why. A pattern
/// is dotted segments; `*` matches any key of a map, `[]` follows a list's
/// name. A pattern withholds everything under it.
pub const CONFIG_WITHHELD: &[(&str, Kind)] = &[
    ("lms_bin", Kind::Path),
    ("lmstudio_url", Kind::Url),
    ("dirs", Kind::Path),
    ("redis.host", Kind::Address),
    ("redis.port", Kind::ExecutionSurface),
    ("audit.dir", Kind::Path),
    ("runtime.daemon_cors_origins", Kind::Url),
    ("serve.bind", Kind::Address),
    ("serve.port", Kind::ExecutionSurface),
    ("fleet.identity.bin", Kind::Path),
    ("fleet.listener.port", Kind::ExecutionSurface),
    ("fleet.busy_policy", Kind::ExecutionSurface),
    // The whole entry: the machine name (the key) stays, its node id and the
    // scope it grants do not.
    ("fleet.accept_work.*", Kind::ExecutionSurface),
    ("fleet.accept_work.*.node_id", Kind::Identity),
    ("fleet.accept_work.*.repos", Kind::Url),
    ("hooks.outbox_dir", Kind::Path),
    ("hooks.rules[].http", Kind::Url),
    ("hooks.rules[].file", Kind::Path),
    ("hooks.rules[].headers", Kind::Credential),
    ("hooks.rules[].signing_secret_keychain_item", Kind::Credential),
    ("hooks.rules[].transform", Kind::FreeText),
];

/// `config.json` paths a remote viewer IS shown: settings that name no
/// address, path, URL, credential pointer or execution-surface fact. Anything
/// in neither list (a key a newer darkmux added, a hand-added key, `_comment`)
/// is withheld: the default is to withhold.
pub const CONFIG_SHOWN: &[&str] = &[
    "schema_version",
    "machine_id",
    "redis.enabled",
    "redis.db",
    "redis.stream",
    "redis.maxlen",
    "redis.telemetry_maxlen",
    "audit.enabled",
    "runtime.inactivity_timeout_seconds",
    "runtime.model_load_timeout_seconds",
    "runtime.step_command_timeout_seconds",
    "runtime.mission_wall_clock_timeout_seconds",
    "runtime.dispatch_free_concurrency",
    "runtime.local_dispatch_concurrency",
    "runtime.max_turns",
    "runtime.max_tokens",
    "runtime.max_tokens_per_call",
    "runtime.reasoning_checkpoint_interval_tokens",
    "runtime.generation_checkpoint_interval_tokens",
    "runtime.max_stall_recoveries",
    "runtime.strict_selection",
    "runtime.feedback_injection",
    "runtime.default_role",
    "runtime.check_updates",
    "runtime.injected_context_fraction",
    "runtime.acp_idle_exit_minutes",
    "runtime.turn_delay_ms",
    "runtime.host_sampler_interval_ms",
    "runtime.live_sample_ms",
    "runtime.thermal.enabled",
    "runtime.thermal.pause_at",
    "runtime.thermal.resume_at",
    "runtime.thermal.resume_hold_ms",
    "runtime.thermal.max_pause_ms",
    "runtime.thermal.min_cpu_speed_limit_pct",
    "runtime.thermal.speed_limit_hold_samples",
    "runtime.thermal.duty_delay_ms",
    "runtime.thermal.ratchet_factor",
    "runtime.thermal.episode_threshold",
    "runtime.thermal.tier4_enabled",
    "runtime.detection.degeneracy.policy",
    "runtime.liveness_retention_hours",
    "runtime.verbose",
    "fleet.mode",
    "fleet.identity.provider",
    "fleet.listener.enabled",
    "fleet.defaults.radio.answerer_profile",
    "power.min_battery_pct",
    "power.refuse_start_below_min",
    "power.pause_running_below_min",
    "mission.stale_active_days",
    "radio.answerer_profile",
    "radio.humor",
    "cmd.enabled",
    "cmd.allowed",
    "hooks.enabled",
    "hooks.max_outbox_mb",
    "hooks.jq_timeout_ms",
    "hooks.jq_max_output_bytes",
    "hooks.rules[].match.action",
    "hooks.rules[].match.session_id",
    "hooks.rules[].match.mission_id",
    "hooks.rules[].match.machine_id",
    "hooks.rules[].match.category",
    "hooks.rules[].match.level",
    "hooks.rules[].attribution_headers",
    "serve.token_keychain",
    "serve.read_auth",
    "role_profiles.*",
];

/// What a remote viewer is shown of one `config.json` path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    Shown,
    /// Withheld, as the most specific matching kind; `None` for a path
    /// neither list names (withheld by default).
    Withheld(Option<Kind>),
}

/// Whether `pattern` matches the start of `path` (both dotted segments).
fn prefix_match(pattern: &str, path: &[String]) -> bool {
    let pat: Vec<&str> = pattern.split('.').collect();
    pat.len() <= path.len() && pat.iter().zip(path).all(|(p, s)| *p == "*" || p == s)
}

/// What a remote viewer is shown of the `config.json` value at `path` (dotted
/// segments; a list item's segment is its list's name plus `[]`).
pub fn config_verdict(path: &[String]) -> Verdict {
    let withheld = CONFIG_WITHHELD
        .iter()
        .filter(|(p, _)| prefix_match(p, path))
        .max_by_key(|(p, _)| p.split('.').count())
        .map(|(_, k)| *k);
    if let Some(kind) = withheld {
        return Verdict::Withheld(Some(kind));
    }
    let shown = CONFIG_SHOWN.iter().any(|p| p.split('.').count() == path.len() && prefix_match(p, path));
    if shown {
        Verdict::Shown
    } else {
        Verdict::Withheld(None)
    }
}

/// Whether some [`CONFIG_WITHHELD`] pattern names `path` itself or a parent of
/// it: the whole value there is withheld, whatever is inside.
fn withheld_whole(path: &[String]) -> bool {
    CONFIG_WITHHELD.iter().any(|(p, _)| prefix_match(p, path))
}

/// `config.json` as a remote viewer reads it: every value [`config_verdict`]
/// does not show reads [`WITHHELD`]; keys, and the values it shows, stay.
/// Returns whether anything was withheld.
pub fn shape_config_json(root: &mut Value) -> bool {
    fn walk(v: &mut Value, path: &mut Vec<String>) -> bool {
        if !path.is_empty() && withheld_whole(path) {
            *v = Value::String(WITHHELD.to_string());
            return true;
        }
        match v {
            Value::Object(map) => {
                let mut any = false;
                for (k, child) in map.iter_mut() {
                    path.push(k.clone());
                    any |= walk(child, path);
                    path.pop();
                }
                any
            }
            Value::Array(items) => {
                let Some(last) = path.pop() else { return false };
                path.push(format!("{last}[]"));
                let mut any = false;
                for item in items.iter_mut() {
                    any |= walk_item(item, path);
                }
                path.pop();
                path.push(last);
                any
            }
            _ => match config_verdict(path) {
                Verdict::Shown => false,
                Verdict::Withheld(_) => {
                    *v = Value::String(WITHHELD.to_string());
                    true
                }
            },
        }
    }
    /// A list item: its path is the list's (`...[]`); a scalar item takes
    /// the list's own verdict.
    fn walk_item(v: &mut Value, path: &mut Vec<String>) -> bool {
        match v {
            Value::Object(_) => walk(v, path),
            _ => {
                let mut list = path.clone();
                if let Some(last) = list.last_mut() {
                    *last = last.trim_end_matches("[]").to_string();
                }
                if withheld_whole(path) || config_verdict(&list) != Verdict::Shown {
                    *v = Value::String(WITHHELD.to_string());
                    true
                } else {
                    false
                }
            }
        }
    }
    walk(root, &mut Vec::new())
}

/// Every string in `config.json` whose kind [`Kind::scrubbed`]: what a remote
/// viewer must not read anywhere a panel prints it.
pub fn config_scrub_values(root: &Value) -> Vec<String> {
    fn walk(v: &Value, path: &mut Vec<String>, out: &mut Vec<String>) {
        match v {
            Value::Object(map) => {
                for (k, child) in map {
                    path.push(k.clone());
                    walk(child, path, out);
                    path.pop();
                }
            }
            Value::Array(items) => {
                let Some(last) = path.pop() else { return };
                path.push(format!("{last}[]"));
                for item in items {
                    if item.is_object() {
                        walk(item, path, out);
                    } else if let Some(s) = item.as_str() {
                        let mut list = path.clone();
                        if let Some(l) = list.last_mut() {
                            *l = l.trim_end_matches("[]").to_string();
                        }
                        if matches!(config_verdict(&list), Verdict::Withheld(Some(k)) if k.scrubbed()) {
                            out.push(s.to_string());
                        }
                    }
                }
                path.pop();
                path.push(last);
            }
            Value::String(s) => {
                if matches!(config_verdict(path), Verdict::Withheld(Some(k)) if k.scrubbed()) {
                    out.push(s.clone());
                }
            }
            _ => {}
        }
    }
    let mut out = Vec::new();
    walk(root, &mut Vec::new(), &mut out);
    out
}

/// The values a remote viewer must not read anywhere a panel prints them:
/// this machine's addresses, paths, endpoint URLs and credential pointers.
/// [`Self::scrub`] replaces each whole occurrence with [`WITHHELD`].
#[derive(Debug, Clone, Default)]
pub struct Withheld {
    /// Longest first, so a path is replaced before a prefix of it.
    needles: Vec<String>,
}

impl Withheld {
    /// From raw values. A value too plain to be a host fact is dropped: one
    /// shorter than four characters, a loopback address or URL (which the
    /// shared redaction also leaves visible), and an address, path or URL
    /// with no `/`, `.` or `:` in it (`lms`, the default `lms_bin`, is a
    /// command name, not a path). A comma-separated list is split, and a URL
    /// also contributes its host and `host:port`, since a verb may print only
    /// those (`profile list` names an endpoint by its host).
    pub fn from_values<I: IntoIterator<Item = String>>(values: I) -> Self {
        let mut needles: Vec<String> = Vec::new();
        for v in values {
            for part in v.split(',') {
                let part = part.trim().trim_end_matches('/');
                if part.chars().count() < 4 || is_loopback(part) {
                    continue;
                }
                needles.push(part.to_string());
                if part.contains("://") {
                    if let Some(hostport) = crate::url_authority::UrlAuthority::parse(part).map(|a| a.hostport()) {
                        let host = host_of(hostport);
                        needles.extend([hostport, host].into_iter().filter(|h| h.chars().count() >= 4).map(str::to_string));
                    }
                }
            }
        }
        needles.sort_by_key(|n| std::cmp::Reverse(n.len()));
        needles.dedup();
        Self { needles }
    }

    /// Like [`Self::from_values`], keeping only values that look like an
    /// address, a path or a URL (they hold a `/`, `.` or `:`).
    pub fn from_locations<I: IntoIterator<Item = String>>(values: I) -> Self {
        Self::from_values(values.into_iter().filter(|v| v.contains(['/', '.', ':'])))
    }

    /// Both sets together.
    pub fn merged(mut self, other: Withheld) -> Self {
        self.needles.extend(other.needles);
        self.needles.sort_by_key(|n| std::cmp::Reverse(n.len()));
        self.needles.dedup();
        self
    }

    /// The values, longest first (for tests and diagnostics).
    pub fn values(&self) -> &[String] {
        &self.needles
    }

    /// `text` with every whole occurrence of a value replaced by [`WITHHELD`].
    /// An occurrence inside a longer word is not one (`/srv/x` in
    /// `/srv/xy`); a path's own children are (`/srv/x` in `/srv/x/y`).
    pub fn scrub(&self, text: &str) -> String {
        let mut out = text.to_string();
        for n in &self.needles {
            out = replace_whole(&out, n);
        }
        out
    }

    /// Whether `text` holds any value.
    pub fn hits(&self, text: &str) -> bool {
        self.needles.iter().any(|n| replace_whole(text, n) != text)
    }

    /// Every string inside `v` scrubbed, keys included.
    pub fn scrub_json(&self, v: &mut Value) {
        match v {
            Value::String(s) => *s = self.scrub(s),
            Value::Array(items) => items.iter_mut().for_each(|i| self.scrub_json(i)),
            Value::Object(map) => {
                let taken = std::mem::take(map);
                for (k, mut child) in taken {
                    self.scrub_json(&mut child);
                    map.insert(self.scrub(&k), child);
                }
            }
            _ => {}
        }
    }
}

/// A character that continues a word: a match touching one is part of a
/// longer token.
fn word_char(c: char) -> bool {
    c.is_alphanumeric() || c == '-' || c == '_'
}

fn replace_whole(text: &str, needle: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(at) = rest.find(needle) {
        let before = rest[..at].chars().next_back();
        let after = rest[at + needle.len()..].chars().next();
        let glued_before = before.is_some_and(word_char) && needle.starts_with(word_char);
        let glued_after = after.is_some_and(word_char) && needle.ends_with(word_char);
        out.push_str(&rest[..at]);
        if glued_before || glued_after {
            out.push_str(needle);
        } else {
            out.push_str(WITHHELD);
        }
        rest = &rest[at + needle.len()..];
    }
    out.push_str(rest);
    out
}

/// The host of `host:port`, `[v6]:port` or a bare host.
fn host_of(hostport: &str) -> &str {
    if let Some(rest) = hostport.strip_prefix('[') {
        return rest.split_once(']').map(|(h, _)| h).unwrap_or(rest);
    }
    hostport
        .rsplit_once(':')
        .filter(|(h, p)| !h.contains(':') && p.chars().all(|c| c.is_ascii_digit()))
        .map(|(h, _)| h)
        .unwrap_or(hostport)
}

/// A loopback address, `localhost`, or a URL whose host is one of them.
fn is_loopback(v: &str) -> bool {
    let hostport = crate::url_authority::UrlAuthority::parse(v).map(|a| a.hostport()).unwrap_or(v);
    let host = host_of(hostport);
    host.eq_ignore_ascii_case("localhost")
        || host.parse::<std::net::IpAddr>().is_ok_and(|ip| ip.is_loopback())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn path(p: &str) -> Vec<String> {
        p.split('.').map(str::to_string).collect()
    }

    /// Every leaf path `config.json`'s schema can hold, in this module's
    /// pattern notation (`*` for a map key, `[]` after a list's name).
    fn schema_leaves() -> Vec<String> {
        let schema = schemars::schema_for!(crate::config::DarkmuxConfig).to_value();
        fn resolve<'a>(root: &'a Value, mut n: &'a Value) -> &'a Value {
            while let Some(r) = n.get("$ref").and_then(Value::as_str) {
                n = root.pointer(r.trim_start_matches('#')).expect("a $ref the schema defines");
            }
            n
        }
        fn branches(n: &Value) -> Vec<&Value> {
            ["anyOf", "oneOf", "allOf"].iter().flat_map(|c| n.get(*c).and_then(Value::as_array).into_iter().flatten()).collect()
        }
        fn walk(root: &Value, n: &Value, path: String, out: &mut Vec<String>) {
            let n = resolve(root, n);
            let mut leaf = true;
            for b in branches(n) {
                let b = resolve(root, b);
                if b.get("type") == Some(&Value::String("null".into())) {
                    continue;
                }
                leaf = false;
                walk(root, b, path.clone(), out);
            }
            if let Some(props) = n.get("properties").and_then(Value::as_object) {
                leaf = false;
                for (k, child) in props {
                    let p = if path.is_empty() { k.clone() } else { format!("{path}.{k}") };
                    walk(root, child, p, out);
                }
            }
            if let Some(map) = n.get("additionalProperties").filter(|a| a.is_object()) {
                leaf = false;
                walk(root, map, format!("{path}.*"), out);
            }
            if let Some(items) = n.get("items").filter(|a| a.is_object()) {
                let item = resolve(root, items);
                if item.get("properties").is_some() || !branches(item).is_empty() {
                    leaf = false;
                    walk(root, item, format!("{path}[]"), out);
                }
            }
            if leaf && !path.is_empty() {
                out.push(path);
            }
        }
        let mut out = Vec::new();
        walk(&schema, &schema, String::new(), &mut out);
        out.sort();
        out.dedup();
        out
    }

    /// The guard that keeps the classification complete: every path
    /// `config.json` can hold is either shown or withheld ON PURPOSE. A field
    /// a new darkmux adds fails here until someone decides what a remote
    /// viewer may read of it (and until then it is withheld anyway).
    #[test]
    fn every_config_path_is_classified_for_a_remote_viewer() {
        let leaves = schema_leaves();
        assert!(leaves.len() > 60, "the walker found the schema: {leaves:?}");
        let unclassified: Vec<&String> = leaves
            .iter()
            .filter(|l| {
                let p = path(l);
                let named_withheld = CONFIG_WITHHELD.iter().any(|(w, _)| prefix_match(w, &p));
                let named_shown = CONFIG_SHOWN.iter().any(|s| s.split('.').count() == p.len() && prefix_match(s, &p));
                named_withheld == named_shown
            })
            .collect();
        assert!(unclassified.is_empty(), "classify each (shown, or withheld with a kind): {unclassified:?}");
        // And no entry names a path the schema does not have.
        for s in CONFIG_SHOWN {
            assert!(leaves.iter().any(|l| l == s), "CONFIG_SHOWN names `{s}`, which config.json does not have");
        }
        for (w, _) in CONFIG_WITHHELD {
            assert!(leaves.iter().any(|l| prefix_match(w, &path(l))), "CONFIG_WITHHELD names `{w}`, which config.json does not have");
        }
    }

    fn fixture() -> Value {
        serde_json::json!({
            "schema_version": "2.0",
            "machine_id": "studio",
            "lms_bin": "/opt/fake/bin/lms",
            "lmstudio_url": "http://127.0.0.1:1234",
            "redis": { "enabled": true, "host": "100.64.77.9", "port": 6379, "stream": "darkmux:flow" },
            "serve": { "bind": "127.0.0.1", "port": 8765, "read_auth": false },
            "fleet": {
                "mode": "hub",
                "listener": { "enabled": true, "port": 8766 },
                "busy_policy": "queue",
                "accept_work": { "laptop": { "node_id": "nFAKENODE", "roles": ["coder"], "repos": ["git@github.com:x/private.git"] } }
            },
            "hooks": { "rules": [ { "match": { "action": "run.complete" }, "http": "https://hooks.example.com/p",
                "headers": { "Authorization": "Bearer literal-secret" }, "signing_secret_keychain_item": "hook-sign" } ] },
            "cmd": { "allowed": ["status"] },
            "_comment": "my notes at /Users/someone",
            "future_key": "x"
        })
    }

    #[test]
    fn a_remote_viewer_reads_the_keys_and_the_plain_settings_only() {
        let mut v = fixture();
        assert!(shape_config_json(&mut v));
        let w = Value::String(WITHHELD.into());
        for p in ["/lms_bin", "/lmstudio_url", "/redis/host", "/redis/port", "/serve/bind", "/serve/port", "/fleet/listener/port", "/fleet/busy_policy", "/fleet/accept_work/laptop", "/hooks/rules/0/http", "/hooks/rules/0/headers", "/hooks/rules/0/signing_secret_keychain_item", "/_comment", "/future_key"] {
            assert_eq!(v.pointer(p), Some(&w), "{p} is withheld: {v:#}");
        }
        for (p, want) in [("/machine_id", "studio"), ("/redis/stream", "darkmux:flow"), ("/fleet/mode", "hub"), ("/hooks/rules/0/match/action", "run.complete"), ("/cmd/allowed/0", "status")] {
            assert_eq!(v.pointer(p).and_then(Value::as_str), Some(want), "{p} is shown: {v:#}");
        }
        assert_eq!(v.pointer("/fleet/listener/enabled"), Some(&Value::Bool(true)));
        assert_eq!(v.pointer("/serve/read_auth"), Some(&Value::Bool(false)));
        let mut plain = serde_json::json!({ "machine_id": "studio", "fleet": { "mode": "standalone" } });
        assert!(!shape_config_json(&mut plain), "nothing to withhold");
    }

    #[test]
    fn the_scrub_values_are_the_distinctive_ones() {
        let vals = config_scrub_values(&fixture());
        for want in ["/opt/fake/bin/lms", "100.64.77.9", "nFAKENODE", "git@github.com:x/private.git", "https://hooks.example.com/p", "Bearer literal-secret", "hook-sign"] {
            assert!(vals.iter().any(|v| v == want), "{want} in {vals:?}");
        }
        // A port, a busy policy and a role are withheld where printed as what
        // they are, not hidden everywhere.
        for not in ["queue", "coder", "8766"] {
            assert!(!vals.iter().any(|v| v == not), "{not} in {vals:?}");
        }
    }

    #[test]
    fn scrub_replaces_whole_values_only() {
        let w = Withheld::from_values(["/srv/x".to_string(), "100.64.77.9".into(), "lms".into(), "http://localhost:1234".into(), "127.0.0.1".into(), "a.example,b.example".into()]);
        assert_eq!(w.scrub("at /srv/x/y and /srv/xy"), format!("at {WITHHELD}/y and /srv/xy"));
        assert_eq!(w.scrub("redis 100.64.77.9:6379 not 100.64.77.90"), format!("redis {WITHHELD}:6379 not 100.64.77.90"));
        assert_eq!(w.scrub("lms ps; 127.0.0.1; http://localhost:1234"), "lms ps; 127.0.0.1; http://localhost:1234", "plain and loopback values stay");
        assert_eq!(w.scrub("b.example a.example"), format!("{WITHHELD} {WITHHELD}"));
        assert!(w.hits("x /srv/x") && !w.hits("x /srv/xz"));
        let url = Withheld::from_values(["https://myres.example.com:8443/v1".to_string()]);
        assert_eq!(url.scrub("azure @ myres.example.com"), format!("azure @ {WITHHELD}"), "a URL's host goes too");
        assert_eq!(url.scrub("at myres.example.com:8443"), format!("at {WITHHELD}"));
        let only_locations = Withheld::from_locations(["lms".to_string(), "/opt/lms".into(), "item-name".into()]);
        assert_eq!(only_locations.values(), ["/opt/lms"]);
    }

    #[test]
    fn the_notice_names_the_command_and_the_machine() {
        assert_eq!(
            notice("darkmux doctor", Some("studio")),
            "shown on this machine only: run `darkmux doctor` on studio itself, or over ssh, for the full output"
        );
        assert!(notice("darkmux doctor", None).contains("on that machine itself"));
    }
}
