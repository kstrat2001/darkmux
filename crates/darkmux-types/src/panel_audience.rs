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
//! - **The daemon redacts every panel's text,** stdout and stderr alike
//!   (stderr is redacted, never dropped, so a failed panel still says why).
//!   Roster addresses, tailnet names, IP literals and home directories
//!   (`darkmux-serve`'s shared redaction), plus every value in [`Withheld`]:
//!   the addresses, paths, endpoint URLs and credential pointers this machine
//!   is configured with (see `darkmux_serve::panel_withheld`).
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
/// name. A pattern withholds everything under it. Every leaf the schema has
/// is named here or in [`CONFIG_SHOWN`] by its own path, even under a prefix
/// that withholds it whole, so a new field is a decision about its kind (and
/// so whether it is hidden wherever else it prints), never an inheritance.
pub const CONFIG_WITHHELD: &[(&str, Kind)] = &[
    ("lms_bin", Kind::Path),
    ("lmstudio_url", Kind::Url),
    ("dirs", Kind::Path),
    ("dirs.audit", Kind::Path),
    ("dirs.findings", Kind::Path),
    ("dirs.fleet_file", Kind::Path),
    ("dirs.flows", Kind::Path),
    ("dirs.identity", Kind::Path),
    ("dirs.lab", Kind::Path),
    ("dirs.mods", Kind::Path),
    ("dirs.skills", Kind::Path),
    ("dirs.templates", Kind::Path),
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
    ("fleet.accept_work.*.images", Kind::ExecutionSurface),
    ("fleet.accept_work.*.profiles", Kind::ExecutionSurface),
    ("fleet.accept_work.*.roles", Kind::ExecutionSurface),
    ("fleet.accept_work.*.workspace", Kind::ExecutionSurface),
    ("hooks.outbox_dir", Kind::Path),
    ("hooks.rules[].http", Kind::Url),
    ("hooks.rules[].file", Kind::Path),
    ("hooks.rules[].headers", Kind::Credential),
    ("hooks.rules[].headers.*", Kind::Credential),
    ("hooks.rules[].headers.*.keychain_item", Kind::Credential),
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
/// segments; a list item's segment is its list's name plus `[]`), judged by
/// the key alone. [`shape_config_json`] also withholds a shown key whose
/// value has the wrong type.
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

/// The paths (as the user-file gate writes them: `runtime.max_turns`,
/// `hooks.rules[0].http`) of every value in `root` whose JSON type
/// `config.json`'s schema does not accept. What such a value holds is not
/// what its key means, so a key that is shown is not shown with it.
fn wrong_typed(root: &Value) -> std::collections::HashSet<String> {
    use crate::user_files::{config_retired, key_issues, Issue};
    key_issues::<crate::config::DarkmuxConfig>(root, &config_retired)
        .into_iter()
        .filter(|k| matches!(k.issue, Issue::WrongType { .. }))
        .map(|k| k.path)
        .collect()
}

/// Whether `v`, at `pattern`, is a string an enum-valued setting does not
/// register as one of its tokens (`config_enum::ENUM_SETTINGS`). That is bad
/// config, and doctor's refusal quotes it: what it holds is not what the key
/// means, so it is withheld like a value of the wrong type. A retired
/// spelling is not a token either.
fn bad_enum_token(pattern: &[String], v: &Value) -> bool {
    let Value::String(raw) = v else { return false };
    let key = pattern.join(".");
    crate::config_enum::ENUM_SETTINGS.iter().any(|s| s.key == key && s.canonical(raw).is_none())
}

/// An unknown key's NAME that looks like a location (it holds a `/`, `.` or
/// `:`): `config list` prints keys, and doctor names an unknown key by path.
fn location_like_key(k: &str) -> bool {
    k.contains(['/', '.', ':'])
}

/// One step down from a value: its pattern path (`hooks.rules[].http`, for the
/// tables) and its display path (`hooks.rules[0].http`, as the user-file gate
/// names it).
#[derive(Clone)]
struct At {
    pattern: Vec<String>,
    display: String,
}

impl At {
    fn root() -> Self {
        Self { pattern: Vec::new(), display: String::new() }
    }

    fn key(&self, k: &str) -> Self {
        let mut pattern = self.pattern.clone();
        pattern.push(k.to_string());
        Self { pattern, display: crate::user_files::join_display(&self.display, k) }
    }

    /// Item `i` of the list at `self`.
    fn item(&self, i: usize) -> Self {
        let mut pattern = self.pattern.clone();
        if let Some(last) = pattern.last_mut() {
            last.push_str("[]");
        }
        Self { pattern, display: format!("{}[{i}]", self.display) }
    }

    /// A scalar list item's verdict is its list's.
    fn verdict(&self) -> Verdict {
        let mut list = self.pattern.clone();
        if let Some(last) = list.last_mut() {
            *last = last.trim_end_matches("[]").to_string();
        }
        config_verdict(&list)
    }
}

/// `config.json` as a remote viewer reads it: every value [`config_verdict`]
/// does not show, and every value of the wrong type, reads [`WITHHELD`];
/// keys, and the values it shows, stay. Returns whether anything was
/// withheld.
pub fn shape_config_json(root: &mut Value) -> bool {
    type Wrong = std::collections::HashSet<String>;
    fn withhold(v: &mut Value) -> bool {
        *v = Value::String(WITHHELD.to_string());
        true
    }
    fn walk(v: &mut Value, at: &At, wrong: &Wrong) -> bool {
        if !at.pattern.is_empty() && (withheld_whole(&at.pattern) || wrong.contains(&at.display)) {
            return withhold(v);
        }
        match v {
            Value::Object(map) => map.iter_mut().fold(false, |any, (k, child)| walk(child, &at.key(k), wrong) | any),
            Value::Array(items) => items.iter_mut().enumerate().fold(false, |any, (i, item)| shape_item(item, &at.item(i), wrong) | any),
            _ => (config_verdict(&at.pattern) != Verdict::Shown || bad_enum_token(&at.pattern, v)) && withhold(v),
        }
    }
    /// One array item: an object is walked; anything else is shown only
    /// when its path is shown and its type is right.
    fn shape_item(item: &mut Value, at: &At, wrong: &Wrong) -> bool {
        if let Value::Object(_) = item {
            return walk(item, at, wrong);
        }
        let shown = !withheld_whole(&at.pattern) && !wrong.contains(&at.display) && at.verdict() == Verdict::Shown;
        !shown && withhold(item)
    }
    let wrong = wrong_typed(root);
    walk(root, &At::root(), &wrong)
}

/// Every string in `config.json` a remote viewer must not read anywhere a
/// panel prints it: a value whose kind [`Kind::scrubbed`], a value under a
/// key neither table names (an unknown or retired key, a note), a value of
/// the wrong type, location-like keys inside it included (doctor's user-file
/// row prints it), an unknown key whose name looks like a location, and a string an
/// enum-valued setting does not register (doctor's refusal quotes it).
pub fn config_scrub_values(root: &Value) -> Vec<String> {
    fn scrubbed(verdict: Verdict) -> bool {
        matches!(verdict, Verdict::Withheld(None)) || matches!(verdict, Verdict::Withheld(Some(k)) if k.scrubbed())
    }
    fn every_string(v: &Value, out: &mut Vec<String>) {
        match v {
            Value::String(s) => out.push(s.clone()),
            Value::Array(items) => items.iter().for_each(|i| every_string(i, out)),
            Value::Object(map) => map.iter().for_each(|(k, c)| {
                // A key here is free text too, but hiding a plain word
                // (`enabled`) everywhere would hide ordinary output: only a
                // key that looks like a location is taken.
                if location_like_key(k) {
                    out.push(k.clone());
                }
                every_string(c, out)
            }),
            _ => {}
        }
    }
    type Wrong = std::collections::HashSet<String>;
    fn walk(v: &Value, at: &At, wrong: &Wrong, out: &mut Vec<String>) {
        if !at.pattern.is_empty() && wrong.contains(&at.display) {
            return every_string(v, out);
        }
        match v {
            Value::Object(map) => map.iter().for_each(|(k, child)| walk_key(k, child, at, wrong, out)),
            Value::Array(items) => items.iter().enumerate().for_each(|(i, item)| walk_item(item, &at.item(i), wrong, out)),
            Value::String(s) if scrubbed(config_verdict(&at.pattern)) || bad_enum_token(&at.pattern, v) => out.push(s.clone()),
            _ => {}
        }
    }
    fn walk_key(k: &str, child: &Value, at: &At, wrong: &Wrong, out: &mut Vec<String>) {
        let at = at.key(k);
        if location_like_key(k) && config_verdict(&at.pattern) == Verdict::Withheld(None) {
            out.push(k.to_string());
        }
        walk(child, &at, wrong, out)
    }
    fn walk_item(item: &Value, at: &At, wrong: &Wrong, out: &mut Vec<String>) {
        match item {
            Value::Object(_) => walk(item, at, wrong, out),
            _ if wrong.contains(&at.display) || scrubbed(at.verdict()) => every_string(item, out),
            _ => {}
        }
    }
    let wrong = wrong_typed(root);
    let mut out = Vec::new();
    walk(root, &At::root(), &wrong, &mut out);
    out
}

/// The shortest prefix of a withheld value that, cut off by a `…`, is still
/// treated as that value (see [`Withheld::scrub`]).
const MIN_CUT_CHARS: usize = 4;
/// What a truncating renderer puts where it cut a value short.
const CUT: char = '…';

/// The values a remote viewer must not read anywhere a panel prints them:
/// this machine's addresses, paths, endpoint URLs and credential pointers.
/// [`Self::scrub`] replaces each whole occurrence with [`WITHHELD`], in any
/// ASCII case (a host is case-insensitive, and so is a default macOS path).
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
                if part.chars().count() < MIN_CUT_CHARS || is_loopback(part) {
                    continue;
                }
                needles.push(part.to_string());
                if part.contains("://") {
                    if let Some(hostport) = crate::url_authority::UrlAuthority::parse(part).map(|a| a.hostport()) {
                        let host = host_of(hostport);
                        needles.extend(
                            [hostport, host].into_iter().filter(|h| h.chars().count() >= MIN_CUT_CHARS).map(str::to_string),
                        );
                    }
                }
            }
        }
        Self { needles }.sorted()
    }

    /// Like [`Self::from_values`], keeping only values that look like an
    /// address, a path or a URL (they hold a `/`, `.` or `:`).
    pub fn from_locations<I: IntoIterator<Item = String>>(values: I) -> Self {
        Self::from_values(values.into_iter().filter(|v| v.contains(['/', '.', ':'])))
    }

    /// Both sets together.
    pub fn merged(mut self, other: Withheld) -> Self {
        self.needles.extend(other.needles);
        self.sorted()
    }

    /// Without any value spelled like one of `public`, in any case: a machine,
    /// endpoint, profile or fixture name every viewer reads. A credential
    /// pointer that happens to share a public name (an endpoint's Keychain
    /// item named after the endpoint) tells a viewer nothing the name does
    /// not, and hiding it would hide the name everywhere.
    pub fn sparing<I: IntoIterator<Item = S>, S: AsRef<str>>(mut self, public: I) -> Self {
        let public: Vec<S> = public.into_iter().collect();
        self.needles.retain(|n| !public.iter().any(|p| p.as_ref().eq_ignore_ascii_case(n)));
        self
    }

    fn sorted(mut self) -> Self {
        self.needles.sort_by(|a, b| b.len().cmp(&a.len()).then_with(|| a.cmp(b)));
        self.needles.dedup_by(|a, b| a.eq_ignore_ascii_case(b));
        self
    }

    /// The values, longest first (for tests and diagnostics).
    pub fn values(&self) -> &[String] {
        &self.needles
    }

    /// `text` with every whole occurrence of a value replaced by [`WITHHELD`].
    /// An occurrence inside a longer word is not one (`/srv/x` in
    /// `/srv/xy`); a path's own children are (`/srv/x` in `/srv/x/y`). A
    /// value a renderer cut short with `…` is one too, head and tail
    /// together, when at least [`MIN_CUT_CHARS`] of it show (`/srv/xy…` or
    /// `/sr…xyz` for `/srv/xyz`, see [`cut_spans`]): truncating a cell must
    /// not turn a withheld value into a readable part of it.
    pub fn scrub(&self, text: &str) -> String {
        let mut out = text.to_string();
        for n in &self.needles {
            out = replace_whole(&out, n);
        }
        if out.contains(CUT) {
            for n in &self.needles {
                out = replace_cut(&out, n);
            }
        }
        out
    }

    /// Whether `text` holds any value.
    pub fn hits(&self, text: &str) -> bool {
        self.scrub(text) != text
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

/// Whether `needle` found at `text[at..at + len]` is glued to a word on
/// either side (and so is part of a longer token, not the value).
fn glued(text: &str, needle: &str, at: usize, len: usize) -> bool {
    let before = text[..at].chars().next_back();
    let after = text[at + len..].chars().next();
    (before.is_some_and(word_char) && needle.starts_with(word_char)) || (after.is_some_and(word_char) && needle.ends_with(word_char))
}

/// `text` with each whole, ASCII-case-insensitive occurrence of `needle`
/// replaced by [`WITHHELD`]. Byte offsets are char boundaries: a match starts
/// where the needle's first byte matches, which is ASCII or a UTF-8 lead byte
/// (compared exactly), and spans the same complete characters.
fn replace_whole(text: &str, needle: &str) -> String {
    let (t, n) = (text.as_bytes(), needle.as_bytes());
    let mut out = String::with_capacity(text.len());
    let (mut last, mut i) = (0, 0);
    while !n.is_empty() && i + n.len() <= t.len() {
        if !t[i..i + n.len()].eq_ignore_ascii_case(n) {
            i += 1;
            continue;
        }
        if !glued(text, needle, i, n.len()) {
            out.push_str(&text[last..i]);
            out.push_str(WITHHELD);
            last = i + n.len();
        }
        i += n.len();
    }
    out.push_str(&text[last..]);
    out
}

/// The byte spans of `text` that are `needle` cut short by a truncating
/// renderer: a head of it, a `…`, then a tail of it (`head…tail`, as a cell
/// cut in the middle reads; `head…` when the cut is at the end), with at least
/// [`MIN_CUT_CHARS`] of it showing and neither end glued to a longer word.
/// Each span runs from the head's first byte to the tail's last. Shared by
/// [`Withheld::scrub`] and the daemon's address redaction.
pub fn cut_spans(text: &str, needle: &str) -> Vec<(usize, usize)> {
    let cut = NeedleCut::new(needle);
    let mut out = Vec::new();
    let mut last = 0;
    for (e, _) in text.match_indices(CUT) {
        if e < last {
            continue;
        }
        let (before, after) = (&text[last..e], &text[e + CUT.len_utf8()..]);
        if let Some((k, j)) = cut.best(before, after).filter(|(k, j)| k + j >= MIN_CUT_CHARS) {
            let start = e - cut.bounds[k];
            let end = e + CUT.len_utf8() + cut.tail_bytes(j);
            out.push((start, end));
            last = end;
        }
    }
    out
}

/// A needle as [`cut_spans`] matches it: the byte offset of every char
/// boundary, and its char count.
struct NeedleCut<'a> {
    needle: &'a str,
    bounds: Vec<usize>,
    chars: usize,
}

impl<'a> NeedleCut<'a> {
    fn new(needle: &'a str) -> Self {
        let bounds: Vec<usize> = needle.char_indices().map(|(i, _)| i).chain([needle.len()]).collect();
        let chars = bounds.len() - 1;
        Self { needle, bounds, chars }
    }

    /// The bytes of the needle's last `j` chars.
    fn tail_bytes(&self, j: usize) -> usize {
        self.needle.len() - self.bounds[self.chars - j]
    }

    /// Whether `before` ends with the needle's first `k` chars, not glued to
    /// a longer word.
    fn head_shows(&self, before: &str, k: usize) -> bool {
        let hb = self.bounds[k];
        hb <= before.len()
            && before.is_char_boundary(before.len() - hb)
            && before.as_bytes()[before.len() - hb..].eq_ignore_ascii_case(&self.needle.as_bytes()[..hb])
            && (k == 0 || !(before[..before.len() - hb].chars().next_back().is_some_and(word_char) && self.needle.starts_with(word_char)))
    }

    /// Whether `after` starts with the needle's last `j` chars, not glued to
    /// a longer word.
    fn tail_shows(&self, after: &str, j: usize) -> bool {
        let tb = self.tail_bytes(j);
        tb <= after.len()
            && after.is_char_boundary(tb)
            && after.as_bytes()[..tb].eq_ignore_ascii_case(&self.needle.as_bytes()[self.needle.len() - tb..])
            && (j == 0 || !(after[tb..].chars().next().is_some_and(word_char) && self.needle.ends_with(word_char)))
    }

    /// The cut around one `…` that shows the most of the needle, and never
    /// all of it: `(k, j)` chars of head and tail.
    fn best(&self, before: &str, after: &str) -> Option<(usize, usize)> {
        (0..self.chars)
            .filter(|&k| self.head_shows(before, k))
            .filter_map(|k| (0..self.chars - k).rev().find(|&j| self.tail_shows(after, j)).map(|j| (k, j)))
            .max_by_key(|(k, j)| (k + j, *k))
    }
}

/// `text` with each [`cut_spans`] occurrence of `needle` replaced by
/// [`WITHHELD`].
fn replace_cut(text: &str, needle: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut at = 0;
    for (start, end) in cut_spans(text, needle) {
        out.push_str(&text[at..start]);
        out.push_str(WITHHELD);
        at = end;
    }
    out.push_str(&text[at..]);
    out
}

/// The host of `host:port` or `[v6]:port`; `None` for anything else (a bare
/// host, a bare IPv6 address, an unclosed bracket). The one parser for both
/// this module and the daemon's address redaction.
pub fn address_host(addr: &str) -> Option<&str> {
    if let Some(rest) = addr.strip_prefix('[') {
        return rest.split_once(']').map(|(host, _)| host);
    }
    match addr.rsplit_once(':') {
        Some((host, port)) if !host.contains(':') && port.chars().all(|c| c.is_ascii_digit()) => Some(host),
        _ => None,
    }
}

/// The host of `host:port`, `[v6]:port` or a bare host. Malformed input
/// (an unclosed `[`) is returned whole, so it never reads as loopback.
pub(crate) fn host_of(hostport: &str) -> &str {
    address_host(hostport).unwrap_or(hostport)
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

    /// The one host parser: a port is split off a name or a bracketed IPv6;
    /// anything else has no host:port shape. A malformed address (an
    /// unclosed `[`) is never read as loopback, so it is withheld like any
    /// other value.
    #[test]
    fn address_host_splits_host_port_and_a_malformed_address_is_not_loopback() {
        assert_eq!(address_host("studio:8765"), Some("studio"));
        assert_eq!(address_host("[::1]:8765"), Some("::1"));
        assert_eq!(address_host("studio"), None);
        assert_eq!(address_host("::1"), None);
        assert_eq!(address_host("[::1"), None);
        assert!(is_loopback("[::1]:8765") && is_loopback("localhost:1234") && is_loopback("http://127.0.0.1:1234/v1"));
        assert!(!is_loopback("[::1"), "an unclosed bracket is not loopback");
    }

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

    /// The leaves the tables do not classify: a leaf must be named by its
    /// own path in exactly one table, so a field added under a prefix that is
    /// already withheld whole (`fleet.accept_work.*.x`) is a decision about
    /// its kind, not an inheritance. A shown leaf under a withheld prefix is
    /// a contradiction (it would be withheld at run time) and is reported
    /// too.
    fn unclassified(leaves: &[String]) -> Vec<&String> {
        leaves
            .iter()
            .filter(|l| {
                let exact_withheld = CONFIG_WITHHELD.iter().any(|(w, _)| w == l);
                let exact_shown = CONFIG_SHOWN.contains(&l.as_str());
                let under_withheld = CONFIG_WITHHELD.iter().any(|(w, _)| prefix_match(w, &path(l)));
                exact_withheld == exact_shown || (exact_shown && under_withheld)
            })
            .collect()
    }

    /// The guard that keeps the classification complete: every path
    /// `config.json` can hold is either shown or withheld ON PURPOSE. A field
    /// a new darkmux adds fails here until someone decides what a remote
    /// viewer may read of it (and until then it is withheld anyway).
    #[test]
    fn every_config_path_is_classified_for_a_remote_viewer() {
        let leaves = schema_leaves();
        assert!(leaves.len() > 60, "the walker found the schema: {leaves:?}");
        let unclassified = unclassified(&leaves);
        assert!(unclassified.is_empty(), "classify each (shown, or withheld with a kind): {unclassified:?}");
        // And no entry names a path the schema does not have.
        for s in CONFIG_SHOWN {
            assert!(leaves.iter().any(|l| l == s), "CONFIG_SHOWN names `{s}`, which config.json does not have");
        }
        for (w, _) in CONFIG_WITHHELD {
            assert!(leaves.iter().any(|l| prefix_match(w, &path(l))), "CONFIG_WITHHELD names `{w}`, which config.json does not have");
        }
    }

    /// (5.0 review) The guard catches a new field under a prefix that is
    /// already withheld whole: it is withheld at run time anyway, but nobody
    /// decided what KIND of fact it is, so whether it is also scrubbed
    /// wherever else it prints was never chosen.
    #[test]
    fn a_new_field_under_a_withheld_prefix_is_unclassified() {
        for new in ["fleet.accept_work.*.new_field", "dirs.new_dir", "hooks.rules[].headers.*.new_part"] {
            let leaves = vec![new.to_string()];
            assert_eq!(unclassified(&leaves), vec![&leaves[0]], "{new} must be classified by name");
        }
        // A shown leaf under a withheld prefix is a contradiction (it would be
        // withheld at run time), so it is reported too.
        let leaves = vec!["dirs.flows".to_string()];
        assert!(unclassified(&leaves).is_empty(), "an exactly named leaf is classified");
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

    /// (5.0 review item 3) A key a remote viewer is shown, holding a value
    /// of the wrong JSON type, is withheld: what it holds is not what the key
    /// means. A list where a string belongs is withheld whole.
    #[test]
    fn a_shown_key_holding_the_wrong_type_is_withheld() {
        let mut v = serde_json::json!({
            "machine_id": ["/opt/fake-list-path"],
            "runtime": { "max_turns": "/opt/fake-wrongtype-path", "max_tokens": 5 },
            "radio": { "humor": { "x": "/opt/fake-nested" } }
        });
        assert!(shape_config_json(&mut v));
        let w = Value::String(WITHHELD.into());
        for p in ["/machine_id", "/runtime/max_turns", "/radio/humor"] {
            assert_eq!(v.pointer(p), Some(&w), "{p}: {v:#}");
        }
        assert_eq!(v.pointer("/runtime/max_tokens"), Some(&Value::from(5)), "a value of the right type stays");
    }

    /// (5.0 review item 3) Wherever a panel prints a wrong-typed value or an
    /// unknown key's value (doctor's user-file row names it), it is scrubbed:
    /// both are in the scrub set.
    #[test]
    fn wrong_typed_and_unknown_values_are_scrubbed_everywhere() {
        let doc = serde_json::json!({
            "runtime": { "max_turns": "/opt/fake-wrongtype-path" },
            "future_key": "/opt/fake-unknown-value",
            "fleet": { "mode": "standalone" }
        });
        let vals = config_scrub_values(&doc);
        for want in ["/opt/fake-wrongtype-path", "/opt/fake-unknown-value"] {
            assert!(vals.iter().any(|v| v == want), "{want} in {vals:?}");
        }
        assert!(!vals.iter().any(|v| v == "standalone"), "a shown value is not scrubbed: {vals:?}");
    }

    /// (5.0 security re-review N3) Three more places a value doctor or
    /// `config list` prints could name a location: a KEY inside a value of the
    /// wrong type, an unknown key whose NAME is a path or a host, and a shown
    /// enum-valued key holding a value that is not one of its tokens (bad
    /// config, which doctor's refusal quotes). Each is scrubbed everywhere,
    /// and the enum value is withheld in `config list`; a valid token stays.
    #[test]
    fn wrong_typed_keys_path_named_keys_and_bad_enum_tokens_are_withheld() {
        let doc = serde_json::json!({
            "radio": { "humor": { "/opt/fake-key-in-wrong-type": 1, "enabled": 2 } },
            "/opt/fake-unknown-key-path": true,
            "runtime": { "gpubox.corp-fake.example": 1, "max_turns": 5 },
            "fleet": { "mode": "/opt/fake-enum-garbage", "busy_policy": "queue" },
            "hooks": { "rules": [ { "match": { "level": "/opt/fake-level-garbage", "action": "run.complete" } } ] }
        });
        let vals = config_scrub_values(&doc);
        for want in ["/opt/fake-key-in-wrong-type", "/opt/fake-unknown-key-path", "gpubox.corp-fake.example", "/opt/fake-enum-garbage", "/opt/fake-level-garbage"] {
            assert!(vals.iter().any(|v| v == want), "{want} in {vals:?}");
        }
        assert!(!vals.iter().any(|v| v == "run.complete" || v == "max_turns" || v == "enabled"), "plain words stay: {vals:?}");
        let mut v = doc.clone();
        assert!(shape_config_json(&mut v));
        let w = Value::String(WITHHELD.into());
        assert_eq!(v.pointer("/fleet/mode"), Some(&w), "{v:#}");
        assert_eq!(v.pointer("/hooks/rules/0/match/level"), Some(&w), "{v:#}");
        let mut good = serde_json::json!({ "fleet": { "mode": "hub" }, "hooks": { "rules": [ { "match": { "level": "error" } } ] } });
        shape_config_json(&mut good);
        assert_eq!(good.pointer("/fleet/mode").and_then(Value::as_str), Some("hub"), "a registered token stays");
        assert_eq!(good.pointer("/hooks/rules/0/match/level").and_then(Value::as_str), Some("error"), "a registered token stays");
    }

    /// (5.0 review item 6) Hosts are case-insensitive, so the scrub is: a
    /// host written in another case is the same host.
    #[test]
    fn scrub_ignores_case() {
        let w = Withheld::from_values(["myres.Example.com".to_string(), "/Opt/Fake/lms".into()]);
        assert_eq!(w.scrub("at MYRES.example.COM:443"), format!("at {WITHHELD}:443"));
        assert_eq!(w.scrub("bin /opt/fake/LMS"), format!("bin {WITHHELD}"));
        assert!(w.hits("x MyRes.Example.Com"));
        assert_eq!(w.scrub("myres.example.community"), "myres.example.community", "still whole tokens only");
    }

    /// (5.0 review item 8) A value cut short by a truncating cell (`…`) is
    /// still withheld: the visible part is a prefix of the value.
    #[test]
    fn a_value_truncated_with_an_ellipsis_is_still_withheld() {
        let w = Withheld::from_values(["/opt/fake-wrongtype-path/deep/inside".to_string(), "hosted-secret.example.com".into()]);
        assert_eq!(w.scrub("got \"/opt/fake-wrongtype-pa…"), format!("got \"{WITHHELD}"));
        assert_eq!(w.scrub("at hosted-sec…  next"), format!("at {WITHHELD}  next"));
        assert_eq!(w.scrub("nothing to cut… here"), "nothing to cut… here");
        assert_eq!(w.scrub("whole /opt/fake-wrongtype-path/deep/inside"), format!("whole {WITHHELD}"));
    }

    /// The middle cut `run list` and `mission status` make: head, `…`, tail,
    /// `max` characters in all.
    fn middle_cut(s: &str, max: usize) -> String {
        let chars: Vec<char> = s.chars().collect();
        let keep = max - 1;
        let (head, tail) = (keep.div_ceil(2), keep - keep.div_ceil(2));
        format!("{}…{}", chars[..head].iter().collect::<String>(), chars[chars.len() - tail..].iter().collect::<String>())
    }

    /// (5.0 security re-review C5) A value cut in the MIDDLE (`head…tail`, as
    /// `run list` and `mission status` truncate a cell) is withheld whole: the
    /// tail is as much the value as the head. At every width that shows at
    /// least [`MIN_CUT_CHARS`] of it, inside a line or alone in a cell.
    #[test]
    fn a_value_cut_in_the_middle_is_withheld_with_its_tail() {
        let needles = ["http://gpubox.corp-fake.example:1234", "/opt/fake-wrongtype-path/deep/inside"];
        let w = Withheld::from_values(needles.iter().map(|n| n.to_string()));
        for n in needles {
            for max in MIN_CUT_CHARS + 1..n.chars().count() {
                let cut = middle_cut(n, max);
                assert_eq!(w.scrub(&format!("at {cut}  next")), format!("at {WITHHELD}  next"), "{cut}");
                assert_eq!(w.scrub(&cut), WITHHELD, "{cut}");
            }
        }
        // A `…` with nothing of a value on either side, and a value's head
        // followed by text that is not its tail, keep what is not the value.
        assert_eq!(w.scrub("nothing to cut… here"), "nothing to cut… here");
        assert_eq!(w.scrub("http://gpub… unrelated"), format!("{WITHHELD} unrelated"));
        // A tail glued to a longer word is not the value's end.
        assert_eq!(w.scrub("x …h/inside2"), "x …h/inside2");
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
