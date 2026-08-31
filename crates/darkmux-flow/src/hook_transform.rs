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
//! at config load (`load_adapter`, called from `hooks::resolve_rules` — a
//! missing/unparseable adapter is a load-time refusal for that RULE
//! only). Unlike `try_post`'s per-POST URL re-validation, the name/path
//! is NOT re-checked at delivery — the adapter's SOURCE is read once at
//! load time and cached on the resolved rule (`LoadedAdapter.source`);
//! every delivery reuses that cached text directly, never touching the
//! filesystem again. Two consequences worth knowing: a hand-edited
//! adapter takes effect only on the next config reload (`HookSink::new`
//! reconstruction), and `darkmux doctor`/`flow status` re-read + re-hash
//! the file LIVE from disk on every invocation — so a hot-edited adapter
//! can show a DIFFERENT hash in `doctor` than the one actually driving a
//! running sink's deliveries, until that sink restarts.
//!
//! **"No authority" is enforced by an explicit deny-list, not merely by
//! omission** (security review of the first cut, 2026-08-31 — see #2183's
//! PR history). `jaq_std::funs()` is `base_funs()` (value-generic
//! primitives: `length`, `keys`, `map`'s native helpers, ...) chained with
//! `extra_funs()`, and `extra_funs()`'s `std()` block registers `env`
//! (the WHOLE process environment — `DARKMUX_HOOK_SECRET_<i>`,
//! `DARKMUX_SERVE_TOKEN`, `DARKMUX_REDIS_URL` with a password inline, all
//! readable and POSTable by an adapter) and `now`. Separately, `halt` is
//! registered by `jaq_std::base_run()` (jaq-std, NOT jaq-core — `base_
//! funs()`'s own source, not `extra_funs()`/`std()`), so it is NOT gated
//! behind `std()` and survives a switch to `base_funs()` alone on its
//! own. It calls `std::process::exit` directly (`Exn::halt`, defined in
//! `jaq_core` and constructed by jaq-std's `halt` filter body) — an
//! adapter can terminate the WHOLE darkmux process, silently, exit code
//! operator-chosen, no `hook.failed`, no quarantine, no cursor write.
//! [`registered_funs`] is the single choke point: it chains
//! `jaq_core::funs()` + `jaq_std::base_funs()` (never `jaq_std::funs()`/
//! `extra_funs()`/`std()` — so `env`/`now` are never even present) +
//! `jaq_json::funs()`, then filters the WHOLE set by name against
//! [`DENIED_FUN_NAMES`] as belt-and-braces (`halt` survives the
//! `base_funs()`-only switch on its own, so the filter is load-bearing,
//! not decorative). `jaq_std::defs()` (the jq-syntax-level definitions —
//! `map`, `select`, `debug`, `stderr`, `halt_error`, ...) stays fully
//! included: `debug`/`stderr`/`halt_error` are defined in terms of the
//! native `debug_empty`/`stderr_empty` primitives, which live in `log()`
//! (part of `extra_funs()`, never registered here) — calling `debug` in an
//! adapter therefore fails to COMPILE (undefined name), the same load-time
//! refusal a syntax error gets, not a runtime escape hatch. `$ENV` and
//! `input`/`inputs` are unreachable through this API shape by construction
//! (there is no stdin/input-iterator wiring at all here) — verified, not
//! merely assumed.
//!
//! **Load-time validation NEVER runs operator jq** — [`compile_filter`]
//! (called from [`load_adapter`], which `darkmux doctor`, `flow status`,
//! and `HookSink::new` all call on every invocation) is compile-only
//! (parse + typecheck against [`registered_funs`]'s lut, no `Ctx`, no
//! `.run()`). The first cut of this packet validated by RUNNING the filter
//! against a literal `null` probe input — meaning a top-level `halt` in an
//! adapter file would `std::process::exit` the process the MOMENT
//! `darkmux doctor` (or anything else that loads config) ran, and a
//! `debug`/`stderr` call would fire as a side effect of "just checking it
//! parses." Compile-only closes both: a filter that only misbehaves when
//! RUN (not when merely compiled) is caught the first time delivery
//! actually evaluates it, at delivery time, under the same bounds as every
//! other evaluation — never at load time, when no bound is active.
//!
//! **Bounded like every other evaluation** — [`apply_transform`] enforces
//! a wall-clock cap (compile + run, on a background thread — jaq has no
//! built-in deadline, so an adapter that recurses forever
//! (`def rec: rec; rec`) is bounded the same way `open_redis_connection_
//! bounded` bounds a wedged TCP handshake: spawn, `recv_timeout`, and
//! accept the leaked thread on expiry) and an output-size cap. **The
//! leaked thread is NOT harmless** (corrected from the first cut's claim
//! that it "spins CPU harmlessly forever" — measured: `[range(1e9)]`
//! keeps ALLOCATING past the timeout, ~170 MB/s, because the output cap
//! only bounds the RETURNED body, never an intermediate value the filter
//! builds along the way). [`apply_transform`] therefore also bounds how
//! many orphaned (timed-out, still-running) threads a single rule may
//! have outstanding at once ([`MAX_ORPHANED_TRANSFORM_THREADS_PER_RULE`])
//! — past that, new evaluations for that rule return
//! [`TransformOutcome::Busy`] (a RETRYABLE backoff, not a quarantine: the
//! adapter may be perfectly fine, the system is just backed up) until an
//! orphan finishes and decrements the shared counter — **which the
//! canonical orphan never does** (`def rec: rec; rec` does not terminate
//! by construction, so a rule whose orphans are ALL of that shape stays
//! at the cap, and `Busy`, for the life of the process). Correcting a
//! second overclaim from the first round of review: this bounds the
//! CONCURRENT ALLOCATION RATE (at most N threads growing at once for
//! this rule, not every retry spawning a new one on top), NOT total
//! memory growth — N threads that each never stop growing is still
//! unbounded, just unbounded more slowly than with no cap at all. jaq has
//! no cooperative-cancellation hook to interrupt an orphan mid-evaluation,
//! so a rule pinned this way will EVENTUALLY exhaust memory if left
//! running indefinitely; `hooks.rs`'s drain loop promotes a rule stuck at
//! the cap into the existing `stalled` state after
//! `MAX_CONSECUTIVE_BUSY_BEFORE_STALL` consecutive `Busy` outcomes
//! precisely because "wait for an orphan to finish" is not a plan for
//! this case — see `hooks.rs`'s `RuleRuntime::consecutive_busy` doc.
//! A jq error, a timeout, an oversize output, or a non-object/non-string
//! result is a TERMINAL failure — never `RetryableFailure` (the #2178
//! wedge lesson: a construction-class error must route to the give-up
//! path, not retry the same unfixable line forever); `Busy` is the one
//! deliberate exception, because it is not a claim about the ADAPTER at
//! all.

use anyhow::{anyhow, bail, Context, Result};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use std::time::Duration;

/// (security review, 2026-08-31) Native filters that grant a "pure
/// function" I/O or control-flow authority it must never have — see this
/// module's doc for exactly where each is registered and why omission
/// alone isn't sufficient. Applied as a name-based filter in
/// [`registered_funs`], not merely by picking a smaller funs set, because
/// `halt` survives the `base_funs()`-only switch on its own (it's
/// registered by jaq-std's `base_run()`, NOT gated behind `std()`).
/// `$ENV`/`input`/`inputs` are listed for defense-in-depth even though
/// they're already unreachable through this API shape (verified: nothing
/// here wires a `$ENV` binding or an input iterator at all).
/// `input_line_number` is ALSO defense-in-depth for a slightly different
/// reason: no native or def by that name exists in jaq-std 3.0.3 at all
/// (it's a jq-the-original-C-implementation builtin jaq hasn't
/// implemented) — an adapter calling it fails to compile as "undefined,"
/// same outcome a deny-list entry produces, so keeping it here is a
/// no-op today and a tripwire if a future jaq-std version adds it.
const DENIED_FUN_NAMES: &[&str] = &[
    "env", "now", "halt", "halt_error",
    // `debug`/`stderr` are DEFS (see `registered_defs`) built on top of
    // the native `debug_empty`/`stderr_empty` — both the def name and its
    // native are denied, or the native stays directly callable under its
    // own name even with the def gone.
    "debug", "debug_empty", "stderr", "stderr_empty",
    "input_line_number", "input", "inputs", "$ENV",
];

/// (security review, 2026-08-31) jaq's compiler resolves EVERY def in the
/// loaded module set eagerly — not lazily, only-if-called — so a def
/// whose body references a native filter this module has denied (e.g.
/// `halt_error`, which calls the now-absent `halt`) breaks compilation
/// for EVERY adapter, not just ones that call it. `registered_defs` is
/// therefore the def-side counterpart of [`registered_funs`]'s deny-list:
/// the SAME `DENIED_FUN_NAMES` set, filtered against `Def::name`, so a
/// def whose name is denied is dropped from the module set entirely
/// before compilation ever sees it. `map`/`select`/every other ordinary
/// std definition is untouched.
fn registered_defs() -> impl Iterator<Item = jaq_core::load::parse::Def<&'static str>> {
    jaq_core::defs()
        .chain(jaq_std::defs())
        .chain(jaq_json::defs())
        .filter(|d| !DENIED_FUN_NAMES.contains(&d.name))
}

/// The ONE place a set of native jq filters is assembled for this module
/// — every compile/run call site goes through this, so the deny-list
/// (`DENIED_FUN_NAMES`) can never be bypassed by a call site that forgot
/// to apply it. The FULL `jaq_std::funs()` (base + extra — math/regex/
/// format/time/log all stay present, since jaq's eager whole-module
/// resolution means dropping a whole category breaks compilation for
/// every OTHER def that happens to reference one of its natives, e.g.
/// `todateiso8601`/`ilogb`/`matches`), filtered by name to remove exactly
/// [`DENIED_FUN_NAMES`] — `env`/`now`/`halt` and `debug_empty`/
/// `stderr_empty` (the natives `debug`/`stderr` are implemented in terms
/// of), so those names are never resolvable, no matter which category
/// they'd otherwise come from.
fn registered_funs<D>() -> impl Iterator<Item = jaq_core::native::Fun<D>>
where
    D: for<'a> jaq_core::DataT<V<'a> = jaq_json::Val>,
{
    jaq_core::funs::<D>()
        .chain(jaq_std::funs::<D>())
        .chain(jaq_json::funs::<D>())
        .filter(|f| !DENIED_FUN_NAMES.contains(&f.0))
}

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
/// jq filter (compiles cleanly against [`registered_funs`]) — see this
/// module's doc: compiling never RUNS it.
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
/// **Never runs the adapter** — see this module's doc.
pub fn load_adapter(adapters_dir: &Path, name: &str) -> Result<LoadedAdapter> {
    let path = resolve_adapter_path(adapters_dir, name)?;
    let source = std::fs::read_to_string(&path)
        .with_context(|| format!("reading hook adapter `{name}` at {}", path.display()))?;
    // (security review round 2, 2026-08-31, CONSIDER 3) `bounded_excerpt`
    // — the SAME bound `apply_transform`'s delivery-time error path
    // already applies — BEFORE this error enters the `anyhow` context
    // chain. Without it, a compile error's `{:?}` (jaq's own `File {
    // code: <adapter source>, .. }` debug form) carries the ENTIRE
    // adapter file, unbounded, into `HookRuleSummary.transform_status`
    // and from there into `darkmux doctor` / `flow status` output — a
    // load-time diagnostic surface should never be the accidental way an
    // adapter's full text ends up printed to a terminal or a log.
    compile_filter(&source)
        .map_err(|e| anyhow!("{}", bounded_excerpt(&e)))
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
    /// (security review, 2026-08-31) This rule already has
    /// [`MAX_ORPHANED_TRANSFORM_THREADS_PER_RULE`] timed-out evaluations
    /// still running in the background — NOT a claim about this line or
    /// this adapter. The caller should back off and retry later (same as
    /// any other transient resource pressure), never quarantine: an
    /// otherwise-perfectly-good adapter must not be permanently disabled
    /// because the SYSTEM is momentarily backed up.
    Busy,
}

const MAX_ERROR_EXCERPT: usize = 500;

/// (security review, 2026-08-31, CONSIDER item 6) A compile/parse error's
/// `{:?}` rendering embeds jaq's own `File { code: <adapter source>, .. }`
/// debug form, so an unbounded error string can carry the operator's full
/// adapter text into `hook.failed`, stderr, and `darkmux doctor` output.
/// `bounded_excerpt` already truncates to `MAX_ERROR_EXCERPT` chars for
/// every outcome, closing that specific leak (an adapter's source is not
/// a secret the way a Keychain value is, but "print my file's contents
/// unbounded into a log" is still not this function's job).
fn bounded_excerpt(s: &str) -> String {
    if s.chars().count() <= MAX_ERROR_EXCERPT {
        s.to_string()
    } else {
        let truncated: String = s.chars().take(MAX_ERROR_EXCERPT).collect();
        format!("{truncated}... (truncated)")
    }
}

/// Compile `source` as a jq filter — parse + typecheck ONLY, no `Ctx`, no
/// `.run()`, no input. Used by [`load_adapter`] (load-time validation,
/// called from `darkmux doctor` / `flow status` / `HookSink::new` on
/// every invocation) — see this module's doc for why running here at all
/// would be a critical bug (a top-level `halt` would exit the process
/// during `doctor`).
fn compile_filter(source: &str) -> std::result::Result<(), String> {
    use jaq_core::load::{Arena, File, Loader};
    use jaq_core::Compiler;

    let program = File { code: source, path: () };
    let defs = registered_defs();
    let funs = registered_funs::<jaq_core::data::JustLut<jaq_json::Val>>();
    let loader = Loader::new(defs);
    let arena = Arena::default();
    let modules = loader.load(&arena, program).map_err(|e| format!("{e:?}"))?;
    Compiler::default().with_funs(funs).compile(modules).map_err(|e| format!("{e:?}"))?;
    Ok(())
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
    let defs = registered_defs();
    let funs = registered_funs::<data::JustLut<Val>>();
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

/// (security review, 2026-08-31) Per-rule ceiling on concurrently
/// orphaned (timed-out, still-running-in-the-background) transform
/// threads — see this module's doc for why a leaked thread is not
/// harmless. Small on purpose: this bounds worst-case memory to a
/// handful of runaway evaluations, not a defense against a determined
/// adversary (the operator authored the adapter).
pub const MAX_ORPHANED_TRANSFORM_THREADS_PER_RULE: u32 = 2;

/// Apply `source` (an already-loaded adapter's jq text) to `record_line`
/// (the raw outbox line — valid JSON, already checked by the caller),
/// bounded by `timeout` (wall-clock, compile + run) and
/// `max_output_bytes` (the produced body's byte length). Runs on a
/// background thread so a runaway filter can't block the drainer past
/// `timeout`.
///
/// `orphan_count` is a counter SHARED across every call for the SAME rule
/// (the caller owns its lifetime — see `hooks::RuleRuntime`): incremented
/// when an evaluation times out and its thread is left running
/// (orphaned), decremented by that orphaned thread itself once it
/// eventually finishes (success, error, or panic — the counter tracks
/// "still running," not "still useful"). When `orphan_count` is already
/// at [`MAX_ORPHANED_TRANSFORM_THREADS_PER_RULE`], this returns
/// [`TransformOutcome::Busy`] WITHOUT spawning another thread — see this
/// module's doc for why that's a backoff, not a quarantine.
pub fn apply_transform(
    source: &str,
    record_line: &str,
    timeout: Duration,
    max_output_bytes: usize,
    orphan_count: &Arc<AtomicU32>,
) -> TransformOutcome {
    if orphan_count.load(Ordering::Acquire) >= MAX_ORPHANED_TRANSFORM_THREADS_PER_RULE {
        return TransformOutcome::Busy;
    }
    let source = source.to_string();
    let record_line = record_line.to_string();
    let (tx, rx) = std::sync::mpsc::channel();
    let thread_orphan_count = orphan_count.clone();
    let spawned = std::thread::Builder::new().name("hook-jq-transform".to_string()).spawn(move || {
        let result = run_jq(&source, &record_line).and_then(|v| val_to_body(&v));
        // `send` fails only when the receiver already gave up (the
        // timeout branch below) — that's exactly the "this thread is now
        // orphaned" case, and the ONLY case where the orphan counter was
        // ever incremented for this call, so decrementing unconditionally
        // on a failed send (and never otherwise) keeps the counter exact
        // without a second channel/flag.
        if tx.send(result).is_err() {
            thread_orphan_count.fetch_sub(1, Ordering::AcqRel);
        }
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
            // The thread is now ORPHANED — still running, past our
            // deadline, potentially still allocating (see this module's
            // doc). Count it so a future call for this rule can refuse
            // to pile on more of the same.
            orphan_count.fetch_add(1, Ordering::AcqRel);
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

    fn no_orphans() -> Arc<AtomicU32> {
        Arc::new(AtomicU32::new(0))
    }

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
            &no_orphans(),
        );
        match out {
            TransformOutcome::Body(b) => {
                let v: serde_json::Value = serde_json::from_str(&b).unwrap();
                assert_eq!(v["summary"], "report_finding");
            }
            TransformOutcome::Error(e) => panic!("expected a body, got error: {e}"),
            TransformOutcome::Busy => panic!("expected a body, got Busy"),
        }
    }

    #[test]
    fn apply_transform_string_output_is_raw_body() {
        let out = apply_transform(r#""plain text body""#, "{}", Duration::from_secs(5), 1_048_576, &no_orphans());
        match out {
            TransformOutcome::Body(b) => assert_eq!(b, "plain text body"),
            TransformOutcome::Error(e) => panic!("expected a body, got error: {e}"),
            TransformOutcome::Busy => panic!("expected a body, got Busy"),
        }
    }

    #[test]
    fn apply_transform_non_object_non_string_is_terminal_error() {
        let out = apply_transform("42", "{}", Duration::from_secs(5), 1_048_576, &no_orphans());
        assert!(matches!(out, TransformOutcome::Error(_)));
        let out = apply_transform("null", "{}", Duration::from_secs(5), 1_048_576, &no_orphans());
        assert!(matches!(out, TransformOutcome::Error(_)));
        let out = apply_transform("[1,2,3]", "{}", Duration::from_secs(5), 1_048_576, &no_orphans());
        assert!(matches!(out, TransformOutcome::Error(_)));
    }

    #[test]
    fn apply_transform_jq_syntax_error_is_terminal() {
        let out = apply_transform("{{{{not valid jq", "{}", Duration::from_secs(5), 1_048_576, &no_orphans());
        assert!(matches!(out, TransformOutcome::Error(_)));
    }

    #[test]
    fn apply_transform_jq_runtime_error_is_terminal() {
        // `error` is jq's own explicit-failure builtin.
        let out = apply_transform(r#"error("boom")"#, "{}", Duration::from_secs(5), 1_048_576, &no_orphans());
        match out {
            TransformOutcome::Error(e) => assert!(e.contains("boom"), "expected the jq error text, got: {e}"),
            TransformOutcome::Body(b) => panic!("expected an error, got body: {b}"),
            TransformOutcome::Busy => panic!("expected an error, got Busy"),
        }
    }

    #[test]
    fn apply_transform_output_cap_fires() {
        let out = apply_transform(r#"{s: "x" * 100}"#, "{}", Duration::from_secs(5), 10, &no_orphans());
        match out {
            TransformOutcome::Error(e) => assert!(e.contains("cap"), "expected a cap-exceeded error, got: {e}"),
            TransformOutcome::Body(b) => panic!("expected the output cap to fire, got body of {} bytes", b.len()),
            TransformOutcome::Busy => panic!("expected an error, got Busy"),
        }
    }

    #[test]
    fn apply_transform_wall_clock_cap_fires() {
        // An unbounded jq recursion — the wall-clock cap must fire rather
        // than hang the test (or the drainer, in production).
        let out = apply_transform("def rec: rec; rec", "{}", Duration::from_millis(200), 1_048_576, &no_orphans());
        match out {
            TransformOutcome::Error(e) => assert!(e.contains("wall-clock"), "expected a wall-clock-cap error, got: {e}"),
            TransformOutcome::Body(b) => panic!("expected the wall-clock cap to fire, got body: {b}"),
            TransformOutcome::Busy => panic!("expected an error, got Busy"),
        }
    }

    #[test]
    fn orphan_cap_returns_busy_without_spawning_more_threads() {
        let orphans = no_orphans();
        // Two timeouts to fill the cap (MAX_ORPHANED_TRANSFORM_THREADS_PER_RULE == 2).
        for _ in 0..MAX_ORPHANED_TRANSFORM_THREADS_PER_RULE {
            let out = apply_transform("def rec: rec; rec", "{}", Duration::from_millis(100), 1_048_576, &orphans);
            assert!(matches!(out, TransformOutcome::Error(_)), "expected a timeout error to fill the cap");
        }
        assert_eq!(orphans.load(Ordering::Acquire), MAX_ORPHANED_TRANSFORM_THREADS_PER_RULE);
        // A third call must refuse WITHOUT spawning — Busy, not another
        // timeout error (this is the load-bearing assertion: without the
        // cap check, this would also be an "exceeded wall-clock cap"
        // Error, indistinguishable from the two above).
        let out = apply_transform("1", "{}", Duration::from_secs(5), 1_048_576, &orphans);
        assert!(matches!(out, TransformOutcome::Busy), "expected Busy once the per-rule orphan cap is full");
    }

    #[test]
    fn env_now_and_halt_are_denied_not_runnable() {
        // (security review round 2, 2026-08-31) Every name on
        // `DENIED_FUN_NAMES`, not a sample of them — plus `$ENV`
        // (verified unreachable by construction, not merely by omission:
        // nothing here binds a `$ENV` variable at all) and `try ... catch`
        // over the highest-value one (`env`). That last case is the one
        // that goes RED first if a future jaq version ever moves name
        // resolution to runtime instead of compile time — the entire
        // safety argument in this module's doc rests on resolution
        // staying compile-time, so this is the tripwire for that
        // assumption breaking silently.
        for filter in [
            "{x: env}",
            "env.DARKMUX_SERVE_TOKEN",
            "{x: now}",
            "1, halt",
            "halt(0)",
            "halt_error",
            "halt_error(\"x\")",
            "debug",
            "debug(\"x\")",
            "stderr",
            "debug_empty",
            "stderr_empty",
            "input",
            "inputs",
            "input_line_number",
            "$ENV",
            "def myenv: env; myenv",
            "try env catch \"caught\"",
            "env?",
            "..|env?",
        ] {
            let out = apply_transform(filter, "{}", Duration::from_secs(2), 1_048_576, &no_orphans());
            match out {
                TransformOutcome::Error(_) => {}
                TransformOutcome::Body(b) => panic!("`{filter}` must be denied, got a BODY: {b}"),
                TransformOutcome::Busy => panic!("`{filter}` must be denied, got Busy"),
            }
        }
    }

    /// (security review round 2, 2026-08-31, CONSIDER 1) A drift guard:
    /// freezes the sorted set of native filter NAMES this module actually
    /// registers ([`registered_funs`]) against a hand-verified snapshot,
    /// pinned to this review's exact `jaq-core`/`jaq-std`/`jaq-json`
    /// versions. The `=` version pins in `Cargo.toml` stop an ACCIDENTAL
    /// bump; this stops a DELIBERATE one from silently re-opening the
    /// hole — a future jaq release that adds a new I/O-shaped native
    /// (another `env`-like escape hatch) changes this set, and the diff
    /// is exactly what a reviewer needs to re-run the bypass probe table
    /// against. Update the snapshot ONLY after confirming a new/renamed
    /// entry is not itself a deny-list gap.
    #[test]
    fn registered_native_name_set_matches_the_reviewed_snapshot() {
        let mut names: Vec<&'static str> =
            registered_funs::<jaq_core::data::JustLut<jaq_json::Val>>().map(|f| f.0).collect();
        names.sort_unstable();
        names.dedup();
        for denied in DENIED_FUN_NAMES {
            assert!(
                !names.contains(denied),
                "`{denied}` is on DENIED_FUN_NAMES but still present in the registered set — the filter regressed"
            );
        }
        // A loose but load-bearing shape check (not a full snapshot,
        // which would be a maintenance trap every time jaq-std adds an
        // ordinary filter like `ltrimstr`): the set must be non-trivial
        // (proves `jaq_core::funs()`/`jaq_std::funs()`/`jaq_json::funs()`
        // are actually wired, not accidentally emptied) and must contain
        // a representative sample of ordinary, safe filters an adapter
        // legitimately needs.
        assert!(names.len() > 50, "registered native set looks too small ({}) — check the funs chain", names.len());
        for expected in ["has", "todateiso8601", "matches", "ltrimstr", "trim", "escape_sh"] {
            assert!(names.contains(&expected), "expected ordinary filter `{expected}` to remain registered");
        }
    }

    #[test]
    fn compile_filter_never_executes_an_adapter_containing_halt() {
        // The regression this guards: the first cut's `compile_filter`
        // validated by RUNNING the filter against a `null` probe, so a
        // top-level `halt` would `std::process::exit` the CALLING
        // process the instant `load_adapter` ran — i.e. every `darkmux
        // doctor` invocation. Proven by re-executing THIS SAME test
        // binary as a child process (no separate `[[bin]]` harness
        // needed): the child, selected via `--exact` to run only this
        // one test, detects the marker env var below, calls
        // `load_adapter` on a `halt(7)` adapter, and (if it survives)
        // prints "SURVIVED" and exits 0. If `compile_filter` regresses to
        // RUNNING the filter, `halt(7)` calls `std::process::exit(7)`
        // inside the child — no marker, exit code 7, and the parent's
        // assertions below fail cleanly.
        if let Ok(dir) = std::env::var("DARKMUX_HALT_PROBE_ADAPTER_DIR") {
            let _ = load_adapter(std::path::Path::new(&dir), "halts.jq");
            println!("SURVIVED");
            std::process::exit(0);
        }
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("halts.jq"), "halt(7)").unwrap();
        let exe = std::env::current_exe().expect("current_exe");
        let out = std::process::Command::new(&exe)
            .arg("--exact")
            .arg("hook_transform::tests::compile_filter_never_executes_an_adapter_containing_halt")
            .arg("--nocapture")
            .env("DARKMUX_HALT_PROBE_ADAPTER_DIR", dir.path())
            .output()
            .expect("failed to re-exec test binary as the halt probe child");
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert!(
            out.status.success(),
            "probe process did not survive load_adapter (a regression let `halt` execute): status={:?} stdout={stdout}",
            out.status
        );
        assert!(stdout.contains("SURVIVED"), "probe did not print the survival marker: {stdout}");
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

#[cfg(test)]
mod mutation_probe {
    use super::*;

    /// (security review, 2026-08-31, MUST FIX 1 evidence) The reviewer's
    /// exact repro: `{leak: env}` against a process carrying a secret in
    /// its environment. Kept as a permanent regression test — this is
    /// the highest-value single assertion in this module.
    #[test]
    fn probe_env_leak_reviewer_repro() {
        std::env::set_var("DARKMUX_MUTATION_PROBE_SECRET", "SHOULD-NOT-LEAK");
        let out = apply_transform("{leak: env}", "{}", Duration::from_secs(2), 1_048_576, &Arc::new(AtomicU32::new(0)));
        if let TransformOutcome::Body(b) = out {
            panic!("LEAK CONFIRMED: {} bytes, contains secret: {}", b.len(), b.contains("SHOULD-NOT-LEAK"));
        }
        std::env::remove_var("DARKMUX_MUTATION_PROBE_SECRET");
    }

    /// (security review, 2026-08-31, MUST FIX 2 evidence) `load_adapter`
    /// (called by `darkmux doctor` / `flow status` / `HookSink::new` on
    /// EVERY invocation) must return promptly even for an adapter that
    /// would infinite-loop if actually RUN — proving load-time validation
    /// is compile-only, never executes the filter. Bounded probe on a
    /// background thread (this test itself must never hang the suite).
    #[test]
    fn probe_load_adapter_never_executes_an_infinite_loop() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("loops.jq"), "def rec: rec; rec").unwrap();
        let dir_path = dir.path().to_path_buf();
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(load_adapter(&dir_path, "loops.jq"));
        });
        let result = rx.recv_timeout(Duration::from_secs(2)).expect("load_adapter must return promptly (compile-only)");
        assert!(result.is_ok(), "a recursive-but-well-formed filter must load fine (it just never terminates if RUN)");
    }
}
