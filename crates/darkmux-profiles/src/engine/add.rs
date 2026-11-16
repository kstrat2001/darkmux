//! (#3569) Adding a detected engine to `profiles.json`: an endpoint that
//! declares it and a profile that runs one of its models.
//!
//! Only an engine darkmux starts servers for (Tensorfold, rec's Flint) is
//! added: LM Studio needs no endpoint, and the default engine is one
//! already. darkmux adds what the request names and nothing it chose: the
//! model is the one named, or the engine's only one; the context is the one
//! named, or the one the engine states for the model (a rec revision's
//! `max_context`). An engine that states none, or holds several models
//! when none is named, is refused naming what to send.
//!
//! The caller writes only after the user confirms ([`plan`] writes nothing;
//! [`write`] is the one write). The edit is text-level, as every `init` edit
//! of the operator's file is, so its formatting and comments of layout
//! survive; it is checked before it is written: the file must pass the
//! unknown-key check (contract 7) before and after, the result must read
//! back as the original plus exactly the two new entries, and the new
//! endpoint must classify as the engine. A file changed since it was read is
//! never overwritten.

use super::detect::{DetectedEngine, DetectedKind, DetectedState};
use darkmux_types::config_enum::ConfigEnum;
use darkmux_types::{EndpointKind, ManagedBackend};
use std::path::Path;

/// What to add: the engine, and optionally which of its models and at what
/// context.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AddRequest {
    pub engine: DetectedKind,
    pub model: Option<String>,
    pub n_ctx: Option<u32>,
}

/// Where a planned context came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContextFrom {
    /// The request named it.
    Requested,
    /// The engine states it as the model's largest context.
    EngineStated,
}

/// An add, planned and checked, not yet written.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Planned {
    pub engine: ManagedBackend,
    /// The new endpoint's id.
    pub endpoint: String,
    /// The loopback port darkmux will serve the engine on.
    pub port: u16,
    /// The new profile's name.
    pub profile: String,
    pub model: String,
    pub n_ctx: u32,
    pub n_ctx_from: ContextFrom,
    /// The registry with both entries added.
    pub text: String,
}

impl Planned {
    /// What the add writes, as one line a confirmation shows.
    pub fn summary(&self) -> String {
        let from = match self.n_ctx_from {
            ContextFrom::Requested => "",
            ContextFrom::EngineStated => ", the largest the engine states for it",
        };
        format!(
            "endpoint `{}` (`\"managed\": \"{}\"`, port {}) and profile `{}` running `{}` at a context of {} tokens{from}",
            self.endpoint,
            self.engine.token(),
            self.port,
            self.profile,
            self.model,
            self.n_ctx
        )
    }
}

/// Why an add was refused, by kind: what an app branches on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Refusal {
    /// The engine is not installed, or not usable as it is.
    Unavailable(String),
    /// It is configured already.
    Configured(String),
    /// The request names something the engine does not have, or leaves out
    /// what darkmux cannot fill in.
    BadRequest(String),
    /// `profiles.json` cannot be edited safely.
    Registry(String),
    /// `profiles.json` changed since it was read.
    Changed(String),
}

impl std::fmt::Display for Refusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let (Refusal::Unavailable(m)
        | Refusal::Configured(m)
        | Refusal::BadRequest(m)
        | Refusal::Registry(m)
        | Refusal::Changed(m)) = self;
        f.write_str(m)
    }
}

/// The first port darkmux offers an engine it adds; ports are taken upward
/// from it ([`FIRST_PORT`]..[`LAST_PORT`]), skipping any an endpoint
/// declares or something listens on. 8091 is the port DESIGN.md's
/// Tensorfold example uses.
pub const FIRST_PORT: u16 = 8091;
/// The last port offered.
pub const LAST_PORT: u16 = 8190;

/// Plan adding the engine `req` names to the registry `text` (the file at
/// `path`), from what detection found. Writes nothing. `port_free` says
/// whether nothing listens on a port.
pub fn plan(
    text: &str,
    path: &Path,
    found: &[DetectedEngine],
    req: &AddRequest,
    port_free: &dyn Fn(u16) -> bool,
) -> Result<Planned, Refusal> {
    let backend = addable_kind(req.engine)?;
    let engine = usable_unconfigured(found, req.engine)?;
    let (model, n_ctx, n_ctx_from) = chosen_model(engine, req)?;
    let original = clean_registry(text, path)?;
    let registry = crate::profiles::parse_registry_lenient(text)
        .map_err(|e| Refusal::Registry(format!("{e:#}")))?;
    let endpoint = free_name(
        backend.token(),
        &registry.endpoints.keys().cloned().collect::<Vec<_>>(),
    );
    let profile = free_name(
        backend.token(),
        &registry.profiles.keys().cloned().collect::<Vec<_>>(),
    );
    let port = free_port(&registry, port_free)?;
    let entries = Entries::new(backend, &endpoint, port, &profile, &model, n_ctx, engine);
    let edited = entries.insert(text)?;
    entries.verify(&original, &edited, path)?;
    Ok(Planned {
        engine: backend,
        endpoint,
        port,
        profile,
        model,
        n_ctx,
        n_ctx_from,
        text: edited,
    })
}

/// The `managed` backend of an engine darkmux can add.
fn addable_kind(kind: DetectedKind) -> Result<ManagedBackend, Refusal> {
    kind.startable().ok_or_else(|| {
        Refusal::BadRequest(format!(
            "{} needs nothing added: darkmux runs work on it as it is. Only an engine darkmux starts servers for \
             (`tensorfold`, `flint`) is added",
            kind.label()
        ))
    })
}

/// The detected engine of `kind`, when it is usable and not configured.
fn usable_unconfigured(
    found: &[DetectedEngine],
    kind: DetectedKind,
) -> Result<&DetectedEngine, Refusal> {
    let Some(engine) = found.iter().find(|d| d.engine == kind) else {
        return Err(Refusal::Unavailable(format!(
            "{} is not installed on this machine",
            kind.label()
        )));
    };
    if engine.configured {
        return Err(Refusal::Configured(format!(
            "{} is configured already, as endpoint {}; edit profiles.json to change it",
            kind.label(),
            engine
                .endpoints
                .iter()
                .map(|e| format!("`{e}`"))
                .collect::<Vec<_>>()
                .join(", ")
        )));
    }
    if engine.state != DetectedState::Usable {
        let why = engine.reason.as_deref().unwrap_or("it did not say why");
        return Err(Refusal::Unavailable(format!(
            "{} cannot be used: {why}",
            kind.label()
        )));
    }
    Ok(engine)
}

/// The model to add and its context: the named one (or the only one), at
/// the named context (or the one the engine states).
fn chosen_model(
    engine: &DetectedEngine,
    req: &AddRequest,
) -> Result<(String, u32, ContextFrom), Refusal> {
    let held = || {
        engine
            .models
            .iter()
            .map(|m| format!("`{}`", m.id))
            .collect::<Vec<_>>()
            .join(", ")
    };
    let model = match (&req.model, engine.models.as_slice()) {
        (Some(id), models) => models.iter().find(|m| &m.id == id).ok_or_else(|| {
            Refusal::BadRequest(format!(
                "{} has no model `{id}`; it holds {}",
                engine.engine.label(),
                held()
            ))
        })?,
        (None, [only]) => only,
        (None, _) => {
            return Err(Refusal::BadRequest(format!(
                "{} holds several models ({}); name the one to add",
                engine.engine.label(),
                held()
            )))
        }
    };
    let (n_ctx, from) = context(engine.engine, &model.id, req.n_ctx, model.max_context)?;
    Ok((model.id.clone(), n_ctx, from))
}

/// The context to add `id` at: the requested one, within what the engine
/// states, or the one it states.
fn context(
    kind: DetectedKind,
    id: &str,
    asked: Option<u32>,
    stated: Option<u32>,
) -> Result<(u32, ContextFrom), Refusal> {
    match (asked, stated) {
        (Some(0), _) => Err(Refusal::BadRequest(
            "a context of 0 tokens runs nothing; name a larger one".into(),
        )),
        (Some(n), Some(max)) if n > max => Err(Refusal::BadRequest(format!(
            "`{id}` takes at most {max} tokens of context, as {} states; name {max} or fewer",
            kind.label()
        ))),
        (Some(n), _) => Ok((n, ContextFrom::Requested)),
        (None, Some(max)) => Ok((max, ContextFrom::EngineStated)),
        (None, None) => Err(Refusal::BadRequest(format!(
            "{} states no context for `{id}`; name the context to run it at (`n_ctx`)",
            kind.label()
        ))),
    }
}

/// The registry's parsed text, when it passes the unknown-key check
/// (contract 7): a file the loader refuses is never edited.
fn clean_registry(text: &str, path: &Path) -> Result<serde_json::Value, Refusal> {
    if let Some(problem) = crate::profiles::user_file_problem_text(path, text) {
        return Err(Refusal::Registry(format!(
            "{problem}; fix it before adding an engine"
        )));
    }
    serde_json::from_str(text)
        .map_err(|e| Refusal::Registry(format!("{} is not JSON ({e})", path.display())))
}

/// `base`, or `base-2`, `base-3`… when it is taken.
pub fn free_name(base: &str, taken: &[String]) -> String {
    let mut name = base.to_string();
    let mut n = 2;
    while taken.iter().any(|t| t == &name) {
        name = format!("{base}-{n}");
        n += 1;
    }
    name
}

/// The first port from [`FIRST_PORT`] that no endpoint declares and nothing
/// listens on.
fn free_port(
    registry: &darkmux_types::ProfileRegistry,
    port_free: &dyn Fn(u16) -> bool,
) -> Result<u16, Refusal> {
    let declared: Vec<u16> = registry.endpoints.values().filter_map(|e| e.port).collect();
    (FIRST_PORT..=LAST_PORT)
        .find(|p| !declared.contains(p) && port_free(*p))
        .ok_or_else(|| {
            Refusal::Registry(format!(
                "no port from {FIRST_PORT} to {LAST_PORT} is free for the engine's server"
            ))
        })
}

/// The two entries an add writes.
struct Entries {
    backend: ManagedBackend,
    endpoint: (String, serde_json::Value),
    profile: (String, serde_json::Value),
}

impl Entries {
    fn new(
        backend: ManagedBackend,
        endpoint: &str,
        port: u16,
        profile: &str,
        model: &str,
        n_ctx: u32,
        engine: &DetectedEngine,
    ) -> Self {
        let version = engine
            .version
            .as_deref()
            .map(|v| format!(" {v}"))
            .unwrap_or_default();
        let description = format!(
            "{}{version} serving `{model}`, added by darkmux",
            engine.engine.label()
        );
        Entries {
            backend,
            endpoint: (
                endpoint.to_string(),
                serde_json::json!({ "managed": backend.token(), "port": port }),
            ),
            profile: (
                profile.to_string(),
                serde_json::json!({
                    "description": description,
                    "models": [{ "id": model, "n_ctx": n_ctx, "endpoint": endpoint }]
                }),
            ),
        }
    }

    fn insert(&self, text: &str) -> Result<String, Refusal> {
        let text = insert_member(text, "endpoints", &self.endpoint.0, &self.endpoint.1)?;
        insert_member(&text, "profiles", &self.profile.0, &self.profile.1)
    }

    /// The edit reads back as `original` plus exactly the two entries,
    /// passes the unknown-key check, and the endpoint classifies as the
    /// engine with the profile's model on it.
    fn verify(
        &self,
        original: &serde_json::Value,
        edited: &str,
        path: &Path,
    ) -> Result<(), Refusal> {
        let not_written =
            |why: String| Refusal::Registry(format!("the edit was not written: {why}"));
        let back: serde_json::Value = serde_json::from_str(edited)
            .map_err(|e| not_written(format!("it would not parse ({e})")))?;
        if back != self.expected(original) {
            return Err(not_written(
                "it did not read back as the registry plus the two entries".into(),
            ));
        }
        if let Some(problem) = crate::profiles::user_file_problem_text(path, edited) {
            return Err(not_written(problem.to_string()));
        }
        self.classifies(edited).map_err(not_written)
    }

    fn expected(&self, original: &serde_json::Value) -> serde_json::Value {
        let mut want = original.clone();
        for (map, (id, value)) in [("endpoints", &self.endpoint), ("profiles", &self.profile)] {
            let slot = want
                .as_object_mut()
                .map(|o| o.entry(map).or_insert_with(|| serde_json::json!({})));
            if let Some(serde_json::Value::Object(m)) = slot {
                m.insert(id.clone(), value.clone());
            }
        }
        want
    }

    fn classifies(&self, edited: &str) -> Result<(), String> {
        let mut reg =
            crate::profiles::parse_registry_lenient(edited).map_err(|e| format!("{e:#}"))?;
        reg.materialize_endpoints();
        let kind = reg.endpoints.get(&self.endpoint.0).map(|e| e.kind());
        if !matches!(kind, Some(Ok(EndpointKind::Managed(b))) if b == self.backend) {
            return Err(format!(
                "endpoint `{}` would not read as `{}`",
                self.endpoint.0,
                self.backend.token()
            ));
        }
        let model = reg
            .profiles
            .get(&self.profile.0)
            .and_then(|p| p.models.first());
        match model.map(|m| m.endpoint_kind()) {
            Some(Ok(EndpointKind::Managed(b))) if b == self.backend => Ok(()),
            _ => Err(format!(
                "profile `{}` would not run on endpoint `{}`",
                self.profile.0, self.endpoint.0
            )),
        }
    }
}

/// Write the planned registry to `path`, when the file still holds
/// `original` (what the plan was made from): into a file beside it, with
/// its permissions, then renamed over it.
pub fn write(path: &Path, original: &str, planned: &Planned) -> Result<(), Refusal> {
    let io = |what: &str, e: std::io::Error| {
        Refusal::Registry(format!("{what} {}: {e}", path.display()))
    };
    let now = std::fs::read_to_string(path).map_err(|e| io("reading", e))?;
    if now != original {
        return Err(Refusal::Changed(format!(
            "{} changed since it was read; nothing was written. Ask again",
            path.display()
        )));
    }
    let next = path.with_extension("json.darkmux-add");
    std::fs::write(&next, &planned.text).map_err(|e| io("writing beside", e))?;
    if let Ok(meta) = std::fs::metadata(path) {
        let _ = std::fs::set_permissions(&next, meta.permissions());
    }
    std::fs::rename(&next, path).map_err(|e| {
        let _ = std::fs::remove_file(&next);
        io("replacing", e)
    })
}

// ─── A member added to a top-level map, as text ─────────────────────────────

/// `text` with `"id": value` added first in the top-level object `map`
/// (made when absent). The rest of the text is kept byte for byte.
pub fn insert_member(
    text: &str,
    map: &str,
    id: &str,
    value: &serde_json::Value,
) -> Result<String, Refusal> {
    let entry = format!("{}: {}", serde_json::Value::from(id), value);
    let shape =
        || Refusal::Registry("profiles.json is not a JSON object darkmux can add to".to_string());
    match member_value(text, map) {
        Some(at) if text.as_bytes()[at] == b'{' => {
            Ok(after_brace(text, at, &format!("\n    {entry}")))
        }
        Some(_) => Err(Refusal::Registry(format!(
            "profiles.json's `{map}` is not an object"
        ))),
        None => {
            let root = skip_ws(text.as_bytes(), 0)
                .filter(|&i| text.as_bytes()[i] == b'{')
                .ok_or_else(shape)?;
            Ok(after_brace(
                text,
                root,
                &format!(
                    "\n  {}: {{\n    {entry}\n  }}",
                    serde_json::Value::from(map)
                ),
            ))
        }
    }
}

/// `text` with `member` written right after the `{` at `open`, followed by
/// a comma unless the object was empty.
fn after_brace(text: &str, open: usize, member: &str) -> String {
    let rest = &text[open + 1..];
    let empty = rest.trim_start().starts_with('}');
    let sep = if empty { "\n" } else { "," };
    format!("{}{member}{sep}{rest}", &text[..=open])
}

/// Where the value of the root object's member `key` starts.
fn member_value(text: &str, key: &str) -> Option<usize> {
    let b = text.as_bytes();
    let root = skip_ws(b, 0).filter(|&i| b[i] == b'{')?;
    let mut i = skip_ws(b, root + 1)?;
    while b[i] == b'"' {
        let (name, value) = member_at(text, i)?;
        if name == key {
            return Some(value);
        }
        i = next_member(b, skip_value(b, value)?)?;
    }
    None
}

/// The member whose key opens at `i`: its key's raw text, and where its
/// value starts.
fn member_at(text: &str, i: usize) -> Option<(&str, usize)> {
    let b = text.as_bytes();
    let (name, after) = string_at(text, i)?;
    let colon = skip_ws(b, after).filter(|&c| b[c] == b':')?;
    Some((name, skip_ws(b, colon + 1)?))
}

/// After a member's value: the start of the next member's key, or `None`
/// at the object's end.
fn next_member(b: &[u8], after_value: usize) -> Option<usize> {
    let i = skip_ws(b, after_value)?;
    (b[i] == b',').then(|| skip_ws(b, i + 1)).flatten()
}

/// The first non-whitespace byte at or after `i`.
fn skip_ws(b: &[u8], i: usize) -> Option<usize> {
    (i..b.len()).find(|&j| !b[j].is_ascii_whitespace())
}

/// The raw text of the string that opens at `i`, and the index after it.
fn string_at(text: &str, i: usize) -> Option<(&str, usize)> {
    let end = string_end(text.as_bytes(), i)?;
    Some((&text[i + 1..end - 1], end))
}

/// The index after the string that opens at `i`.
fn string_end(b: &[u8], i: usize) -> Option<usize> {
    let mut j = i + 1;
    while j < b.len() {
        match b[j] {
            b'\\' => j += 2,
            b'"' => return Some(j + 1),
            _ => j += 1,
        }
    }
    None
}

/// The index after the value that starts at `i`.
fn skip_value(b: &[u8], i: usize) -> Option<usize> {
    match b[i] {
        b'"' => string_end(b, i),
        b'{' | b'[' => container_end(b, i),
        _ => {
            (i..b.len()).find(|&j| matches!(b[j], b',' | b'}' | b']') || b[j].is_ascii_whitespace())
        }
    }
}

/// The index after the object or array that opens at `i`.
fn container_end(b: &[u8], i: usize) -> Option<usize> {
    let mut depth = 0usize;
    let mut j = i;
    while j < b.len() {
        match b[j] {
            b'"' => j = string_end(b, j)?,
            b'{' | b'[' => (depth, j) = (depth + 1, j + 1),
            b'}' | b']' if depth == 1 => return Some(j + 1),
            b'}' | b']' => (depth, j) = (depth - 1, j + 1),
            _ => j += 1,
        }
    }
    None
}

#[cfg(test)]
#[path = "add_tests.rs"]
mod tests;
