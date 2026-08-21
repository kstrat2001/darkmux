//! `GET /panel/:id` — allowlisted read-only CLI command views (#1569 packet B).
//!
//! The keystone mechanism of the CLI-panels epic (#1568): render a command's
//! own ANSI output in the viewer instead of re-implementing its logic in
//! JavaScript. The point is not saved code — it is that **twin-drift becomes
//! structurally impossible**: the viewer's missions board was a JS
//! re-implementation of `mission status` that had already diverged (#1561,
//! where the one row needing attention rendered as the one row with no
//! signal). A panel cannot diverge from the CLI because it IS the CLI.
//!
//! ## Invocation discipline
//!
//! - **Compile-time argv table, extended by a compile-time opts table
//!   (#1911).** The client sends an opaque id; the server maps it through
//!   [`panel_spec`]'s `match`. For panels that declare [`PanelSpec::opts`],
//!   the client ALSO sends `opt.<name>=<value>` selections — string keys
//!   used ONLY as lookups into that panel's own table, never a value that
//!   reaches argv directly (see "Opts", below). No shell, no PATH
//!   resolution (`std::env::current_exe()` — the daemon re-invokes its own
//!   binary), and **no client-supplied string ever reaches argv directly**:
//!   the only OTHER client-influenced value is a render width, clamped and
//!   passed as `COLUMNS` env, never argv.
//! - **Read-only allowlist, by doctrine.** Nothing that dispatches a model
//!   (observability paths contain zero model dispatches, #1286) and nothing
//!   that mutates. The worst case of a bug here is a wrong reading, never a
//!   wrong action. If the list grows past a handful it is becoming a new
//!   accretion surface — that is the signal to stop, not to add a config.
//! - **`doctor` is manual-run only.** It PROBES (spawns checks, touches
//!   `lms`, reads disk) — an auto-polling doctor panel open on the measured
//!   host during a canon run is the observer joining the observed (#1286).
//!   Its entry is marked `auto_refresh: false` and the viewer must honor it;
//!   the TTL of 0 means even an explicit re-request never serves stale.
//!
//! ## Opts: a declared option space, not an open one (#1911)
//!
//! A panel's [`PanelSpec::opts`] table is a SECOND, narrower allowlist
//! layered under the first: the id picks the base verb, and `opt.<name>`
//! query params pick among a CLOSED set of pre-declared argv fragments for
//! that verb. This replaced an earlier design where every variant got its
//! OWN allowlist id (`mission-status-all` used to be its own entry, argv
//! `["mission","status","--all"]`) — that shape cannot scale past a
//! boolean: `run list`'s four `--kind` values crossed with its `--all`
//! toggle would need EIGHT separate ids for one verb, defeating the
//! "deliberately short" allowlist this module's own guard tests enforce.
//!
//! The struct shape IS the legality proof (see [`PanelOpt`]/
//! [`PanelOptValue`]): there is no field that could hold a placeholder, a
//! format string, or a client-echoed value, so `--machine <id>` cannot be
//! written down at all. Only a `(name, value)` LOOKUP KEY crosses the wire;
//! the server owns every literal argv fragment it could ever resolve to.
//! This is the SAME trust mechanism the panel id itself already used, one
//! level down. An unknown name or an unknown value for a known name is a
//! 400 naming the legal set — never a pass-through, never silently ignored
//! (see [`resolve_opts`]).
//!
//! `mission-status-all` is kept as a one-release ACCEPTED ALIAS
//! (see [`resolve_alias`]) so the pre-#1911 client (a separate PR migrates
//! it to `opt.*`) keeps working unmodified: the id resolves to the
//! `mission-status` spec with its `all` opt forced to `"all"`, reproducing
//! the old entry's exact argv and landing in the exact same cache entry a
//! direct `opt.all=all` request would. It is deliberately kept OUT of
//! [`PANEL_IDS`] (it is no longer a base verb — its old argv would fail the
//! layer-2 flag guard below if it were one) and dropped entirely under the
//! pre-1.0 no-compat-baggage posture once the client migrates.
//!
//! ## Response shape
//!
//! `{ panel, argv, opts, captured_ts_ms, gather_ms, exit_code, ansi_text,
//!    cache_ttl_ms, age_ms, auto_refresh }` — metadata AROUND the text,
//! never extraction FROM it (the moment the server parses the output, the
//! twin-drift this exists to kill is reborn server-side). `opts` echoes the
//! RESOLVED selection for every declared opt, including defaults (#1911) —
//! `{"kind":"mission","all":"recent"}` — so the artifact stays
//! self-describing even when nothing was picked explicitly. `gather_ms`
//! stamps the observer's own cost into the payload (#1286 constraint 3),
//! and `cache_ttl_ms`/`age_ms` make the staleness story verifiable rather
//! than assumed (constraint 4).
//!
//! ## Caching + single-flight
//!
//! A per-VARIANT TTL cache bounds the cost of an enthusiastic client to one
//! spawn per [`PANEL_CACHE_TTL`] per SELECTION, not per panel id (#1911):
//! the key is the canonical [`variant_key`] — spec id plus sorted
//! non-default `name=value` pairs, built AFTER opt validation so only legal
//! combinations ever become keys. Because a default value's argv is always
//! empty and canonicalization drops defaults, "no selection" and
//! "explicitly picked the default" are byte-identical requests and land in
//! ONE cache entry. A per-variant single-flight lock collapses concurrent
//! misses for the same selection into one spawn. Cache entries are
//! whole-response; `age_ms` reports how stale a served entry is.
//!
//! **The MANUAL-RUN floor (`MANUAL_MIN_INTERVAL`/`last_manual`) stays keyed
//! by BASE id, never by variant** — see [`check_manual_floor`]'s own doc.
//! Otherwise a manual panel that also declared options could be re-run
//! continuously by cycling `opt.*` values, defeating the floor precisely
//! where it matters most (a probing panel, not a disk read). `doctor`
//! declares no options today, so this is a structural guarantee (the
//! floor-check function's signature has no parameter a variant key could
//! even arrive through) rather than an observed behavior yet.
//!
//! **`cols` is deliberately NOT part of the cache key.** Two clients at
//! different widths share one entry for up to the TTL, so the second sees
//! the first's width — self-describing (the body carries the `cols` it was
//! rendered at) and bounded at 3s. Keying on width would multiply spawns
//! for a difference nobody notices in a 3-second window.
//!
//! **The spawn timeout kills the CHILD, not its descendants.** `kill_on_drop`
//! SIGKILLs the direct child; a grandchild it left running (doctor's `curl`,
//! `machine status`'s `lms`) survives until its own bound fires. Every verb
//! on today's allowlist bounds its own subprocesses, so this is latent — but
//! any addition to [`panel_spec`] must re-check that it still holds.

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use std::collections::HashMap;
use std::process::Stdio;
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::{current_millis, AppState};

/// TTL for cached panel output. Short: panels are "state right now" views,
/// and the underlying commands are cheap disk reads — the cache exists to
/// bound a polling client, not to make data old.
pub(crate) const PANEL_CACHE_TTL: Duration = Duration::from_millis(3_000);

/// Hard wall-clock bound on one panel spawn. These are fast read-only CLI
/// verbs (disk + local probes); anything hitting this bound is wedged, and a
/// wedged child must never wedge the daemon route (#1570/#1573's class —
/// the same week this module was written, the crate sweep found two more
/// instances of unbounded external calls; this one is born bounded).
const PANEL_SPAWN_TIMEOUT: Duration = Duration::from_secs(10);

/// Required on every `/panel/*` request. Its ONLY job is to be a
/// non-simple header, which forces the browser to send a CORS **preflight**
/// — and `local_only_cors()` allows no custom request headers, so a foreign
/// origin's preflight fails and the real request is never sent (#1602 gate).
///
/// Without it, `/panel/:id` is a "simple request": any page open in the
/// operator's browser could `fetch("http://127.0.0.1:8765/panel/doctor")` on
/// a loop from a background tab. CORS would stop it READING the response but
/// not SENDING it, and the daemon would faithfully run doctor — spawning
/// probes, shelling to `curl` against the GitHub API, touching the Keychain
/// — over and over on the measured host. That is precisely the #1286
/// "observer joins the observed" failure, driven by a page the operator
/// never opened on purpose.
///
/// Bearer auth does not cover this: loopback is exempt by design, and under
/// the documented `tailscale serve` phone-dashboard pattern every tailnet
/// peer arrives as loopback too.
pub(crate) const PANEL_HEADER: &str = "x-darkmux-panel";

/// Server-enforced floor between runs of a MANUAL-ONLY panel (TTL 0).
///
/// `auto_refresh: false` is advice to the viewer, and the module doc used to
/// admit as much — "the viewer must honor it". A buggy viewer build, a stuck
/// tab, or any non-browser client defeats advice. This floor is the
/// enforcement: doctor costs ~2.2s of probing per run, so an unbounded
/// caller could pin the measured host indefinitely. A human clicking
/// "re-run" does not notice 30s; a loop is bounded to twice a minute and
/// told so with `Retry-After`.
const MANUAL_MIN_INTERVAL: Duration = Duration::from_secs(30);

/// One legal value of a [`PanelOpt`] and the literal argv fragment it
/// contributes when selected.
///
/// `argv` is always either empty (the default — see [`PanelOpt::values`])
/// or flag-shaped (its first token starts with `-`), pinned by
/// `every_opt_value_argv_is_flag_shaped`. There is no field here that could
/// hold a client-supplied string, a format placeholder, or an operator ID —
/// only a fixed, server-authored literal.
pub(crate) struct PanelOptValue {
    /// The wire value a client selects by (`"mission"`, `"all"`).
    pub(crate) value: &'static str,
    /// The literal argv fragment this value contributes, appended to
    /// `spec.argv` in the opt's declaration position.
    pub(crate) argv: &'static [&'static str],
}

/// One declared, closed option group for a panel — e.g. `--kind` on
/// `run-list` (#1911). The struct shape IS the legality proof: it has no
/// field that could hold a placeholder, a format string, or a
/// client-echoed value, so `--machine <id>` cannot be written down here at
/// all. A client transmits a `(name, value)` pair used ONLY as a lookup key
/// into this table; the argv tokens appended are the table's OWN literals.
pub(crate) struct PanelOpt {
    /// Query-param key (`opt.<name>`) AND the opts-bar group label. By
    /// convention this matches the flag it drives ("kind", "all").
    pub(crate) name: &'static str,
    /// Legal values. `values[0]` IS the default and MUST carry an empty
    /// argv fragment, so "default selected" and "nothing selected" are
    /// byte-identical requests: one cache entry, one canonical key. Pinned
    /// by `every_default_value_has_empty_argv`.
    pub(crate) values: &'static [PanelOptValue],
}

/// The `--all` toggle shared by `mission-status` and `run-list`: default
/// `"recent"` contributes no argv (today's already-capped rendering),
/// `"all"` contributes the literal `--all` flag.
const ALL_OPT: PanelOpt = PanelOpt {
    name: "all",
    values: &[
        PanelOptValue { value: "recent", argv: &[] },
        PanelOptValue { value: "all", argv: &["--all"] },
    ],
};

/// `run list`'s `--kind` filter. Values are deliberately the same strings
/// as `RunKindArg` (`src/cli.rs`) and `RUNS_KINDS`
/// (`ui/src/lib/route.ts`) — pinned against the Rust side by
/// `run_list::run_kind_arg_vocabulary_matches_the_ui_runs_kinds_twin`
/// (`src/run_list.rs`).
const RUN_LIST_KIND_OPT: PanelOpt = PanelOpt {
    name: "kind",
    values: &[
        PanelOptValue { value: "all", argv: &[] },
        PanelOptValue { value: "mission", argv: &["--kind", "mission"] },
        PanelOptValue { value: "dispatch", argv: &["--kind", "dispatch"] },
        PanelOptValue { value: "lab", argv: &["--kind", "lab"] },
    ],
};

const MISSION_STATUS_OPTS: &[PanelOpt] = &[ALL_OPT];
const RUN_LIST_OPTS: &[PanelOpt] = &[RUN_LIST_KIND_OPT, ALL_OPT];

/// One allowlist entry: the argv after the binary, whether the viewer may
/// auto-refresh it, the cache TTL applied, and the closed option space (if
/// any) it declares.
pub(crate) struct PanelSpec {
    /// The canonical id — carried HERE rather than derived by a second
    /// lookup. An earlier draft had `panel_spec()` plus a `panel_spec_key()`
    /// twin plus a hardcoded list in the tests: three copies of one table,
    /// where adding an 8th panel and forgetting the twin panicked the
    /// handler on first request and no test could catch the drift. One
    /// match, one table (#1602 gate).
    pub(crate) id: &'static str,
    pub(crate) argv: &'static [&'static str],
    pub(crate) auto_refresh: bool,
    pub(crate) cache_ttl: Duration,
    /// Declared option space (#1911) — empty for verbs with no legal
    /// variants. See the module doc, "Opts: a declared option space, not
    /// an open one".
    pub(crate) opts: &'static [PanelOpt],
}

/// Every allowlisted BASE panel id (#1911: this counts base verbs, not
/// variants — a verb with declared opts is still one id here). Test-only
/// by design: the production source of truth is `panel_spec`'s single
/// match (that is the whole point of collapsing the old three-table
/// shape), and this exists so the drift guard can assert against an
/// INDEPENDENT list rather than deriving its expectations from the thing
/// under test. Promote it to a real constant when a consumer needs it —
/// B3's panel picker likely will.
#[cfg(test)]
pub(crate) const PANEL_IDS: &[&str] = &[
    "mission-status",
    "role-list",
    "machine-status",
    "config-list",
    "flow-status",
    "lab-fixture-list",
    "run-list",
    "doctor",
];

/// The allowlist. Deliberately short — see the module doc. Ids are kebab-case
/// and OPAQUE to the client; the mapping to argv (and opts) lives here and
/// only here. `mission-status-all` is NOT a base verb here — see
/// [`resolve_alias`].
pub(crate) fn panel_spec(id: &str) -> Option<PanelSpec> {
    let (id, argv, auto_refresh, ttl, opts): (
        &'static str,
        &'static [&'static str],
        bool,
        Duration,
        &'static [PanelOpt],
    ) = match id {
        "mission-status" => {
            ("mission-status", &["mission", "status"], true, PANEL_CACHE_TTL, MISSION_STATUS_OPTS)
        }
        "role-list" => ("role-list", &["role", "list"], true, PANEL_CACHE_TTL, &[]),
        "machine-status" => ("machine-status", &["machine", "status"], true, PANEL_CACHE_TTL, &[]),
        "config-list" => ("config-list", &["config", "list"], true, PANEL_CACHE_TTL, &[]),
        "flow-status" => ("flow-status", &["flow", "status"], true, PANEL_CACHE_TTL, &[]),
        "lab-fixture-list" => {
            ("lab-fixture-list", &["lab", "fixture", "list"], true, PANEL_CACHE_TTL, &[])
        }
        // (#1911) The CLI twin of the RUNS lens's union — see
        // `src/run_list.rs`'s own module doc.
        "run-list" => ("run-list", &["run", "list"], true, PANEL_CACHE_TTL, RUN_LIST_OPTS),
        // Manual-run only (#1286): never auto-refreshed by the viewer,
        // TTL 0 so an explicit re-run is always a real run, and rate-
        // floored server-side (see MANUAL_MIN_INTERVAL) because
        // "the viewer must honor it" is not enforcement.
        "doctor" => ("doctor", &["doctor"], false, Duration::ZERO, &[]),
        _ => return None,
    };
    Some(PanelSpec { id, argv, auto_refresh, cache_ttl: ttl, opts })
}

/// One-release compatibility alias (#1911): the pre-opts client still
/// requests `mission-status-all` by id (it has not yet migrated to
/// `opt.all=all` — that migration is a separate, client-side PR). Resolves
/// to the BASE id `panel_spec` should be looked up under, plus the forced
/// `(name, value)` selections that reproduce the old entry's argv exactly.
/// Anything else passes straight through with no forced opts.
///
/// Kept deliberately separate from `panel_spec`'s own match: the alias is
/// NOT a base verb (it is absent from [`PANEL_IDS`], and its old argv
/// `["mission","status","--all"]` would fail the layer-2 flag guard if
/// entered directly), so it must never be reachable by looking `panel_spec`
/// up under its own name.
fn resolve_alias(id: &str) -> (&str, &'static [(&'static str, &'static str)]) {
    match id {
        "mission-status-all" => ("mission-status", &[("all", "all")]),
        other => (other, &[]),
    }
}

/// One resolved `(name, value)` opt selection, in [`PanelSpec::opts`]'
/// DECLARATION order — never query-string order — with its argv fragment
/// and whether it is the opt's default. Built exclusively by
/// [`resolve_opts`].
#[derive(Debug)]
struct ResolvedOpt {
    name: &'static str,
    value: &'static str,
    argv: &'static [&'static str],
    is_default: bool,
}

/// Validate and resolve a request's `opt.<name>` selections against
/// `spec`'s declared table (#1911). `requested` holds ONLY the already
/// `opt.`-stripped names (the caller sees `kind`, not `opt.kind`) mapped to
/// their raw string values — this function never touches header/alias
/// plumbing, so it stays a pure function a test can drive directly.
///
/// An unknown name, or a known name with an unknown value, is an `Err`
/// naming the legal set — never a pass-through, never silently ignored.
/// The `Ok` vector is always in DECLARATION order (`spec.opts`'s own
/// order) and always has exactly `spec.opts.len()` entries — one per
/// declared opt, defaulted when the client did not select it — so argv
/// composition, the cache key, and the response echo can never disagree
/// about which opts exist.
fn resolve_opts(spec: &PanelSpec, requested: &HashMap<String, String>) -> Result<Vec<ResolvedOpt>, String> {
    let legal_names: Vec<&'static str> = spec.opts.iter().map(|o| o.name).collect();
    for name in requested.keys() {
        if !spec.opts.iter().any(|o| o.name == name.as_str()) {
            return Err(format!(
                "unknown option \"{name}\" for panel \"{}\" — legal options: {}\n",
                spec.id,
                if legal_names.is_empty() { "(none)".to_string() } else { legal_names.join(", ") }
            ));
        }
    }

    let mut out = Vec::with_capacity(spec.opts.len());
    for opt in spec.opts {
        let default_value = opt.values[0].value;
        let selected: &str = requested.get(opt.name).map(String::as_str).unwrap_or(default_value);
        let Some(pv) = opt.values.iter().find(|v| v.value == selected) else {
            let legal: Vec<&'static str> = opt.values.iter().map(|v| v.value).collect();
            return Err(format!(
                "unknown value \"{selected}\" for option \"{}\" on panel \"{}\" — legal values: {}\n",
                opt.name,
                spec.id,
                legal.join(", ")
            ));
        };
        out.push(ResolvedOpt { name: opt.name, value: pv.value, argv: pv.argv, is_default: pv.value == default_value });
    }
    Ok(out)
}

/// Compose the final argv: `spec.argv` followed by each resolved opt's
/// fragment, walked in `resolved`'s own order — which [`resolve_opts`]
/// guarantees is DECLARATION order, never query-string order. A default's
/// fragment is empty, so it contributes nothing.
fn compose_argv(spec: &PanelSpec, resolved: &[ResolvedOpt]) -> Vec<&'static str> {
    let mut out: Vec<&'static str> = spec.argv.to_vec();
    for r in resolved {
        out.extend_from_slice(r.argv);
    }
    out
}

/// The canonical cache / single-flight key (#1911): `spec_id` plus sorted
/// NON-DEFAULT `name=value` pairs, built AFTER [`resolve_opts`] has already
/// validated the request — so only legal combinations ever become keys,
/// and the key space is exactly the bounded cross-product
/// `variant_cross_product_stays_bounded` guards. Because a default's argv
/// is empty and this function drops defaults entirely, "no selection" and
/// "explicitly picked the default" produce the SAME key — one cache entry.
fn variant_key(spec_id: &str, resolved: &[ResolvedOpt]) -> String {
    let mut pairs: Vec<(&str, &str)> =
        resolved.iter().filter(|r| !r.is_default).map(|r| (r.name, r.value)).collect();
    pairs.sort_by_key(|(name, _)| *name);
    if pairs.is_empty() {
        spec_id.to_string()
    } else {
        let joined = pairs.iter().map(|(n, v)| format!("{n}={v}")).collect::<Vec<_>>().join("&");
        format!("{spec_id}?{joined}")
    }
}

/// The response's `"opts"` echo (#1911): every declared opt's resolved
/// value, INCLUDING defaults, so the artifact stays self-describing even
/// when nothing was picked explicitly. Empty object for a panel with no
/// declared opts.
fn opts_json(resolved: &[ResolvedOpt]) -> serde_json::Value {
    let map: serde_json::Map<String, serde_json::Value> =
        resolved.iter().map(|r| (r.name.to_string(), serde_json::Value::String(r.value.to_string()))).collect();
    serde_json::Value::Object(map)
}

/// Pull `opt.<name>=<value>` pairs out of the full raw query map, stripping
/// the `opt.` prefix so [`resolve_opts`] sees bare names. Anything not
/// prefixed with `opt.` (`cols`, or an unrelated param) is ignored here —
/// `cols` is read separately by the caller.
fn extract_opt_params(raw: &HashMap<String, String>) -> HashMap<String, String> {
    raw.iter().filter_map(|(k, v)| k.strip_prefix("opt.").map(|name| (name.to_string(), v.clone()))).collect()
}

/// Whole-response cache entry.
struct CacheEntry {
    body: serde_json::Value,
    captured: Instant,
}

/// Panel state carried on [`AppState`]: the TTL cache plus the per-variant
/// single-flight locks, both keyed by the canonical [`variant_key`]
/// (#1911 — previously keyed by the bare panel id, back when a panel could
/// have no variants at all).
/// One variant's single-flight lock — concurrent cache misses for the same
/// selection queue on this instead of each spawning a child.
type FlightLock = Arc<tokio::sync::Mutex<()>>;

#[derive(Clone, Default)]
pub(crate) struct PanelState {
    cache: Arc<tokio::sync::Mutex<HashMap<String, CacheEntry>>>,
    flights: Arc<tokio::sync::Mutex<HashMap<String, FlightLock>>>,
    /// Last completed run of each MANUAL-ONLY panel — the floor's clock.
    /// Keyed by BASE id (`spec.id`), deliberately never by variant — see
    /// [`check_manual_floor`]. Manual panels are uncached by design, so
    /// this is the only record that one ran at all.
    last_manual: Arc<tokio::sync::Mutex<HashMap<&'static str, Instant>>>,
}

fn clamp_cols(cols: Option<u16>) -> u16 {
    // (#1613) Floor is 36, not 60. A 390px phone fits ~52 columns at the
    // panel's font; the old 60 floor meant the viewer ASKED for 60, the CLI
    // faithfully rendered 60, and eight columns hung off the right edge — so
    // the operator scrolled sideways to see a mission's progress. The CLI
    // adapts correctly all the way down (measured: exact fits at 40, 44 and
    // 50), so the floor was defending against nothing and costing the
    // narrowest, most-used surface.
    cols.unwrap_or(100).clamp(36, 200)
}

/// The manual-run floor's enforcement (#1286, #1911). Deliberately takes
/// only `id: &'static str` — the panel's BASE id straight off the spec, per
/// `spec.id` — never a variant key: there is no parameter here through
/// which a variant selection could even arrive, so cycling a manual
/// panel's `opt.*` values (should one ever declare options) cannot defeat
/// `MANUAL_MIN_INTERVAL` by construction, not by convention. `doctor`
/// declares no options today, so no live manual panel exercises the
/// distinction yet — the signature itself is the guarantee.
async fn check_manual_floor(panels: &PanelState, id: &'static str) -> Result<(), (StatusCode, String)> {
    let last = panels.last_manual.lock().await;
    if let Some(prev) = last.get(id) {
        let since = prev.elapsed();
        if since < MANUAL_MIN_INTERVAL {
            let wait = (MANUAL_MIN_INTERVAL - since).as_secs() + 1;
            return Err((
                StatusCode::TOO_MANY_REQUESTS,
                format!(
                    "panel \"{id}\" is manual-run only and ran {}s ago — it probes the \
                     machine, so it is floored at {}s between runs. Retry-After: {wait}\n",
                    since.as_secs(),
                    MANUAL_MIN_INTERVAL.as_secs()
                ),
            ));
        }
    }
    Ok(())
}

pub(crate) async fn panel_handler(
    Path(id): Path<String>,
    Query(params): Query<HashMap<String, String>>,
    headers: axum::http::HeaderMap,
    State(state): State<AppState>,
) -> Result<axum::Json<serde_json::Value>, (StatusCode, String)> {
    // The preflight forcer — see PANEL_HEADER. Checked BEFORE the allowlist
    // lookup so a drive-by never even learns which ids exist.
    if !headers.contains_key(PANEL_HEADER) {
        return Err((
            StatusCode::FORBIDDEN,
            format!(
                "panel requests require the `{PANEL_HEADER}` header — it forces a CORS \
                 preflight so a foreign page cannot drive this endpoint from the \
                 operator's browser\n"
            ),
        ));
    }

    // Alias resolution BEFORE the allowlist lookup (#1911): a
    // `mission-status-all` request resolves to the `mission-status` spec
    // with its `all` opt forced — see `resolve_alias`'s own doc. Any other
    // id passes through unchanged.
    let (base_id, forced_opts) = resolve_alias(&id);
    let Some(spec) = panel_spec(base_id) else {
        return Err((
            StatusCode::NOT_FOUND,
            format!("unknown panel \"{id}\" — panels are a fixed allowlist, not arbitrary commands\n"),
        ));
    };
    // Canonical &'static id straight off the spec — one table, no second
    // lookup that could drift out from under it.
    let id: &'static str = spec.id;

    // `opt.<name>` query params, with the alias's forced overrides applied
    // LAST so they always win — the alias's whole point is a fixed,
    // non-negotiable selection regardless of what a stray query param says.
    let mut requested = extract_opt_params(&params);
    for (name, value) in forced_opts {
        requested.insert((*name).to_string(), (*value).to_string());
    }

    let resolved = resolve_opts(&spec, &requested).map_err(|msg| (StatusCode::BAD_REQUEST, msg))?;
    let final_argv = compose_argv(&spec, &resolved);
    let key = variant_key(id, &resolved);

    let cols = clamp_cols(params.get("cols").and_then(|v| v.parse::<u16>().ok()));

    // Manual-only panels (TTL 0) are floored server-side, keyed by BASE id
    // — see `check_manual_floor`'s own doc for why that must never be the
    // variant key.
    if !spec.auto_refresh {
        check_manual_floor(&state.panels, id).await?;
    }

    // Serve fresh-enough cache without spawning.
    if let Some(body) = cached_if_fresh(&state.panels, &key, spec.cache_ttl).await {
        return Ok(axum::Json(body));
    }

    // Single-flight: collapse concurrent misses for the same VARIANT into
    // one spawn.
    let flight = {
        let mut flights = state.panels.flights.lock().await;
        flights.entry(key.clone()).or_insert_with(|| Arc::new(tokio::sync::Mutex::new(()))).clone()
    };
    let _guard = flight.lock().await;
    // Re-check under the flight lock — a concurrent request may have filled
    // the cache while this one waited.
    if let Some(body) = cached_if_fresh(&state.panels, &key, spec.cache_ttl).await {
        return Ok(axum::Json(body));
    }

    let started = Instant::now();
    let exe = std::env::current_exe().map_err(|e| {
        (StatusCode::INTERNAL_SERVER_ERROR, format!("resolving current_exe: {e}\n"))
    })?;
    let mut cmd = tokio::process::Command::new(exe);
    cmd.args(&final_argv)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        // Styled output on a pipe is the whole point (see style.rs's
        // CLICOLOR_FORCE tier). NO_COLOR is removed for the CHILD only: an
        // operator's NO_COLOR governs their terminal, and a panel is not
        // their terminal — leaving it set would silently blank every panel.
        .env("CLICOLOR_FORCE", "1")
        .env_remove("NO_COLOR")
        .env("COLUMNS", cols.to_string())
        // The child is told WHICH panel it is rendering into, so a verb can
        // make its own hints actionable here without the viewer having to
        // pattern-match its output (that matching IS the twin drift this
        // endpoint exists to kill). Opt-in by construction: a verb that
        // ignores this env var behaves exactly as it does in a terminal.
        .env("DARKMUX_PANEL", id)
        // A child must never inherit the daemon's own serve lifecycle env in
        // a way that could confuse it; everything else (DARKMUX_HOME, dirs)
        // is deliberately inherited — the panel must see the same state the
        // operator's own shell would.
        .kill_on_drop(true);

    let output = tokio::time::timeout(PANEL_SPAWN_TIMEOUT, cmd.output())
        .await
        .map_err(|_| {
            (
                StatusCode::GATEWAY_TIMEOUT,
                format!(
                    "panel \"{id}\" timed out after {}s — the CLI verb is wedged; \
                     the daemon killed it (kill_on_drop)\n",
                    PANEL_SPAWN_TIMEOUT.as_secs()
                ),
            )
        })?
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("spawning panel \"{id}\": {e}\n")))?;

    let gather_ms = started.elapsed().as_millis() as u64;
    let ansi_text = String::from_utf8_lossy(&output.stdout).into_owned();
    let stderr_tail: String = String::from_utf8_lossy(&output.stderr)
        .lines()
        .rev()
        .take(5)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect::<Vec<_>>()
        .join("\n");

    let body = serde_json::json!({
        "panel": id,
        "argv": final_argv,
        "opts": opts_json(&resolved),
        "captured_ts_ms": current_millis(),
        "gather_ms": gather_ms,
        "exit_code": output.status.code(),
        "ansi_text": ansi_text,
        // Non-empty only when something went to stderr — surfaced so a
        // failing verb is diagnosable from the panel itself, not just logs.
        "stderr_tail": stderr_tail,
        "cols": cols,
        "cache_ttl_ms": spec.cache_ttl.as_millis() as u64,
        "age_ms": 0,
        "auto_refresh": spec.auto_refresh,
    });

    if spec.cache_ttl.is_zero() {
        // Manual panel: nothing cached (an explicit run is a real run), but
        // the floor's clock advances so the next caller is bounded. Keyed
        // by BASE id — see `check_manual_floor`'s own doc.
        state.panels.last_manual.lock().await.insert(id, Instant::now());
    } else {
        let mut cache = state.panels.cache.lock().await;
        cache.insert(key, CacheEntry { body: body.clone(), captured: Instant::now() });
    }
    Ok(axum::Json(body))
}

/// Serve the cached body if it is within `ttl`, with `age_ms` restamped so
/// the client can SEE it got a cached copy (#1286 constraint 4 — cadence and
/// staleness are recorded knobs, never silent).
async fn cached_if_fresh(panels: &PanelState, key: &str, ttl: Duration) -> Option<serde_json::Value> {
    if ttl.is_zero() {
        return None;
    }
    let cache = panels.cache.lock().await;
    let entry = cache.get(key)?;
    let age = entry.captured.elapsed();
    if age > ttl {
        return None;
    }
    let mut body = entry.body.clone();
    body["age_ms"] = serde_json::json!(age.as_millis() as u64);
    Some(body)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allowlist_is_closed_and_argv_is_fixed() {
        // The table maps ids to argv; anything else is a 404, never a spawn.
        assert!(panel_spec("mission-status").is_some());
        assert!(panel_spec("doctor").is_some());
        assert!(panel_spec("rm -rf /").is_none());
        assert!(panel_spec("mission status").is_none(), "argv-looking ids are not ids");
        assert!(panel_spec("").is_none());
    }

    /// The drift guard the three-parallel-tables shape could not have: the
    /// ONE list and the ONE match must agree, in both directions.
    #[test]
    fn every_listed_id_has_a_spec_and_reports_itself() {
        for id in PANEL_IDS {
            let spec = panel_spec(id).unwrap_or_else(|| panic!("PANEL_IDS lists {id} with no spec"));
            assert_eq!(spec.id, *id, "a spec must report the id it was looked up by");
            assert!(!spec.argv.is_empty(), "{id} has empty argv");
        }
        // 8 as of `run-list` (#1911). Bumping this number is the doctrine
        // decision: an entry is legal only if it neither dispatches a model
        // (#1286) nor mutates, so the worst case of a bug here stays a wrong
        // READING. This now counts 8 BASE VERBS, not 8 id/argv combinations
        // (#1911) — a verb's VARIANTS (its declared `opts`) grow the option
        // space, not this list; see `variant_cross_product_stays_bounded`
        // for that guard instead. If growth ever comes from wanting
        // operator-supplied VALUES with no closed set (`--machine <id>`,
        // `--since <when>`), stop — an open value space is not an
        // allowlist, and that surface is a lens, not a panel.
        assert_eq!(PANEL_IDS.len(), 8, "allowlist growth is a doctrine decision, not a drive-by");
    }

    /// Layer 2 of the three-layer guard (#1911): pills are always base
    /// verbs, mechanically. A flag baked into a spec's BASE argv means the
    /// old per-variant-id shape crept back in — `mission-status-all`'s own
    /// argv (`["mission","status","--all"]`) would trip this if it were
    /// ever re-added as a direct entry, which is exactly why it is now
    /// resolved only through `resolve_alias`, never through `panel_spec`'s
    /// own match.
    #[test]
    fn no_panel_bakes_a_flag_into_its_base_argv() {
        for id in PANEL_IDS {
            let spec = panel_spec(id).unwrap();
            assert!(
                spec.argv.iter().all(|t| !t.starts_with('-')),
                "{id} bakes a flag into its argv — flags are opts, ids are verbs"
            );
        }
    }

    #[test]
    fn doctor_is_manual_only_and_uncached() {
        let d = panel_spec("doctor").unwrap();
        assert!(!d.auto_refresh, "doctor probes — auto-polling it is the observer joining the observed");
        assert!(d.cache_ttl.is_zero(), "an explicit doctor run must be a real run");
        // …and every other panel IS auto-refreshable with a real TTL.
        for id in
            ["mission-status", "role-list", "machine-status", "config-list", "flow-status", "lab-fixture-list", "run-list"]
        {
            let s = panel_spec(id).unwrap();
            assert!(s.auto_refresh, "{id}");
            assert_eq!(s.cache_ttl, PANEL_CACHE_TTL, "{id}");
        }
    }

    #[test]
    fn cols_clamped_hard() {
        assert_eq!(clamp_cols(None), 100);
        assert_eq!(clamp_cols(Some(10)), 36);
        assert_eq!(clamp_cols(Some(5000)), 200);
        assert_eq!(clamp_cols(Some(120)), 120);
        // (#1613) A phone's real ask must survive the clamp unchanged. 390px
        // fits ~52 columns; the old floor of 60 rounded it UP, which is how a
        // narrow screen ended up rendering wider than itself.
        assert_eq!(clamp_cols(Some(52)), 52, "a phone's width is not a floor violation");
        assert_eq!(clamp_cols(Some(46)), 46);
    }

    // ── opts data-shape guards (#1911) ───────────────────────────────

    /// Every declared opt's DEFAULT (`values[0]`) must carry empty argv —
    /// see `PanelOpt::values`' own doc for why: it is what makes "no
    /// selection" and "explicitly picked the default" the same request.
    #[test]
    fn every_default_value_has_empty_argv() {
        for id in PANEL_IDS {
            let spec = panel_spec(id).unwrap();
            for opt in spec.opts {
                assert!(
                    opt.values[0].argv.is_empty(),
                    "{id}'s opt \"{}\" default value \"{}\" must carry empty argv",
                    opt.name,
                    opt.values[0].value
                );
            }
        }
    }

    /// Every non-default opt value's argv must be flag-shaped (its first
    /// token starts with `-`) — a bare value with no flag would be
    /// unrepresentable in a real invocation and is the kind of mistake the
    /// struct shape is supposed to make impossible to state cleanly.
    #[test]
    fn every_opt_value_argv_is_flag_shaped() {
        for id in PANEL_IDS {
            let spec = panel_spec(id).unwrap();
            for opt in spec.opts {
                for v in opt.values {
                    if v.argv.is_empty() {
                        continue;
                    }
                    assert!(
                        v.argv[0].starts_with('-'),
                        "{id}'s opt \"{}\" value \"{}\" has non-flag-shaped argv: {:?}",
                        opt.name,
                        v.value,
                        v.argv
                    );
                }
            }
        }
    }

    /// The cache-growth guard #1911 calls for: a bound on the TOTAL variant
    /// cross-product, not just the base-verb count layer 3 already guards.
    /// Today: `mission-status` (2) + `run-list` (4×2=8) + six no-opt panels
    /// (1 each) = 16.
    #[test]
    fn variant_cross_product_stays_bounded() {
        let mut total = 0usize;
        for id in PANEL_IDS {
            let spec = panel_spec(id).unwrap();
            let variants: usize = spec.opts.iter().map(|o| o.values.len()).product::<usize>().max(1);
            total += variants;
        }
        assert!(
            total <= 24,
            "variant cross-product grew to {total} — bumping the bound is a doctrine \
             decision (#1911), not a drive-by"
        );
    }

    // ── resolve_opts: the 400-on-unknown-option/value guard (#1911) ──

    #[test]
    fn resolve_opts_defaults_every_declared_opt_when_nothing_requested() {
        let spec = panel_spec("run-list").unwrap();
        let requested = HashMap::new();
        let resolved = resolve_opts(&spec, &requested).unwrap();
        assert_eq!(resolved.len(), 2, "one resolved entry per DECLARED opt, defaulted");
        assert_eq!(resolved[0].name, "kind");
        assert_eq!(resolved[0].value, "all");
        assert!(resolved[0].argv.is_empty());
        assert!(resolved[0].is_default);
        assert_eq!(resolved[1].name, "all");
        assert_eq!(resolved[1].value, "recent");
        assert!(resolved[1].argv.is_empty());
        assert!(resolved[1].is_default);
    }

    #[test]
    fn resolve_opts_picks_the_named_value() {
        let spec = panel_spec("run-list").unwrap();
        let mut requested = HashMap::new();
        requested.insert("kind".to_string(), "lab".to_string());
        let resolved = resolve_opts(&spec, &requested).unwrap();
        assert_eq!(resolved[0].value, "lab");
        assert_eq!(resolved[0].argv, &["--kind", "lab"]);
        assert!(!resolved[0].is_default);
    }

    /// The 400-on-unknown-option case named explicitly by the task: a name
    /// not in the panel's own table must fail closed, naming the legal set
    /// — never pass through to argv, never get silently dropped.
    #[test]
    fn resolve_opts_rejects_an_unknown_name_naming_the_legal_set() {
        let spec = panel_spec("run-list").unwrap();
        let mut requested = HashMap::new();
        requested.insert("machine".to_string(), "studio".to_string());
        let err = resolve_opts(&spec, &requested).unwrap_err();
        assert!(err.contains("unknown option"), "{err}");
        assert!(err.contains("machine"), "{err}");
        assert!(err.contains("kind"), "must name the legal set: {err}");
        assert!(err.contains("all"), "must name the legal set: {err}");
    }

    /// The value-side twin: a known option name with a value outside its
    /// closed set is likewise a 400, never a pass-through.
    #[test]
    fn resolve_opts_rejects_an_unknown_value_naming_the_legal_set() {
        let spec = panel_spec("run-list").unwrap();
        let mut requested = HashMap::new();
        requested.insert("kind".to_string(), "bogus".to_string());
        let err = resolve_opts(&spec, &requested).unwrap_err();
        assert!(err.contains("unknown value"), "{err}");
        assert!(err.contains("bogus"), "{err}");
        assert!(err.contains("all"), "must list every legal value: {err}");
        assert!(err.contains("mission"), "{err}");
        assert!(err.contains("dispatch"), "{err}");
        assert!(err.contains("lab"), "{err}");
    }

    /// A panel with NO declared opts rejects any `opt.*` at all — an open
    /// value space is never tolerated by silently ignoring it.
    #[test]
    fn resolve_opts_on_a_no_opts_panel_rejects_any_opt_name() {
        let spec = panel_spec("doctor").unwrap();
        let mut requested = HashMap::new();
        requested.insert("kind".to_string(), "mission".to_string());
        let err = resolve_opts(&spec, &requested).unwrap_err();
        assert!(err.contains("unknown option"), "{err}");
        assert!(err.contains("(none)"), "a no-opts panel's legal set is empty: {err}");
    }

    // ── declaration-order argv (#1911) ────────────────────────────────

    /// The task's named case: a `HashMap` has no reliable iteration order,
    /// so this feeds `all` before `kind` into the request map and asserts
    /// the OUTPUT still matches `spec.opts`' declared order (kind, then
    /// all) — never whatever order the map happened to iterate them in.
    #[test]
    fn compose_argv_is_declaration_order_never_query_order() {
        let spec = panel_spec("run-list").unwrap();
        let mut requested = HashMap::new();
        requested.insert("all".to_string(), "all".to_string());
        requested.insert("kind".to_string(), "mission".to_string());
        let resolved = resolve_opts(&spec, &requested).unwrap();
        let argv = compose_argv(&spec, &resolved);
        assert_eq!(
            argv,
            vec!["run", "list", "--kind", "mission", "--all"],
            "argv must follow declaration order (kind, then all), not query-string order: {argv:?}"
        );
    }

    // ── variant_key: cache-key canonicalization (#1911) ───────────────

    #[test]
    fn variant_key_is_base_id_alone_when_everything_is_default() {
        let spec = panel_spec("run-list").unwrap();
        let resolved = resolve_opts(&spec, &HashMap::new()).unwrap();
        assert_eq!(variant_key(spec.id, &resolved), "run-list");
    }

    /// "No selection" and "explicitly picked the default" must be ONE
    /// cache entry — the task's named case.
    #[test]
    fn variant_key_matches_regardless_of_whether_the_default_was_explicit() {
        let spec = panel_spec("run-list").unwrap();
        let nothing = resolve_opts(&spec, &HashMap::new()).unwrap();
        let mut explicit = HashMap::new();
        explicit.insert("kind".to_string(), "all".to_string());
        explicit.insert("all".to_string(), "recent".to_string());
        let picked_default = resolve_opts(&spec, &explicit).unwrap();
        assert_eq!(
            variant_key(spec.id, &nothing),
            variant_key(spec.id, &picked_default),
            "no selection and explicitly picking the default must be ONE cache entry"
        );
    }

    #[test]
    fn variant_key_sorts_non_default_pairs_by_name() {
        let spec = panel_spec("run-list").unwrap();
        let mut requested = HashMap::new();
        requested.insert("all".to_string(), "all".to_string());
        requested.insert("kind".to_string(), "lab".to_string());
        let resolved = resolve_opts(&spec, &requested).unwrap();
        assert_eq!(variant_key(spec.id, &resolved), "run-list?all=all&kind=lab");
    }

    #[test]
    fn variant_key_differs_for_different_selections() {
        let spec = panel_spec("run-list").unwrap();
        let mut a = HashMap::new();
        a.insert("kind".to_string(), "mission".to_string());
        let mut b = HashMap::new();
        b.insert("kind".to_string(), "dispatch".to_string());
        let ra = resolve_opts(&spec, &a).unwrap();
        let rb = resolve_opts(&spec, &b).unwrap();
        assert_ne!(variant_key(spec.id, &ra), variant_key(spec.id, &rb));
    }

    // ── opts_json: the response echo (#1911) ──────────────────────────

    #[test]
    fn opts_json_echoes_every_declared_opt_including_defaults() {
        let spec = panel_spec("run-list").unwrap();
        let resolved = resolve_opts(&spec, &HashMap::new()).unwrap();
        let json = opts_json(&resolved);
        assert_eq!(json, serde_json::json!({"kind": "all", "all": "recent"}));
    }

    #[test]
    fn opts_json_is_empty_object_for_a_no_opts_panel() {
        let spec = panel_spec("doctor").unwrap();
        let resolved = resolve_opts(&spec, &HashMap::new()).unwrap();
        assert_eq!(opts_json(&resolved), serde_json::json!({}));
    }

    // ── mission-status-all alias fold (#1911) ─────────────────────────

    #[test]
    fn mission_status_all_is_no_longer_a_base_spec() {
        assert!(
            panel_spec("mission-status-all").is_none(),
            "mission-status-all folded into mission-status's `all` opt (#1911) — it must \
             not resolve as a base verb any more"
        );
    }

    #[test]
    fn mission_status_all_alias_resolves_to_mission_status_with_all_forced() {
        let (base_id, forced) = resolve_alias("mission-status-all");
        assert_eq!(base_id, "mission-status");
        assert_eq!(forced, &[("all", "all")]);

        let spec = panel_spec(base_id).unwrap();
        let mut requested: HashMap<String, String> = HashMap::new();
        for (name, value) in forced {
            requested.insert((*name).to_string(), (*value).to_string());
        }
        let resolved = resolve_opts(&spec, &requested).unwrap();
        let argv = compose_argv(&spec, &resolved);
        assert_eq!(
            argv,
            vec!["mission", "status", "--all"],
            "the alias must reproduce the OLD entry's exact argv"
        );
        // …and it lands in the exact same cache entry a direct
        // `opt.all=all` request against `mission-status` would.
        assert_eq!(variant_key(spec.id, &resolved), "mission-status?all=all");
    }

    #[test]
    fn a_normal_id_is_unaffected_by_alias_resolution() {
        let (base_id, forced) = resolve_alias("run-list");
        assert_eq!(base_id, "run-list");
        assert!(forced.is_empty());
    }

    // ── manual-run floor: keyed by base id, never by variant (#1911) ──

    #[tokio::test]
    async fn manual_floor_is_open_before_any_run_is_recorded() {
        let panels = PanelState::default();
        assert!(check_manual_floor(&panels, "doctor").await.is_ok());
    }

    #[tokio::test]
    async fn manual_floor_fires_on_the_second_call_within_the_window() {
        let panels = PanelState::default();
        panels.last_manual.lock().await.insert("doctor", Instant::now());
        let err = check_manual_floor(&panels, "doctor").await.unwrap_err();
        assert_eq!(err.0, StatusCode::TOO_MANY_REQUESTS);
        assert!(err.1.contains("floored"), "{}", err.1);
    }

    #[tokio::test]
    async fn manual_floor_is_independent_per_base_id() {
        let panels = PanelState::default();
        panels.last_manual.lock().await.insert("doctor", Instant::now());
        // A DIFFERENT base id must not be floored by doctor's own run.
        assert!(check_manual_floor(&panels, "some-other-manual-panel").await.is_ok());
    }

    /// The property #1911 names explicitly: the floor check's own
    /// signature only ever receives the panel's BASE id (`spec.id`) — see
    /// `check_manual_floor`'s doc. There is no variant-key parameter for a
    /// caller to pass, structurally, not by convention: cycling any
    /// `opt.*` selection on a (hypothetical, since `doctor` has none today)
    /// manual panel with options cannot produce a different key to the
    /// floor than the base id it always uses. Simulated here: once ANY run
    /// of `doctor` is recorded under its base id, EVERY subsequent check
    /// against that same base id floors, regardless of what a caller might
    /// have "meant" by a different selection — because no selection ever
    /// reaches this function at all.
    #[tokio::test]
    async fn manual_floor_cannot_be_defeated_by_cycling_opt_values() {
        let panels = PanelState::default();
        panels.last_manual.lock().await.insert("doctor", Instant::now());
        for _ in 0..3 {
            let err = check_manual_floor(&panels, "doctor").await.unwrap_err();
            assert_eq!(
                err.0,
                StatusCode::TOO_MANY_REQUESTS,
                "repeated checks against the SAME base id must stay floored — there is no \
                 variant key in scope that could reset it"
            );
        }
    }
}
