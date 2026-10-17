//! (#316 stage 4, `docs/architecture/ENGINES.md` section 9.2) Which engine
//! `darkmux init` records as this machine's `default_engine`.
//!
//! `init` asks which engines are answering, not which OS it runs on: LM Studio
//! when `lms ps` answers, and an OpenAI-compatible server when `GET /v1/models`
//! answers at an endpoint `profiles.json` already names on this machine, at
//! `--engine-url`, or at a well-known local default. One answering is
//! suggested; several on a terminal are asked about (LM Studio first); none
//! writes nothing and names every address probed.
//!
//! The choice is recorded once. A config `init` creates gets it, and so does
//! an explicit `--engine-url`; a re-run against an existing config keeps the
//! engine it names, so an engine that is down one morning never moves the
//! machine to another one.

use anyhow::Result;
use darkmux_profiles::engine::{self, Engine};
use darkmux_types::default_engine::{self as default_engine_setting, LMSTUDIO};
use darkmux_types::endpoint::{Dialect, EndpointKind, url_names_this_machine};
use std::io::IsTerminal;
use std::path::Path;

/// `mlx_lm.server`'s default address (port 8080, confirmed on mlx-lm 0.32.1).
pub const MLX_DEFAULT_URL: &str = "http://127.0.0.1:8080/v1";

/// The dialect an endpoint `init` adds speaks: `max_tokens`, the cap field
/// `mlx_lm.server` reads (the Omarchy guide's `mlx-local` endpoint, too).
const ADDED_DIALECT: &str = "chat-completions-max-tokens";

/// How many listed model ids a message names before saying "and N more".
const NAMED_MODELS: usize = 8;

/// An engine that answered.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Found {
    Lmstudio,
    /// An OpenAI-compatible server at `url` (its `/v1` root), the endpoint
    /// `id` when `profiles.json` already names it, and the ids it lists.
    Server { id: Option<String>, url: String, models: Vec<String> },
}

impl Found {
    fn describe(&self) -> String {
        match self {
            Found::Lmstudio => "LM Studio (`lms ps` answers)".to_string(),
            Found::Server { id, url, models } => {
                let named = id.as_deref().map(|i| format!("endpoint `{i}`, ")).unwrap_or_default();
                format!("{named}{url} (lists {})", count_models(models.len()))
            }
        }
    }
}

fn count_models(n: usize) -> String {
    if n == 1 { "1 model".to_string() } else { format!("{n} models") }
}

/// The ids in `models`, the first [`NAMED_MODELS`] of them by name.
pub fn name_models(models: &[String]) -> String {
    let shown: Vec<String> = models.iter().take(NAMED_MODELS).map(|m| format!("`{m}`")).collect();
    let more = models.len().saturating_sub(NAMED_MODELS);
    if more == 0 { shown.join(", ") } else { format!("{} and {more} more", shown.join(", ")) }
}

/// How the recorded engine was picked.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum How {
    /// It was the only one answering (or the one `--engine-url` named).
    Only,
    /// The operator chose it on the terminal.
    Asked,
    /// Several answered and no choice was made: LM Studio, listed first.
    Default,
}

/// What `init` decided about the engine, and what it wrote.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// The engine was already chosen (`from` config.json or the
    /// `DARKMUX_DEFAULT_ENGINE` variable); `init` kept it.
    Kept { engine: String, from: &'static str },
    /// `found` is this machine's engine. `endpoint` is the id `default_engine`
    /// names (added to `profiles.json` when `added`); `others` also answered.
    Chosen { found: Found, endpoint: Option<String>, added: bool, others: Vec<Found>, how: How },
    /// Nothing answered at any of `probed`.
    NoneAnswering { probed: Vec<String> },
    /// `--engine-url` was given and did not answer.
    NotAnswering { url: String, error: String },
    /// Several answered, none of them LM Studio, and there was no terminal to
    /// ask on (or the answer named none of them).
    Undecided { answering: Vec<Found> },
}

impl Outcome {
    /// `init`'s report lines for the engine. `written` is false on a dry run,
    /// or when recording failed: the lines then say what would be written.
    pub fn lines(&self, written: bool) -> Vec<String> {
        match self {
            Outcome::Kept { engine, from } => vec![kept_line(engine, from)],
            Outcome::Chosen { found, endpoint, added, others, how } => {
                let mut lines = vec![chosen_line(found, endpoint.as_deref(), *added, *how, written)];
                if !others.is_empty() {
                    lines.push(format!(
                        "  also answering: {}. `darkmux init --engine-url <url>` records one of these instead.",
                        others.iter().map(Found::describe).collect::<Vec<_>>().join("; ")
                    ));
                }
                lines
            }
            Outcome::NoneAnswering { probed } => none_answering_lines(probed),
            Outcome::NotAnswering { url, error } => vec![format!(
                "default engine: not changed. Nothing answered `GET /models` at {url} ({error}). Start the server, \
                 then re-run `darkmux init --engine-url {url}`."
            )],
            Outcome::Undecided { answering } => vec![format!(
                "default engine: not set. More than one engine answers and there was no choice to record: {}. \
                 {}Re-run `darkmux init --engine-url <url>` with the one this machine should use.",
                answering.iter().map(Found::describe).collect::<Vec<_>>().join("; "),
                if written { "" } else { "A run on a terminal asks which. " }
            )],
        }
    }

    /// The models the chosen OpenAI-compatible server lists, and the name to
    /// give it in messages; `None` when the engine is LM Studio or unknown.
    pub fn server(&self) -> Option<(String, &[String])> {
        match self {
            Outcome::Chosen { found: Found::Server { id, url, models }, endpoint, .. } => {
                Some((endpoint.clone().or_else(|| id.clone()).unwrap_or_else(|| url.clone()), models))
            }
            _ => None,
        }
    }
}

fn chosen_line(found: &Found, endpoint: Option<&str>, added: bool, how: How, written: bool) -> String {
    let how = match how {
        How::Only => "the one answering",
        How::Asked => "chosen",
        How::Default => "listed first; no choice was made",
    };
    let (set, add) = if written { ("set", "added") } else { ("would set", "would be added") };
    match (found, endpoint) {
        (Found::Server { url, .. }, Some(id)) => {
            let added = if added { format!(", endpoint `{id}` {add} to profiles.json") } else { String::new() };
            format!(
                "default engine: `{id}` at {url} ({how}; {set} `default_engine` in config.json{added}). It loads \
                 whichever model a request names and does not report what it holds."
            )
        }
        _ => format!("default engine: LM Studio ({how})."),
    }
}

fn kept_line(engine: &str, from: &str) -> String {
    let back = if engine.trim().eq_ignore_ascii_case(LMSTUDIO) {
        String::new()
    } else {
        format!("; `darkmux config set {} {LMSTUDIO}` goes back to LM Studio", default_engine_setting::KEY)
    };
    format!("default engine: `{engine}` (from {from}; `darkmux init --engine-url <url>` records a server{back})")
}

fn none_answering_lines(probed: &[String]) -> Vec<String> {
    vec![
        format!("default engine: none answering, nothing recorded. Probed: {}.", probed.join(", ")),
        "  the config keeps `lmstudio`: start LM Studio (open the app, or `lms server start`) and it is used. \
         For another engine, start it and record it with `darkmux init --engine-url <url>` (`mlx_lm.server \
         --model <model>` serves on http://127.0.0.1:8080)."
            .to_string(),
    ]
}

/// Asks the engines whether they answer. A trait so tests decide.
pub trait Probe {
    fn lmstudio_answers(&self) -> bool;
    /// The ids `GET {url}/models` lists.
    fn served(&self, url: &str) -> Result<Vec<String>>;
    /// The addresses probed beyond the registry and `--engine-url`.
    fn well_known(&self) -> Vec<String> {
        vec![MLX_DEFAULT_URL.to_string()]
    }
}

/// The live probe: `lms ps` through the LM Studio engine, `GET /v1/models`
/// through the OpenAI-compatible one.
pub struct LiveProbe;

impl Probe for LiveProbe {
    fn lmstudio_answers(&self) -> bool {
        engine::lmstudio().loaded().is_ok()
    }

    fn served(&self, url: &str) -> Result<Vec<String>> {
        if let Some(servers) = test_servers() {
            anyhow::ensure!(servers.iter().any(|s| same_url(s, url)), "not a test server: {url}");
        }
        let catalog = engine::OpenAiCompatible::new(url, url).catalog()?;
        Ok(catalog.into_iter().map(|m| m.model_key).collect())
    }

    fn well_known(&self) -> Vec<String> {
        if let Some(v) = test_servers() {
            return v;
        }
        vec![MLX_DEFAULT_URL.to_string()]
    }
}

/// The TEST hook `DARKMUX_TEST_INIT_SERVERS`: when set (even empty), the
/// comma-separated servers `LiveProbe` may reach. They are its well-known
/// addresses, and any other URL (a registry endpoint on 8080, say) does not
/// answer, so no test, in process or driving the binary, reaches a server
/// this machine happens to run. Not an operator knob, like the other
/// `DARKMUX_TEST_*` hooks.
pub const TEST_SERVERS_HOOK: &str = "DARKMUX_TEST_INIT_SERVERS";

fn test_servers() -> Option<Vec<String>> {
    let v = std::env::var(TEST_SERVERS_HOOK).ok()?;
    Some(v.split(',').map(str::trim).filter(|s| !s.is_empty()).map(str::to_string).collect())
}

/// Picks one of several answering engines (LM Studio first, when it is one).
pub trait Ask {
    /// The index chosen, or `None` when there is no one to ask or the answer
    /// named none of them.
    fn choose(&mut self, options: &[Found]) -> Option<usize>;
}

/// Asks on the terminal; with none, asks nobody.
pub struct TerminalAsk;

impl Ask for TerminalAsk {
    fn choose(&mut self, options: &[Found]) -> Option<usize> {
        if !(std::io::stdin().is_terminal() && std::io::stderr().is_terminal()) {
            return None;
        }
        eprintln!("darkmux init: more than one engine answers on this machine:");
        for (i, f) in options.iter().enumerate() {
            eprintln!("  {}) {}", i + 1, f.describe());
        }
        eprint!("Which is this machine's engine? [1]: ");
        let mut line = String::new();
        std::io::stdin().read_line(&mut line).ok()?;
        parse_choice(&line, options.len())
    }
}

/// An answer to [`TerminalAsk`]: empty is the first option, a number in range
/// is that option, anything else is none.
pub fn parse_choice(line: &str, n: usize) -> Option<usize> {
    let line = line.trim();
    if line.is_empty() {
        return Some(0);
    }
    line.parse::<usize>().ok().filter(|i| (1..=n).contains(i)).map(|i| i - 1)
}

/// The `/v1` root an `--engine-url` names: as given, or with `/v1` added when
/// it names only a host.
pub fn engine_url(given: &str) -> std::result::Result<String, String> {
    let given = given.trim().trim_end_matches('/');
    let Some((scheme, rest)) = given.split_once("://") else {
        return Err(format!("`{given}` is not a URL; give one like {MLX_DEFAULT_URL}"));
    };
    if !matches!(scheme.to_ascii_lowercase().as_str(), "http" | "https") || rest.is_empty() {
        return Err(format!("`{given}` is not an http(s) URL; give one like {MLX_DEFAULT_URL}"));
    }
    Ok(if rest.contains('/') { given.to_string() } else { format!("{given}/v1") })
}

fn same_url(a: &str, b: &str) -> bool {
    a.trim().trim_end_matches('/').eq_ignore_ascii_case(b.trim().trim_end_matches('/'))
}

/// The registry's OpenAI-compatible endpoints on this machine, `(id, url)`:
/// unmanaged, not the Messages dialect (a hosted API), and a URL that names
/// this machine. A remote server is never this machine's engine.
pub fn local_endpoints(registry_json: &str) -> Vec<(String, String)> {
    let Ok(mut reg) = darkmux_profiles::profiles::parse_registry_lenient(registry_json) else {
        return Vec::new();
    };
    reg.materialize_endpoints();
    reg.endpoints
        .iter()
        .filter_map(|(id, ep)| {
            let url = ep.url.as_deref()?;
            let local = matches!(ep.kind(), Ok(EndpointKind::Unmanaged))
                && ep.resolved_dialect().is_ok_and(|d| d != Dialect::Messages)
                && url_names_this_machine(url);
            local.then(|| (id.clone(), url.to_string()))
        })
        .collect()
}

/// Every engine answering, in the order suggested (LM Studio first), and every
/// address probed. With `explicit`, only that URL is probed.
pub fn detect(
    probe: &dyn Probe,
    registry_json: &str,
    explicit: Option<&str>,
) -> (Vec<Found>, Vec<String>, Option<String>) {
    let known = local_endpoints(registry_json);
    let mut urls: Vec<String> = match explicit {
        Some(u) => vec![u.to_string()],
        None => known.iter().map(|(_, u)| u.clone()).chain(probe.well_known()).collect(),
    };
    let mut seen: Vec<String> = Vec::new();
    urls.retain(|u| {
        let fresh = !seen.iter().any(|s| same_url(s, u));
        seen.push(u.clone());
        fresh
    });
    let mut answering = Vec::new();
    let mut probed = Vec::new();
    let mut last_error = None;
    if explicit.is_none() {
        probed.push(format!("LM Studio (`{} ps`)", darkmux_types::config_access::lms_bin()));
        if probe.lmstudio_answers() {
            answering.push(Found::Lmstudio);
        }
    }
    for url in urls {
        probed.push(url.clone());
        match probe.served(&url) {
            Ok(models) => {
                let id = known.iter().find(|(_, u)| same_url(u, &url)).map(|(i, _)| i.clone());
                answering.push(Found::Server { id, url, models });
            }
            Err(e) => last_error = Some(format!("{e:#}")),
        }
    }
    (answering, probed, last_error)
}

/// Which answering engine to record. `ask` is consulted only when several
/// answer and none was named explicitly.
pub fn decide(
    answering: Vec<Found>,
    probed: Vec<String>,
    explicit: Option<&str>,
    last_error: Option<String>,
    ask: Option<&mut dyn Ask>,
) -> Outcome {
    if let Some(url) = explicit {
        if answering.is_empty() {
            return Outcome::NotAnswering { url: url.to_string(), error: last_error.unwrap_or_default() };
        }
        return chosen(answering, 0, How::Only);
    }
    match answering.len() {
        0 => Outcome::NoneAnswering { probed },
        1 => chosen(answering, 0, How::Only),
        _ => pick_among(answering, ask),
    }
}

/// Several answering: the operator's pick, else LM Studio (listed first),
/// else nothing.
fn pick_among(answering: Vec<Found>, ask: Option<&mut dyn Ask>) -> Outcome {
    match ask.and_then(|a| a.choose(&answering)) {
        Some(i) if i < answering.len() => chosen(answering, i, How::Asked),
        _ if answering[0] == Found::Lmstudio => chosen(answering, 0, How::Default),
        _ => Outcome::Undecided { answering },
    }
}

fn chosen(mut all: Vec<Found>, i: usize, how: How) -> Outcome {
    let found = all.remove(i);
    Outcome::Chosen { found, endpoint: None, added: false, others: all, how }
}

/// The endpoint id `init` gives a server it adds: `local-<port>`, or
/// `local-engine` without a port, with `-2`, `-3`… when the id is taken.
pub fn new_endpoint_id(url: &str, taken: &[String]) -> String {
    let authority = url.split_once("://").map_or(url, |(_, r)| r).split('/').next().unwrap_or_default();
    let port = authority.rsplit_once(':').map(|(_, p)| p).filter(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()));
    let base = port.map_or_else(|| "local-engine".to_string(), |p| format!("local-{p}"));
    let mut id = base.clone();
    let mut n = 2;
    while taken.iter().any(|t| t == &id) {
        id = format!("{base}-{n}");
        n += 1;
    }
    id
}

/// `registry_json` with `endpoints.<id>` added as `{ "url", "dialect" }`.
/// Text-level, like every other `init` edit of the operator's file, and
/// checked by parsing the result: an edit that would not read back as that
/// endpoint is refused rather than written.
pub fn add_endpoint(registry_json: &str, id: &str, url: &str) -> std::result::Result<String, String> {
    let q = |s: &str| serde_json::to_string(s).expect("a string serializes");
    let entry = format!("{}: {{ \"url\": {}, \"dialect\": \"{ADDED_DIALECT}\" }}", q(id), q(url));
    let out = match endpoints_open_brace(registry_json) {
        Some(open) => {
            let empty = registry_json[open + 1..].trim_start().starts_with('}');
            let sep = if empty { "\n  " } else { "," };
            format!("{}\n    {entry}{sep}{}", &registry_json[..=open], &registry_json[open + 1..])
        }
        None => {
            let root = registry_json.find('{').ok_or("the registry is not a JSON object")?;
            format!(
                "{}\n  \"endpoints\": {{\n    {entry}\n  }},{}",
                &registry_json[..=root],
                &registry_json[root + 1..]
            )
        }
    };
    let back: serde_json::Value =
        serde_json::from_str(&out).map_err(|e| format!("adding endpoint `{id}` would not parse ({e})"))?;
    if back.pointer(&format!("/endpoints/{id}/url")).and_then(|v| v.as_str()) != Some(url) {
        return Err(format!("adding endpoint `{id}` did not read back as written"));
    }
    Ok(out)
}

/// The byte offset of the `{` that opens the registry's `"endpoints"` map.
/// The key appears only at the top level (a model names its endpoint as
/// `"endpoint"`), so the first `"endpoints"` followed by `:` and `{` is it.
fn endpoints_open_brace(text: &str) -> Option<usize> {
    let key = "\"endpoints\"";
    let mut from = 0;
    while let Some(i) = text[from..].find(key) {
        let after = from + i + key.len();
        let rest = text[after..].trim_start();
        if let Some(value) = rest.strip_prefix(':') {
            let value_trim = value.trim_start();
            if value_trim.starts_with('{') {
                return Some(text.len() - value_trim.len());
            }
        }
        from = after;
    }
    None
}

/// Where [`configured`] read the engine from when the variable set it.
pub const FROM_ENV: &str = default_engine_setting::ENV;

/// The engine already chosen, and where from: `DARKMUX_DEFAULT_ENGINE`, else
/// config.json's `default_engine`, else `lmstudio`. Read from the file, since
/// `init` may run before any config tier is loaded.
pub fn configured(config_path: &Path) -> (String, &'static str) {
    if let Ok(v) = std::env::var(default_engine_setting::ENV) {
        if !v.trim().is_empty() {
            return (v, FROM_ENV);
        }
    }
    let engine = std::fs::read_to_string(config_path)
        .ok()
        .and_then(|t| serde_json::from_str::<serde_json::Value>(&t).ok())
        .and_then(|v| v.get(default_engine_setting::KEY)?.as_str().map(str::to_string))
        .unwrap_or_else(|| LMSTUDIO.to_string());
    (engine, "config.json")
}

/// (#316) Whether `init` may detect and record an engine without being told
/// to: nothing chose one yet. The variable has not; the config names LM
/// Studio (its default); and the registry still holds the worker placeholder,
/// so no model was ever planned on any engine. A tuned registry, or an
/// engine already recorded, is never moved by an engine that happens to be
/// down one morning.
pub fn may_detect(registry_json: &str, engine: &str, from: &str) -> bool {
    from != FROM_ENV
        && engine.trim().eq_ignore_ascii_case(LMSTUDIO)
        && crate::init::has_placeholder_slot(registry_json)
}

/// Why a chosen server cannot be recorded, checked before anything is
/// written: the variable overrides config.json, `DARKMUX_PROFILES` points
/// dispatch at another registry, or the setting would not resolve against
/// the registry `init` would write.
pub fn record_blocker(new_registry: &str, id: &str, registry_path: &Path) -> Option<String> {
    if let Ok(v) = std::env::var(default_engine_setting::ENV).map(|v| v.trim().to_string()) {
        if !v.is_empty() {
            return Some(format!(
                "{FROM_ENV}=`{v}` is set and overrides config.json; unset it, then re-run `darkmux init`."
            ));
        }
    }
    if let Ok(p) = std::env::var("DARKMUX_PROFILES") {
        if !p.trim().is_empty() && !same_file(Path::new(p.trim()), registry_path) {
            return Some(format!(
                "DARKMUX_PROFILES points dispatch at {}, not at {}, the registry init writes; unset it, or add \
                 the endpoint there by hand.",
                p.trim(),
                registry_path.display()
            ));
        }
    }
    let reg = match darkmux_profiles::profiles::parse_registry_lenient(new_registry) {
        Ok(mut r) => {
            r.materialize_endpoints();
            r
        }
        Err(e) => return Some(format!("the registry would not load ({e:#})")),
    };
    default_engine_setting::resolve(id, darkmux_types::config_access::Source::Config, &reg)
        .err()
        .map(|e| format!("`{id}` would not resolve: {}", e.problem))
}

/// Whether two paths name one file: compared resolved (symlinks such as
/// macOS `/tmp` -> `/private/tmp`), falling back to the spelling when a path
/// does not resolve (the registry may not exist yet on a dry run).
fn same_file(a: &Path, b: &Path) -> bool {
    match (std::fs::canonicalize(a), std::fs::canonicalize(b)) {
        (Ok(x), Ok(y)) => x == y,
        _ => a == b,
    }
}

/// The URL of the registry endpoint `id`, when it is one.
pub fn endpoint_url(registry_json: &str, id: &str) -> Option<String> {
    let mut reg = darkmux_profiles::profiles::parse_registry_lenient(registry_json).ok()?;
    reg.materialize_endpoints();
    reg.endpoints.get(id)?.url.clone()
}

/// Record a chosen server: add its endpoint to the registry text when it is
/// not one already, and return the endpoint id with whether it was added.
/// `Err` names why nothing could be recorded.
pub fn plan_record(outcome: &mut Outcome, registry_json: &str) -> std::result::Result<Option<String>, String> {
    let Outcome::Chosen { found: Found::Server { id, url, .. }, endpoint, added, .. } = outcome else {
        return Ok(None);
    };
    if let Some(existing) = id.clone() {
        *endpoint = Some(existing);
        return Ok(None);
    }
    let taken: Vec<String> = darkmux_profiles::profiles::parse_registry_lenient(registry_json)
        .map(|r| r.endpoints.keys().cloned().collect())
        .unwrap_or_default();
    let new_id = new_endpoint_id(url, &taken);
    let text = add_endpoint(registry_json, &new_id, url)?;
    *endpoint = Some(new_id);
    *added = true;
    Ok(Some(text))
}

#[cfg(test)]
#[path = "init_engine_tests.rs"]
mod tests;
