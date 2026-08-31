//! (#2183) jq-based hook transforms — the "shape" axis of a hook rule.
//!
//! **Settled design (operator design chat, 2026-08-31).** A hook transform
//! is a **pure function, record -> body**. It needs no filesystem, no
//! network, no subprocess — so it gets none. Two orthogonal axes, and only
//! one of them ever holds authority:
//!
//! | Axis | Job | Authority |
//! |---|---|---|
//! | **transform** (`*.jq`, this module) | the shape — record -> body | **none, by construction** |
//! | **transport** (`http`/`file`, `hooks.rs`) | destination + credentials | http: a Keychain header value |
//!
//! This supersedes an earlier `transform_cmd` sketch: executing an
//! operator script per delivery hands a pure function full operator
//! authority (fs, net, spawn, Keychain) to do a job that needs none of it.
//! Exec returns as a *transport* (a follow-up packet), gated, never as the
//! transform.
//!
//! **Why jq specifically:** pure by construction (a crawl finding's
//! `evidence` is a source line copied verbatim from a repo under audit —
//! through a shell-spawning adapter that's an injection target; through jq
//! it's a string), the transform never sees a secret (it receives only the
//! record; credentials are resolved by the Rust delivery path, at POST
//! time, and the two never meet), and it's testable with the real `jq`
//! binary with zero darkmux involved (`cat fixture.json | jq -f
//! jira-issue.jq`).
//!
//! **In-process** — `jaq-core` + `jaq-std` (+ `jaq-json` for the JSON
//! value type the 3.x data-generic core needs), pure Rust, no C bindings.
//! A sidecar adapter service would break the outbox's end-to-end delivery
//! guarantee: darkmux's "delivered" would mean "handed to a process that
//! may have dropped it."
//!
//! `transform` on a rule is a NAME (`"jira-issue.jq"`), never a path —
//! resolved inside the darkmux-owned adapters dir
//! (`hooks_adapters_dir()`, normally `~/.darkmux/hooks/adapters/`) by
//! [`resolve_adapter_path`]. Refused (no `/`, `\`, `..`, no absolute path)
//! at BOTH config load (`load_adapter`, called from `hooks::resolve_rules`
//! — a missing/unparseable adapter is a load-time refusal for that RULE
//! only) and again at delivery (belt-and-braces, same discipline as
//! `try_post`'s URL re-validation).
//!
//! **Bounded like every other evaluation** — [`apply_transform`] enforces
//! a wall-clock cap (compile + run, on a background thread — jaq has no
//! built-in deadline, so an adapter that recurses forever
//! (`def rec: rec; rec`) is bounded the same way `open_redis_connection_
//! bounded` bounds a wedged TCP handshake: spawn, `recv_timeout`, and
//! accept the leaked thread on expiry — a pure-jq thread with no I/O
//! either finishes on its own or spins CPU harmlessly forever, never
//! blocks anything else) and an output-size cap. A jq error, a timeout,
//! an oversize output, or a non-object/non-string result is a TERMINAL
//! failure — never `RetryableFailure` (the #2178 wedge lesson: a
//! construction-class error must route to the give-up path, not retry the
//! same unfixable line forever).

use anyhow::{anyhow, bail, Context, Result};
use std::path::{Path, PathBuf};
use std::time::Duration;

/// Refuse a transform NAME containing a path separator, `..`, or one that
/// parses as absolute. `transform` is a NAME resolved inside the
/// darkmux-owned adapters dir, never an operator-supplied path — this is
/// the SAME belt-and-braces discipline `validate_hook_target_url` applies
/// to `http`, checked at both load and delivery time.
pub fn validate_adapter_name(name: &str) -> Result<()> {
    if name.trim().is_empty() {
        bail!("hook adapter name is empty");
    }
    if name.contains('/') || name.contains('\\') {
        bail!(
            "hook adapter name `{name}` may not contain a path separator — it is a NAME \
             resolved inside the adapters dir, never a path"
        );
    }
    if name.split(['/', '\\']).any(|seg| seg == "..") {
        bail!("hook adapter name `{name}` may not contain `..`");
    }
    if Path::new(name).is_absolute() {
        bail!("hook adapter name `{name}` may not be an absolute path");
    }
    Ok(())
}

/// Resolve an adapter NAME to its path inside `adapters_dir`, validating
/// the name first (see [`validate_adapter_name`]).
pub fn resolve_adapter_path(adapters_dir: &Path, name: &str) -> Result<PathBuf> {
    validate_adapter_name(name)?;
    Ok(adapters_dir.join(name))
}

/// A load-time-validated adapter: it exists, is readable, and PARSES as a
/// jq filter (compiles cleanly against jaq-core + jaq-std + jaq-json).
#[derive(Debug)]
pub struct LoadedAdapter {
    pub path: PathBuf,
    pub source: String,
    /// First 16 hex chars of the adapter's BLAKE3 content hash — printed
    /// by `darkmux doctor` so a silently-changed adapter is visible.
    pub short_hash: String,
}

/// Load + validate an adapter at rule-resolution (load) time. A
/// missing/unreadable file or a filter that fails to compile is an `Err`
/// — the caller (`hooks::resolve_rules`) turns this into a load-time
/// refusal scoped to the ONE rule naming this adapter, not the whole sink.
pub fn load_adapter(adapters_dir: &Path, name: &str) -> Result<LoadedAdapter> {
    let path = resolve_adapter_path(adapters_dir, name)?;
    let source = std::fs::read_to_string(&path)
        .with_context(|| format!("reading hook adapter `{name}` at {}", path.display()))?;
    compile_filter(&source)
        .map_err(|e| anyhow!("{e}"))
        .with_context(|| format!("hook adapter `{name}` failed to parse as a jq filter"))?;
    let short_hash = blake3::hash(source.as_bytes()).to_hex()[..16].to_string();
    Ok(LoadedAdapter { path, source, short_hash })
}

/// Outcome of running a transform against one record line.
pub enum TransformOutcome {
    /// The request body the delivery should carry.
    Body(String),
    /// A TERMINAL failure — compile error, runtime jq error, timeout,
    /// oversize output, or a non-object/non-string result. Bounded to a
    /// few hundred characters (never the operator's full adapter source or
    /// an unbounded jq error trace) so it's safe to carry in `hook.failed`.
    Error(String),
}

const MAX_ERROR_EXCERPT: usize = 500;

fn bounded_excerpt(s: &str) -> String {
    if s.chars().count() <= MAX_ERROR_EXCERPT {
        s.to_string()
    } else {
        let truncated: String = s.chars().take(MAX_ERROR_EXCERPT).collect();
        format!("{truncated}... (truncated)")
    }
}

/// Compile `source` as a jq filter — a pure parse+typecheck, no input
/// required. Used both by [`load_adapter`] (load-time validation) and by
/// [`run_jq`] (delivery-time evaluation; jaq's `Filter` type is tied to a
/// local `Arena`'s lifetime, so there is no way to cache a compiled filter
/// across deliveries without a self-referential wrapper — recompiling per
/// line is the accepted cost, bounded by the same wall-clock cap as the
/// run itself).
fn compile_filter(source: &str) -> std::result::Result<(), String> {
    run_jq(source, "null").map(|_| ()).or_else(|e| {
        // `run_jq` may fail either at compile or at run time; for pure
        // load-time validation we only care that it COMPILES — a filter
        // that compiles but errors against the literal `null` probe input
        // (e.g. one that assumes an object shape) is not a load-time
        // refusal, only a genuine parse/compile failure is. Distinguish
        // by re-running just the compile step.
        match compile_only(source) {
            Ok(()) => Ok(()),
            Err(compile_err) => Err(compile_err.unwrap_or(e)),
        }
    })
}

/// Compile-only check (no run). Returns `Ok(None)`-shaped as `Ok(())`
/// when the filter parses/compiles; `Err(Some(msg))` when it does not.
/// Kept separate from `run_jq` so [`compile_filter`] can tell "doesn't
/// compile" (load-time refusal) apart from "compiles but this probe input
/// doesn't suit it" (fine — real records may suit it even if `null`
/// doesn't).
fn compile_only(source: &str) -> std::result::Result<(), Option<String>> {
    use jaq_core::load::{Arena, File, Loader};
    use jaq_core::Compiler;

    let program = File { code: source, path: () };
    let defs = jaq_core::defs().chain(jaq_std::defs()).chain(jaq_json::defs());
    let funs = jaq_core::funs::<jaq_core::data::JustLut<jaq_json::Val>>().chain(jaq_std::funs()).chain(jaq_json::funs());
    let loader = Loader::new(defs);
    let arena = Arena::default();
    let modules = match loader.load(&arena, program) {
        Ok(m) => m,
        Err(e) => return Err(Some(format!("{e:?}"))),
    };
    match Compiler::default().with_funs(funs).compile(modules) {
        Ok(_) => Ok(()),
        Err(e) => Err(Some(format!("{e:?}"))),
    }
}

/// Compile + run `source` against one JSON input, in-process, synchronous
/// (the caller — [`apply_transform`] — bounds this on a background
/// thread). Requires EXACTLY ONE output value: a transform is `record ->
/// body`, a function, not a generator — zero outputs or more than one is
/// as much a shape error as a non-object/non-string single output.
fn run_jq(source: &str, input_json: &str) -> std::result::Result<jaq_json::Val, String> {
    use jaq_core::{data, unwrap_valr, Compiler, Ctx, Vars};
    use jaq_core::load::{Arena, File, Loader};
    use jaq_json::{read, Val};

    let input =
        read::parse_single(input_json.as_bytes()).map_err(|e| format!("parsing record as JSON: {e:?}"))?;
    let program = File { code: source, path: () };
    let defs = jaq_core::defs().chain(jaq_std::defs()).chain(jaq_json::defs());
    let funs = jaq_core::funs::<jaq_core::data::JustLut<jaq_json::Val>>().chain(jaq_std::funs()).chain(jaq_json::funs());
    let loader = Loader::new(defs);
    let arena = Arena::default();
    let modules = loader.load(&arena, program).map_err(|e| format!("{e:?}"))?;
    let filter = Compiler::default()
        .with_funs(funs)
        .compile(modules)
        .map_err(|e| format!("{e:?}"))?;
    let ctx = Ctx::<data::JustLut<Val>>::new(&filter.lut, Vars::new([]));
    let mut out = filter.id.run((ctx, input)).map(unwrap_valr);
    let first = out.next();
    if out.next().is_some() {
        return Err("adapter produced more than one output value — a transform must yield exactly one".to_string());
    }
    match first {
        Some(Ok(v)) => Ok(v),
        Some(Err(e)) => Err(format!("{e}")),
        None => Err("adapter produced no output".to_string()),
    }
}

/// A successfully-produced `Val` must be a JSON object (serialized
/// compactly, becoming the request body verbatim) or a string (its raw
/// UTF-8 bytes become the body verbatim — lets an adapter emit a
/// non-JSON body). Any other shape (`null`, a bare number/bool, an array)
/// is a TERMINAL shape error, same bucket as a jq runtime error.
fn val_to_body(v: &jaq_json::Val) -> std::result::Result<String, String> {
    if matches!(v, jaq_json::Val::Obj(_)) {
        return Ok(v.to_string());
    }
    if let Ok(bytes) = v.try_as_utf8_bytes_owned() {
        return String::from_utf8(bytes.as_ref().to_vec())
            .map_err(|e| format!("adapter produced a non-UTF-8 string result: {e}"));
    }
    Err(format!("adapter must produce a JSON object or a string — got {}", bounded_excerpt(&v.to_string())))
}

/// Apply `source` (an already-loaded adapter's jq text) to `record_line`
/// (the raw outbox line — valid JSON, already checked by the caller),
/// bounded by `timeout` (wall-clock, compile + run) and
/// `max_output_bytes` (the produced body's byte length). Runs on a
/// background thread so a runaway filter can't block the drainer past
/// `timeout` — mirrors `open_redis_connection_bounded`'s accepted
/// leaked-thread trade-off (a pure-jq thread with no I/O either finishes
/// on its own or spins CPU harmlessly; it can never wedge a socket or a
/// file lock the rest of the process depends on).
pub fn apply_transform(source: &str, record_line: &str, timeout: Duration, max_output_bytes: usize) -> TransformOutcome {
    let source = source.to_string();
    let record_line = record_line.to_string();
    let (tx, rx) = std::sync::mpsc::channel();
    let spawned = std::thread::Builder::new().name("hook-jq-transform".to_string()).spawn(move || {
        let result = run_jq(&source, &record_line).and_then(|v| val_to_body(&v));
        let _ = tx.send(result);
    });
    if spawned.is_err() {
        return TransformOutcome::Error("failed to spawn jq evaluation thread".to_string());
    }
    match rx.recv_timeout(timeout) {
        Ok(Ok(body)) => {
            if body.len() > max_output_bytes {
                TransformOutcome::Error(format!(
                    "adapter output is {} bytes, over the {max_output_bytes}-byte cap",
                    body.len()
                ))
            } else {
                TransformOutcome::Body(body)
            }
        }
        Ok(Err(e)) => TransformOutcome::Error(bounded_excerpt(&e)),
        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
            TransformOutcome::Error(format!("jq evaluation exceeded the {}ms wall-clock cap", timeout.as_millis()))
        }
        Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
            TransformOutcome::Error("jq evaluation thread panicked".to_string())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validate_adapter_name_refuses_traversal_and_absolute() {
        assert!(validate_adapter_name("jira-issue.jq").is_ok());
        assert!(validate_adapter_name("../secrets.jq").is_err());
        assert!(validate_adapter_name("a/../b.jq").is_err());
        assert!(validate_adapter_name("sub/dir.jq").is_err());
        assert!(validate_adapter_name("sub\\dir.jq").is_err());
        assert!(validate_adapter_name("/etc/passwd").is_err());
        assert!(validate_adapter_name("").is_err());
        assert!(validate_adapter_name("   ").is_err());
    }

    #[test]
    fn resolve_adapter_path_joins_inside_dir() {
        let dir = Path::new("/home/op/.darkmux/hooks/adapters");
        let p = resolve_adapter_path(dir, "jira-issue.jq").unwrap();
        assert_eq!(p, dir.join("jira-issue.jq"));
        assert!(resolve_adapter_path(dir, "../evil.jq").is_err());
    }

    #[test]
    fn apply_transform_object_becomes_body() {
        let record = r#"{"action":"dispatch.tool","payload":{"tool_name":"report_finding"}}"#;
        let out = apply_transform(
            r#"{summary: .payload.tool_name}"#,
            record,
            Duration::from_secs(5),
            1_048_576,
        );
        match out {
            TransformOutcome::Body(b) => {
                let v: serde_json::Value = serde_json::from_str(&b).unwrap();
                assert_eq!(v["summary"], "report_finding");
            }
            TransformOutcome::Error(e) => panic!("expected a body, got error: {e}"),
        }
    }

    #[test]
    fn apply_transform_string_output_is_raw_body() {
        let out = apply_transform(r#""plain text body""#, "{}", Duration::from_secs(5), 1_048_576);
        match out {
            TransformOutcome::Body(b) => assert_eq!(b, "plain text body"),
            TransformOutcome::Error(e) => panic!("expected a body, got error: {e}"),
        }
    }

    #[test]
    fn apply_transform_non_object_non_string_is_terminal_error() {
        let out = apply_transform("42", "{}", Duration::from_secs(5), 1_048_576);
        assert!(matches!(out, TransformOutcome::Error(_)));
        let out = apply_transform("null", "{}", Duration::from_secs(5), 1_048_576);
        assert!(matches!(out, TransformOutcome::Error(_)));
        let out = apply_transform("[1,2,3]", "{}", Duration::from_secs(5), 1_048_576);
        assert!(matches!(out, TransformOutcome::Error(_)));
    }

    #[test]
    fn apply_transform_jq_syntax_error_is_terminal() {
        let out = apply_transform("{{{{not valid jq", "{}", Duration::from_secs(5), 1_048_576);
        assert!(matches!(out, TransformOutcome::Error(_)));
    }

    #[test]
    fn apply_transform_jq_runtime_error_is_terminal() {
        // `error` is jq's own explicit-failure builtin.
        let out = apply_transform(r#"error("boom")"#, "{}", Duration::from_secs(5), 1_048_576);
        match out {
            TransformOutcome::Error(e) => assert!(e.contains("boom"), "expected the jq error text, got: {e}"),
            TransformOutcome::Body(b) => panic!("expected an error, got body: {b}"),
        }
    }

    #[test]
    fn apply_transform_output_cap_fires() {
        let out = apply_transform(r#"{s: "x" * 100}"#, "{}", Duration::from_secs(5), 10);
        match out {
            TransformOutcome::Error(e) => assert!(e.contains("cap"), "expected a cap-exceeded error, got: {e}"),
            TransformOutcome::Body(b) => panic!("expected the output cap to fire, got body of {} bytes", b.len()),
        }
    }

    #[test]
    fn apply_transform_wall_clock_cap_fires() {
        // An unbounded jq recursion — the wall-clock cap must fire rather
        // than hang the test (or the drainer, in production).
        let out = apply_transform("def rec: rec; rec", "{}", Duration::from_millis(200), 1_048_576);
        match out {
            TransformOutcome::Error(e) => assert!(e.contains("wall-clock"), "expected a wall-clock-cap error, got: {e}"),
            TransformOutcome::Body(b) => panic!("expected the wall-clock cap to fire, got body: {b}"),
        }
    }

    #[test]
    fn load_adapter_rejects_unparseable_filter() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("broken.jq"), "{{{{not valid jq").unwrap();
        let err = load_adapter(dir.path(), "broken.jq").unwrap_err();
        assert!(format!("{err:#}").contains("failed to parse"), "got: {err:#}");
    }

    #[test]
    fn load_adapter_accepts_valid_filter_and_hashes_it() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("ok.jq"), "{summary: .payload.tool_name}").unwrap();
        let loaded = load_adapter(dir.path(), "ok.jq").unwrap();
        assert_eq!(loaded.short_hash.len(), 16);
        assert!(loaded.short_hash.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn load_adapter_refuses_missing_file() {
        let dir = tempfile::tempdir().unwrap();
        assert!(load_adapter(dir.path(), "nope.jq").is_err());
    }
}
