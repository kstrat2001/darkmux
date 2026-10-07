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
//! format string, or a client-echoed value, so a machine id cannot be
//! written down in a table at all (the exception is the roster-valued opt
//! below, one on `profile-list` and one on `machine-status`). Only a `(name, value)` LOOKUP KEY crosses the wire;
//! the server owns every literal argv fragment it could ever resolve to.
//! This is the SAME trust mechanism the panel id itself already used, one
//! level down. An unknown name or an unknown value for a known name is a
//! 400 naming the legal set — never a pass-through, never silently ignored
//! (see [`resolve_opts`]).
//!
//! ### The roster-valued opt (`profile-list` and `machine-status`)
//!
//! `profile-list` and `machine-status` declare [`PanelSpec::roster_opt`]:
//! `opt.machine=<name>` drives `--machine <name>` on the first and the
//! positional id (after a `--`, so no id can read as a flag) on the second.
//! This is the exception to "no client string reaches argv", and it is still
//! a closed set: the legal values are the names
//! the fleet view lists (`FleetView::selector_names`: every roster id and this
//! machine's own name, the set the CLI's `--machine` accepts), and the value is
//! accepted only by exact (case-insensitive) membership. The argv gets the
//! view's own spelling of the name, never the client's bytes, so the client
//! chooses among server-owned strings exactly as it does for every static opt.
//! A name the view does not list is a 400 naming the legal names, like any
//! unknown value.
//!
//! There is no `mission-status-all`: the unlimited board is `mission-status`
//! with `opt.all=all`, and a request for the old id is an unknown panel (404).
//!
//! ## What a remote caller is shown
//!
//! Every panel is served to every caller the read posture admits; none is
//! refused for being remote (5.0, operator decision 2026-10-07: a console is a
//! command line, its output names this machine's own facts, and the full
//! output is for this machine, or for whoever runs the CLI there over ssh). A
//! caller that is neither this machine nor a token holder is served the
//! panel REDACTED, in two layers (see `darkmux_types::panel_audience`):
//!
//! - **The verb renders its remote form.** The child is spawned with
//!   `DARKMUX_PANEL_AUDIENCE=remote`, and a verb whose output carries the
//!   execution surface shapes its own data before printing it: `config list`
//!   withholds every value that names an address, a path, a URL, a credential
//!   pointer or the fleet listener and allow-list; `doctor` still runs every
//!   check and keeps every row and remedy, but its fleet listener, identity
//!   and allow-list rows' detail is withheld; `flow status` withholds its
//!   directories, the Redis URL and each hook's target; `lab fixture list`
//!   withholds each fixture's path. The remote form is cached apart from this
//!   machine's (see [`audience_key`]).
//! - **The daemon redacts the text, whatever the panel** ([`redact_for_remote`]),
//!   stdout and `stderr_tail` alike (stderr is redacted, not dropped, so a
//!   failed panel still says why): every roster address (and its host part)
//!   reads "(address hidden)", the daemon user's home prefix reads `~` (a
//!   `DARKMUX_HOME` outside it reads `$DARKMUX_HOME`), and every value in
//!   [`crate::panel_withheld`] (every address, path, endpoint URL and
//!   credential pointer this machine is configured with) reads "(shown on
//!   this machine only)". Matches are whole tokens only (stdout is split into
//!   escape sequences and text runs first, each run is redacted on its own,
//!   and an OSC 8 link to a hidden or withheld target loses its target), and a
//!   machine, profile or endpoint name is never hidden, only the address
//!   behind it.
//!
//! `doctor`'s manual-run floor is kept per [`Audience`]: a remote caller can
//! neither close this machine's window nor make it probe more than once per
//! window, because inside the remote window it is served the last remote run.
//!
//! Where anything was withheld, the response's `withheld` field carries ONE
//! plain notice naming the command and this machine
//! (`darkmux_types::panel_audience::notice`); the viewer shows it calmly, never
//! as an error. Local callers and token holders see the output unchanged and
//! `withheld` empty. Fleet work submission is untouched by any of this: it
//! still needs the token plus a network-verified sender.
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
//! by BASE id, never by variant** — see [`admit_manual_run`]'s own doc.
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
use std::time::{Duration, Instant, SystemTime};

use crate::redaction::{redact_panel_stdout, Redaction};
use darkmux_types::panel_audience::{self, Withheld};
use crate::wire::PanelResponse;
use crate::{current_millis, AppState};

/// TTL for cached panel output. Short: panels are "state right now" views,
/// and the underlying commands are cheap disk reads — the cache exists to
/// bound a polling client, not to make data old.
pub(crate) const PANEL_CACHE_TTL: Duration = Duration::from_millis(3_000);

/// TTL for the panels that read the fleet stream (`run-list`,
/// `mission-status`). Their cold spawn measures about 5s (the fleet read plus,
/// before the link base went root-relative, a `tailscale` probe), so a 3s TTL
/// expired before one spawn finished and every poll spawned again. The TTL is
/// kept above one cold spawn instead of serving stale while refreshing: one
/// rule, no second code path, and the body's `age_ms` still says how old it is.
pub(crate) const FLEET_PANEL_CACHE_TTL: Duration = Duration::from_millis(8_000);

/// Wall-clock bound on one panel spawn, for the fast read-only verbs (disk +
/// local probes). Hitting it means the verb was slow, and a slow child must
/// never wedge the daemon route (#1570/#1573's class: this is born bounded).
/// The bound is per panel ([`PanelSpec::spawn_timeout`]): `doctor` probes
/// every peer and needs more.
const PANEL_SPAWN_TIMEOUT: Duration = Duration::from_secs(10);

/// `doctor`'s spawn bound. Measured on this machine (release build, 2026-10-02):
/// 5.6s with no peers, 7.1s with three black-holed peers in the roster. The
/// bound is over three times the slower figure so a run that is merely slow
/// finishes instead of timing out into a 504. It is kept BELOW
/// [`MANUAL_MIN_INTERVAL`] on purpose: at or above it the floor would already
/// be open when the timeout fires and [`release_manual_run`] would do nothing
/// (pinned by `doctor_times_out_inside_the_manual_floor`).
const DOCTOR_SPAWN_TIMEOUT: Duration = Duration::from_secs(25);

/// Stderr lines kept in `stderr_tail` (see [`stderr_tail`]).
const STDERR_TAIL_LINES: usize = 8;

/// Hard cap on the stdout a panel returns. A verb printing more is cut at a
/// line boundary and the response says so in its own text; the cap keeps one
/// runaway verb from becoming a multi-megabyte JSON body per poll.
const PANEL_STDOUT_CAP_BYTES: usize = 512 * 1024;

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
/// Bearer auth does not cover this: a request from this machine
/// (`is_local_request`) is exempt by design, and with read auth off every
/// tailnet peer behind the documented `tailscale serve` phone dashboard
/// reads every panel too (redacted, see the module doc).
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
/// client-echoed value, so a machine id cannot be written down here at
/// all (the roster-valued opts are [`PanelSpec::roster_opt`], validated
/// against the roster, not a table). A client transmits a `(name, value)` pair used ONLY as a lookup key
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

/// `run list`'s `--kind` filter. Its values are the SAME four strings as
/// `RunKindArg` (`src/cli.rs`) and `RUNS_KINDS` (`ui/src/lib/route.ts`),
/// and all three are pinned to each other by
/// `run_list::run_kind_arg_vocabulary_matches_the_ui_runs_kinds_twin`
/// (`src/run_list.rs`), which text-scans THIS constant as its third leg.
///
/// The pin is load-bearing, not tidiness. Drop `Lab` from `RunKindArg` and
/// without it this table keeps offering `--kind lab`; the panel spawns a
/// flag clap no longer accepts, and the operator gets an empty body with
/// `exit_code: 2` — a wrong reading with no failing test anywhere. The
/// reverse (a fifth kind) silently makes that kind unreachable from the
/// console.
const RUN_LIST_KIND_OPT: PanelOpt = PanelOpt {
    name: "kind",
    values: &[
        PanelOptValue { value: "all", argv: &[] },
        PanelOptValue { value: "mission", argv: &["--kind", "mission"] },
        PanelOptValue { value: "dispatch", argv: &["--kind", "dispatch"] },
        PanelOptValue { value: "lab", argv: &["--kind", "lab"] },
    ],
};

/// (#2902 step 2b) `run list`'s `--usage` toggle: default `off` contributes
/// no argv, `on` contributes the literal flag, so the token breakdown by
/// endpoint and model appears under the table. Same shape as [`ALL_OPT`]
/// (`isBooleanFlagToggle` on the client renders it as one `[--usage]`
/// token). The client twin is `RUN_LIST_USAGE_OPT` in
/// `ui/src/lenses/console/panels.ts`, pinned by that file's drift guard.
const RUN_LIST_USAGE_OPT: PanelOpt = PanelOpt {
    name: "usage",
    values: &[
        PanelOptValue { value: "off", argv: &[] },
        PanelOptValue { value: "on", argv: &["--usage"] },
    ],
};

const MISSION_STATUS_OPTS: &[PanelOpt] = &[ALL_OPT];
const RUN_LIST_OPTS: &[PanelOpt] = &[RUN_LIST_KIND_OPT, ALL_OPT, RUN_LIST_USAGE_OPT];

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
    /// Wall-clock bound on this panel's spawn; see [`PANEL_SPAWN_TIMEOUT`].
    pub(crate) spawn_timeout: Duration,
    /// Declared option space (#1911) — empty for verbs with no legal
    /// variants. See the module doc, "Opts: a declared option space, not
    /// an open one".
    pub(crate) opts: &'static [PanelOpt],
    /// (#1914, widened #1711) Whether this panel's spawn should hand its
    /// child a fleet-snapshot handoff file (see
    /// `crate::write_fleet_snapshot_file`, `crate::FLEET_SNAPSHOT_ENV_VAR`)
    /// instead of letting it pay its own full Redis round trip. `run-list`
    /// and `mission-status` are the two panels that read the network today
    /// (every other entry is a local-disk read) — see the module doc's
    /// "every panel until now reads local disk only".
    pub(crate) needs_fleet_snapshot: bool,
    /// The name of this panel's roster-valued opt, if it declares one: its
    /// value is a roster machine name, validated by [`resolve_roster_opt`]
    /// against the roster and reaching argv as [`RosterOpt::flag`]. See the
    /// module doc.
    pub(crate) roster_opt: Option<RosterOpt>,
}

/// A panel's roster-valued opt: the query name, and how the chosen machine
/// reaches argv. `profile list` takes it as a flag; `machine status` takes it
/// as its positional id (after `--`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct RosterOpt {
    pub(crate) name: &'static str,
    /// The flag that carries the machine, or `None` for a positional argument.
    pub(crate) flag: Option<&'static str>,
}

/// `profile list`'s `--remote` toggle: every peer's profiles this machine may
/// use. Default `off` is the local list. Conflicts with the roster `machine`
/// opt, which [`resolve_roster_opt`] refuses.
const PROFILE_LIST_REMOTE_OPT: PanelOpt = PanelOpt {
    name: "remote",
    values: &[
        PanelOptValue { value: "off", argv: &[] },
        PanelOptValue { value: "on", argv: &["--remote"] },
    ],
};

const PROFILE_LIST_OPTS: &[PanelOpt] = &[PROFILE_LIST_REMOTE_OPT];

/// The roster-valued opt's query name, shared by every panel that has one.
const ROSTER_MACHINE_OPT: &str = "machine";
const ROSTER_MACHINE_FLAG: &str = "--machine";
/// Ends option parsing before a positional roster id.
const POSITIONAL_SEPARATOR: &str = "--";

/// `profile list --machine <name>`.
const ROSTER_AS_FLAG: RosterOpt = RosterOpt { name: ROSTER_MACHINE_OPT, flag: Some(ROSTER_MACHINE_FLAG) };
/// `machine status -- <name>`.
const ROSTER_AS_POSITIONAL: RosterOpt = RosterOpt { name: ROSTER_MACHINE_OPT, flag: None };

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
    "machine-list",
    "config-list",
    "flow-status",
    "lab-fixture-list",
    "run-list",
    "profile-list",
    "doctor",
];

/// The allowlist. Deliberately short — see the module doc. Ids are kebab-case
/// and OPAQUE to the client; the mapping to argv (and opts) lives here and
/// only here.
pub(crate) fn panel_spec(id: &str) -> Option<PanelSpec> {
    let (id, argv, auto_refresh, ttl, opts): (
        &'static str,
        &'static [&'static str],
        bool,
        Duration,
        &'static [PanelOpt],
    ) = match id {
        // Every panel follows the read posture and is redacted for a caller
        // that is not this machine (see the module doc, "What a remote caller
        // is shown"). The notes say what each prints that a remote form
        // withholds.
        //
        // Mission and run state, the same facts `/runs` serves.
        "mission-status" => ("mission-status", &["mission", "status"], true, FLEET_PANEL_CACHE_TTL, MISSION_STATUS_OPTS),
        // The role manifests the crew loads; no machine or fleet state.
        "role-list" => ("role-list", &["role", "list"], true, PANEL_CACHE_TTL, &[]),
        // Resident models and their ownership, the same facts
        // `/machine/status` serves; an `lms` error names the configured
        // `lms_bin` path (withheld by the daemon's scrub).
        "machine-status" => ("machine-status", &["machine", "status"], true, PANEL_CACHE_TTL, &[]),
        // The fleet view's rows, the same facts `GET /fleet/view` serves
        // (liveness, the card each machine states about itself), worded as the
        // fleet lens's status line words them. A phone has no tooltips, so this
        // is the touch path for a machine's status and its reason. Names the
        // roster file's path.
        "machine-list" => ("machine-list", &["machine", "list"], true, PANEL_CACHE_TTL, &[]),
        // The whole config.json, which holds addresses, paths, URLs,
        // credential pointers, the listener's port and the `fleet.accept_work`
        // allow-list: its remote form withholds all of them.
        "config-list" => ("config-list", &["config", "list"], true, PANEL_CACHE_TTL, &[]),
        // The flows and outbox directories, each hook's target URL and the
        // Redis URL (an overlay address): its remote form withholds them.
        "flow-status" => ("flow-status", &["flow", "status"], true, PANEL_CACHE_TTL, &[]),
        // Registered lab fixtures and their paths.
        "lab-fixture-list" => ("lab-fixture-list", &["lab", "fixture", "list"], true, PANEL_CACHE_TTL, &[]),
        // (#1911) The CLI twin of the RUNS lens's union — see
        // `src/run_list.rs`'s own module doc. The same rows as `/runs`.
        "run-list" => ("run-list", &["run", "list"], true, FLEET_PANEL_CACHE_TTL, RUN_LIST_OPTS),
        // The profiles THIS machine's own grants let it use (its own
        // registry, or a roster peer's card), the same facts `machine list`
        // prints; it reveals only this machine's own allow-list entry on each
        // peer, not the peer's allow-list. An endpoint's host is withheld by
        // the daemon's scrub.
        "profile-list" => ("profile-list", &["profile", "list"], true, PANEL_CACHE_TTL, PROFILE_LIST_OPTS),
        // Manual-run only (#1286): never auto-refreshed by the viewer,
        // TTL 0 so an explicit re-run is always a real run, and rate-
        // floored server-side (see MANUAL_MIN_INTERVAL) because
        // "the viewer must honor it" is not enforcement. Its remote form runs
        // every check and keeps every row and remedy, and withholds the fleet
        // listener's address, port and busy policy, the identity row's node,
        // and the allow-list's entries.
        "doctor" => ("doctor", &["doctor"], false, Duration::ZERO, &[]),
        _ => return None,
    };
    // (#1914, widened #1711) Derived from the SAME `id` just matched above,
    // not a second table. `run-list` was the one panel that read the
    // network (see the module doc's "every panel until now reads local
    // disk only" and `PanelSpec::needs_fleet_snapshot`'s own doc) until
    // `mission-status` joined it: #1711 made the mission board read the
    // shared flow stream too (`darkmux_serve::fleet_records_for_runs()`,
    // the SAME call `run-list` makes), so its 3s-auto-refreshing panel
    // spawn would otherwise pay its own live Redis round trip on every
    // refresh — exactly the defect #1914 fixed for `run-list`, reintroduced
    // for its sibling. A future panel that reads the fleet stream must flip
    // this deliberately too — see `only_these_ids_need_a_fleet_snapshot`.
    let needs_fleet_snapshot = matches!(id, "run-list" | "mission-status");
    let roster_opt = match id {
        "profile-list" => Some(ROSTER_AS_FLAG),
        "machine-status" => Some(ROSTER_AS_POSITIONAL),
        _ => None,
    };
    Some(PanelSpec { id, argv, auto_refresh, cache_ttl: ttl, spawn_timeout: spawn_timeout_for(id), opts, needs_fleet_snapshot, roster_opt })
}

/// The spawn bound for panel `id`: `doctor` probes every peer and gets the long
/// one, every other verb is a fast local read.
fn spawn_timeout_for(id: &str) -> Duration {
    if id == "doctor" {
        DOCTOR_SPAWN_TIMEOUT
    } else {
        PANEL_SPAWN_TIMEOUT
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
                "unknown option \"{name}\" for panel \"{}\": legal options: {}\n",
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
                "unknown value \"{selected}\" for option \"{}\" on panel \"{}\": legal values: {}\n",
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
fn opts_map(resolved: &[ResolvedOpt]) -> std::collections::BTreeMap<String, String> {
    resolved.iter().map(|r| (r.name.to_string(), r.value.to_string())).collect()
}

/// Take the roster-valued opt (if `spec` declares one) out of `requested` and
/// check it against `roster_ids` (#3045). Returns the roster's own spelling of
/// the chosen id, or `None` when no machine was chosen (absent or empty).
///
/// Removing it from `requested` is what keeps [`resolve_opts`] unchanged: that
/// function still sees only static opts, and still rejects any name a panel
/// does not declare. A panel with no roster opt leaves `machine` in place, so
/// `resolve_opts` refuses it as unknown. `remote_on` is whether the static
/// `remote` toggle is set: a machine and `--remote` together are the same
/// conflict the CLI refuses.
fn resolve_roster_opt(
    spec: &PanelSpec,
    requested: &mut HashMap<String, String>,
    roster_ids: &[String],
    remote_on: bool,
) -> Result<Option<String>, String> {
    let Some(RosterOpt { name, .. }) = spec.roster_opt else { return Ok(None) };
    let Some(raw) = requested.remove(name) else { return Ok(None) };
    if raw.is_empty() {
        return Ok(None);
    }
    let Some(id) = roster_ids.iter().find(|id| id.eq_ignore_ascii_case(&raw)) else {
        return Err(format!(
            "unknown value \"{raw}\" for option \"{name}\" on panel \"{}\": legal values: {}\n",
            spec.id,
            if roster_ids.is_empty() { "(the roster is empty)".to_string() } else { roster_ids.join(", ") }
        ));
    };
    // (5.0 console review) A roster id reaches argv, so it must be a machine
    // name even when the roster holds an older, looser spelling (a hand-edited
    // `--all`): the one rule `machine add` enforces, applied again here.
    if let Some(problem) = darkmux_types::profile_address::machine_name_problem(id) {
        return Err(format!(
            "roster id {id:?} is not a legal machine name and is not offered on panel \"{}\": {problem}\n",
            spec.id
        ));
    }
    if remote_on {
        return Err(format!(
            "options \"{name}\" and \"remote\" cannot be combined on panel \"{}\": pick one machine or every peer\n",
            spec.id
        ));
    }
    Ok(Some(id.clone()))
}

/// The panel's argv, cache key and opts echo with the chosen roster machine
/// folded in. The key keeps [`variant_key`]'s `id?name=value&…` shape, so the
/// machine is just one more non-default pair.
fn with_roster_choice(
    spec: &PanelSpec,
    argv: Vec<&'static str>,
    key: String,
    mut echo: std::collections::BTreeMap<String, String>,
    machine: Option<&str>,
) -> (Vec<String>, String, std::collections::BTreeMap<String, String>) {
    let mut argv: Vec<String> = argv.into_iter().map(String::from).collect();
    let (Some(id), Some(roster)) = (machine, spec.roster_opt) else { return (argv, key, echo) };
    match roster.flag {
        Some(flag) => argv.push(flag.to_string()),
        // A positional id follows `--`, so nothing in the roster can be read
        // as a flag (5.0 console review).
        None => argv.push(POSITIONAL_SEPARATOR.to_string()),
    }
    argv.push(id.to_string());
    let joiner = if key.contains('?') { '&' } else { '?' };
    echo.insert(ROSTER_MACHINE_OPT.to_string(), id.to_string());
    (argv, format!("{key}{joiner}{ROSTER_MACHINE_OPT}={id}"), echo)
}

/// The first key that appears twice in a raw query string, percent-decoded
/// (`opt%2Ekind` and `opt.kind` are the same key), or `None`.
fn duplicate_query_key(raw: &str) -> Option<String> {
    let mut seen = std::collections::HashSet::new();
    raw.split('&').filter(|pair| !pair.is_empty()).map(|pair| decode_query_component(pair.split('=').next().unwrap_or(""))).find(|key| !seen.insert(key.clone()))
}

/// `application/x-www-form-urlencoded` decoding of one component: `+` is a
/// space and `%XX` is a byte; a malformed escape stays literal. Lossy UTF-8,
/// which is enough for comparing keys.
fn decode_query_component(raw: &str) -> String {
    let hex = |b: u8| (b as char).to_digit(16).map(|d| d as u8);
    let bytes = raw.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let escaped = match (bytes[i], bytes.get(i + 1).and_then(|b| hex(*b)), bytes.get(i + 2).and_then(|b| hex(*b))) {
            (b'%', Some(hi), Some(lo)) => Some(hi * 16 + lo),
            _ => None,
        };
        match (bytes[i], escaped) {
            (_, Some(byte)) => {
                out.push(byte);
                i += 2;
            }
            (b'+', None) => out.push(b' '),
            (b, None) => out.push(b),
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// The `cols` param, lenient on read (#1911) — see the call site's comment
/// for why this direction differs from `opt.*`'s fail-closed one. Split
/// out so the leniency is pinnable by `cols_is_lenient_on_read` without
/// driving a 200 through the handler, which would spawn `current_exe()`
/// (the test harness itself, under `cargo test`).
fn parse_cols(raw: &HashMap<String, String>) -> Option<u16> {
    raw.get("cols").and_then(|v| v.parse::<u16>().ok())
}

/// Pull `opt.<name>=<value>` pairs out of the full raw query map, stripping
/// the `opt.` prefix so [`resolve_opts`] sees bare names. Anything not
/// prefixed with `opt.` (`cols`, or an unrelated param) is ignored here —
/// `cols` is read separately by the caller.
fn extract_opt_params(raw: &HashMap<String, String>) -> HashMap<String, String> {
    raw.iter().filter_map(|(k, v)| k.strip_prefix("opt.").map(|name| (name.to_string(), v.clone()))).collect()
}

/// Whole-response cache entry.
///
/// (#2479 audit — not in the original MUST FIX list, found during the
/// audit enumeration and fixed for the same reason) `SystemTime`,
/// deliberately not `Instant`: `captured`'s ONLY purpose is the
/// `age_ms`/freshness pair restamped into the served body (this module's
/// own doc, above `cached_if_fresh`) — the OUTSIDE-WORLD bucket, same
/// shape as the fleet cache in `darkmux-serve/src/lib.rs`. A daemon that
/// survives a host sleep would otherwise serve a pre-sleep-cached panel
/// (mission-status, doctor, etc.) within `PANEL_CACHE_TTL` of AWAKE time
/// after wake, restamping a falsely small `age_ms` that claims the body
/// is fresh when it may be hours old.
struct CacheEntry {
    body: PanelResponse,
    captured: SystemTime,
}

/// Whether a [`CacheEntry`] captured at `captured` is still fresh at
/// `now`, given `ttl` (#2479 audit). Explicit `now` — no internal
/// `SystemTime::now()` call — for the same testability reason as
/// `manual_floor_wait`/`wall_clock_cache_is_fresh`. A backward wall-clock jump
/// (`duration_since` errs) is treated as NOT fresh: the safe failure
/// direction for a freshness gate is one extra (cheap, local) spawn, never
/// serving a body whose real age is unknown as though it were fresh.
fn cache_entry_is_fresh(captured: SystemTime, now: SystemTime, ttl: Duration) -> bool {
    now.duration_since(captured).map(|age| age <= ttl).unwrap_or(false)
}

/// Wall-clock milliseconds between `captured` and `now`, for the `age_ms`
/// restamped into a served cached body (#2479 audit). Mirrors
/// `wall_clock_age_ms_at` in `lib.rs`: a backward jump reports maximally
/// stale (`u64::MAX`) rather than `0`, so a clock anomaly never
/// under-reports how old a served body actually is. In practice this arm
/// is unreachable from `cached_if_fresh` (which already refuses to serve
/// on the same backward-jump condition), but the function stays correct
/// on its own terms rather than relying on that caller's behavior.
fn cache_entry_age_ms_at(captured: SystemTime, now: SystemTime) -> u64 {
    now.duration_since(captured).map(|d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX)).unwrap_or(u64::MAX)
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
    /// Test-only: run this program instead of the daemon's own binary, so a
    /// test can drive the real spawn path with a child whose output it knows.
    #[cfg(test)]
    child_exe: Option<std::path::PathBuf>,
    cache: Arc<tokio::sync::Mutex<HashMap<String, CacheEntry>>>,
    flights: Arc<tokio::sync::Mutex<HashMap<String, FlightLock>>>,
    /// Last ADMITTED run of each MANUAL-ONLY panel — the floor's clock.
    /// (#1919) Advances when a run is admitted, not when it completes; see
    /// `admit_manual_run` for why, and for what that costs.
    /// Keyed by BASE id (`spec.id`), deliberately never by variant — see
    /// [`admit_manual_run`]. Manual panels are uncached by design, so
    /// this is the only record that one ran at all.
    ///
    /// (#2479) `SystemTime` is the PRIMARY clock, deliberately not
    /// `Instant` — see [`manual_floor_wait`]'s doc for why: this floor
    /// protects an expensive probe against being re-run more often than
    /// an operator would predict with a wristwatch, which is exactly the
    /// class of deadline `Instant` gets wrong across a sleep/wake gap on
    /// macOS.
    ///
    /// (#2479 audit CONSIDER 6) Paired with an `Instant` companion anchor,
    /// same shape as `thermal_governor.rs`'s `real_age_past_interval`: a
    /// backward `SystemTime` jump (NTP correction) makes the wall-clock
    /// side read as "no time has passed" (`duration_since` errs), and
    /// without a second anchor that would fail closed for however long
    /// the wall clock stays behind — potentially far longer than
    /// `MANUAL_MIN_INTERVAL` — rather than the bounded worst case the
    /// pre-#2479 `Instant`-only code had. The monotonic side bounds it
    /// back down to `MANUAL_MIN_INTERVAL` of real elapsed process time.
    ///
    /// (5.0) Keyed by audience too: a caller that is not this machine runs
    /// against its own window, so it can never hold this machine's doctor
    /// closed (see [`Audience`]).
    last_manual: Arc<tokio::sync::Mutex<HashMap<ManualKey, (SystemTime, Instant)>>>,
}

/// A manual-run floor window: one per panel (BASE id) and audience.
type ManualKey = (&'static str, Audience);

/// Who a panel run is for. A caller that is not this machine (and holds no
/// token) is served the verb's remote form, redacted (see the module doc).
/// The manual-run floor keeps a window per audience: a remote caller cannot
/// close this machine's window, and inside its own window it is answered
/// with the last remote run instead of a new probe (see [`run_panel`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum Audience {
    /// This machine, or a caller holding the serve token: the full output.
    Local,
    /// Any other caller: the remote form, redacted.
    Remote,
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
/// One admitted manual run's claim on the floor: whose window it is, and the
/// timestamps stored in [`PanelState::last_manual`]. A timed-out run hands it
/// back to [`release_manual_run`], which gives back only a claim that is
/// still its own.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ManualClaim {
    audience: Audience,
    at: (SystemTime, Instant),
}

/// Admit one manual run, or refuse it — checking the floor and claiming it
/// in ONE lock acquisition (#1919).
///
/// The previous shape checked, released the lock, and only recorded after
/// the spawn had finished. Five concurrent racers therefore all saw an
/// empty clock and all spawned: the cache cannot absorb them either,
/// because a manual panel has TTL 0 and `cached_if_fresh` short-circuits
/// before it is consulted. That is the #1286 perturbation this floor
/// exists to prevent, reached by nothing more exotic than two open
/// consoles — the laptop browser and the tailnet phone dashboard both
/// reach the daemon on loopback by design.
///
/// The clock now advances at ADMISSION rather than at completion, which
/// is a deliberate trade with two consequences worth stating:
///
/// - A spawn that fails fast (a missing binary, an immediate IO error)
///   still consumes the window, so the operator waits without having got
///   a result. Failing closed is the right default for a probe that costs
///   ~2.2s and touches the Keychain, and the alternative — releasing on
///   the error arms — is a real option if that proves annoying.
/// - Completion-to-completion spacing is therefore as low as
///   `MANUAL_MIN_INTERVAL` minus the panel's spawn bound rather than a flat
///   30s. A spawn that TIMES OUT gives the window back
///   ([`release_manual_run`]): it produced nothing to wait out.
///
/// There is no `.await` between the read and the insert, so a racer
/// either observes the timestamp or genuinely arrived after the window.
async fn admit_manual_run(panels: &PanelState, id: &'static str, audience: Audience) -> Result<ManualClaim, (StatusCode, String)> {
    admit_manual_run_at(panels, id, audience, SystemTime::now()).await
}

/// (#2479 audit, MUST FIX 4) `admit_manual_run` with `now` as an explicit
/// argument — no internal `SystemTime::now()` call — same `_at` convention
/// as `manual_floor_wait` itself and `src/acp.rs`'s
/// `idle_self_exit_loop_with`.
///
/// This is the seam the fix that introduced `manual_floor_wait` was
/// missing: every test of the floor OPENING called `manual_floor_wait`
/// directly (the pure function), never `admit_manual_run` (the real
/// production call site) — because `admit_manual_run` called the clock
/// internally, no test could inject a past `prev`/`now` pair through it.
/// A mutation swapping the two `SystemTime` arguments at the
/// `manual_floor_wait` call site below (`manual_floor_wait(now, prev, …)`
/// instead of `(prev, now, …)`) left every existing test green: the
/// wrapper-level tests only ever exercised "just ran" (immediately
/// refloored, which a swap also floors), never the floor genuinely
/// opening after real elapsed time. Under that mutation the subtraction
/// inside `manual_floor_wait` errors for any forward-moving clock (the
/// swapped `now` argument, now bound to the function's `prev` parameter,
/// is always later than the swapped `prev` argument bound to its `now`
/// parameter), so it always returns the full interval — the manual panel
/// refuses every click, forever, worse than the sleep-blindness bug this
/// PR fixes. `admit_manual_run_at` closes the seam: a test can now call
/// THIS function (the real wrapper, lock acquisition and all) with an
/// injected `prev` far enough in the past that the floor must open, and
/// the argument-swap mutation fails it.
async fn admit_manual_run_at(
    panels: &PanelState,
    id: &'static str,
    audience: Audience,
    now: SystemTime,
) -> Result<ManualClaim, (StatusCode, String)> {
    // (#2479 audit CONSIDER 6) The monotonic companion anchor is always
    // the REAL clock, never injected — see [`manual_floor_wait`]'s doc for
    // why only the wall-clock side needs to be test-injectable. A fresh
    // test process's `Instant::now()` reads as "no time has passed" for
    // any monotonic comparison against a key it just inserted, which is
    // correct: these tests exercise the wall-clock dimension, and the
    // monotonic side only matters when the wall clock is UNRELIABLE
    // (backward jump), which none of them simulate.
    let now_instant = Instant::now();
    let mut last = panels.last_manual.lock().await;
    if let Some(&(prev, prev_instant)) = last.get(&(id, audience)) {
        let monotonic_elapsed = now_instant.duration_since(prev_instant);
        if let Some(remaining) = manual_floor_wait(prev, now, monotonic_elapsed, MANUAL_MIN_INTERVAL) {
            let wait = remaining.as_secs() + 1;
            let since = now.duration_since(prev).unwrap_or(Duration::ZERO).max(monotonic_elapsed);
            return Err((
                StatusCode::TOO_MANY_REQUESTS,
                format!(
                    "panel \"{id}\" is manual-run only and was started {}s ago: it probes \
                     the machine, so it is floored at {}s between runs. Retry-After: {wait}\n",
                    since.as_secs(),
                    MANUAL_MIN_INTERVAL.as_secs()
                ),
            ));
        }
    }
    // Claim the window under the SAME guard that just cleared it.
    let claim = ManualClaim { audience, at: (now, now_instant) };
    last.insert((id, audience), claim.at);
    Ok(claim)
}

/// Pure floor math for [`admit_manual_run`] (#2479): given the wall-clock
/// timestamp of the last admitted run, the current wall-clock time, and
/// how much MONOTONIC time has separately elapsed since admission, return
/// `None` if a new run may be admitted, or `Some(remaining)` if the floor
/// is still in effect. All three time inputs are explicit parameters — no
/// internal `SystemTime::now()`/`Instant::now()` call — so both a
/// sleep-crossing gap and a monotonic-vs-wall-clock divergence are
/// directly testable without ever touching the real clock.
///
/// **`SystemTime`, deliberately not `Instant`, is the PRIMARY fix here.**
/// This floor protects a probe that costs ~2.2s and touches the Keychain
/// from being re-run more often than an operator would predict with a
/// wristwatch — "once every [`MANUAL_MIN_INTERVAL`]". On macOS `Instant`
/// is backed by `CLOCK_UPTIME_RAW`, which does not advance while the
/// machine sleeps (#2479): a floor built on it reads "just ran" for up to
/// a full `MANUAL_MIN_INTERVAL` of AWAKE time after any wake, however long
/// the lid was actually closed — refusing a legitimate click made minutes,
/// or hours, after the probe it is nominally floored against.
/// `SystemTime` (`CLOCK_REALTIME`) advances through sleep, so the floor
/// reflects real elapsed time from the moment the daemon is reachable
/// again, matching what the 429 body already claims ("started Ns ago").
///
/// This is the "outside world" side of #2479's classification, not the
/// "process activity" side: contrast with the inactivity watchdog
/// (`DARKMUX_INACTIVITY_TIMEOUT_SECONDS`) and `DARKMUX_MODEL_LOAD_
/// TIMEOUT_SECONDS`, which correctly stay on `Instant` — see the audit
/// left at their definitions.
///
/// **`monotonic_elapsed` is the CONSIDER-6 companion fix**, same shape as
/// `thermal_governor.rs`'s `real_age_past_interval`: `since` is the
/// LARGER of the wall-clock gap and the monotonic gap, not the wall-clock
/// gap alone. A `now` earlier than `prev` (a backward wall-clock jump —
/// an NTP correction, not sleep) makes the wall-clock side read as zero
/// elapsed (`duration_since` errs, clamped to `Duration::ZERO` rather
/// than treated as evidence of staleness or underflowed into a huge
/// value) — without the monotonic side, that would fail closed for
/// however long the wall clock stays behind, which is unbounded and can
/// exceed `min_interval` by a lot (an hour-back NTP step floors the panel
/// for the better part of an hour, not `min_interval`). The monotonic
/// side, which cannot jump backward, bounds the floored duration back
/// down to `min_interval` of real elapsed process time — the same
/// worst-case bound the pre-#2479 `Instant`-only code had for this exact
/// scenario. A forward sleep gap is unaffected: `by_wall` alone already
/// exceeds `min_interval` in that case, so the max is `by_wall` regardless
/// of what the (necessarily small, process-suspended) monotonic side
/// reads.
fn manual_floor_wait(
    prev: SystemTime,
    now: SystemTime,
    monotonic_elapsed: Duration,
    min_interval: Duration,
) -> Option<Duration> {
    let by_wall = now.duration_since(prev).unwrap_or(Duration::ZERO);
    let since = by_wall.max(monotonic_elapsed);
    if since >= min_interval {
        None
    } else {
        Some(min_interval - since)
    }
}

/// The gates in front of every panel request, in order: the preflight-forcing
/// header (checked BEFORE the allowlist lookup so a drive-by never even learns
/// which ids exist), then the allowlist lookup. Returns the panel's spec. No
/// panel is refused for who is asking: the read gate in front of this route
/// already applied the read posture, and a caller that is not this machine is
/// served the panel redacted (see the module doc).
fn admit_panel_request(id: &str, headers: &axum::http::HeaderMap) -> Result<PanelSpec, (StatusCode, String)> {
    // The preflight forcer — see PANEL_HEADER.
    if !headers.contains_key(PANEL_HEADER) {
        return Err((
            StatusCode::FORBIDDEN,
            format!(
                "panel requests require the `{PANEL_HEADER}` header: it forces a CORS \
                 preflight so a foreign page cannot drive this endpoint from the \
                 operator's browser\n"
            ),
        ));
    }

    let Some(spec) = panel_spec(id) else {
        return Err((
            StatusCode::NOT_FOUND,
            format!("unknown panel \"{id}\": panels are a fixed allowlist, not arbitrary commands\n"),
        ));
    };
    Ok(spec)
}

/// The request's opt selections resolved into what runs: the argv, the cache
/// key and the `opts` echo. The roster-valued opt is taken out first, so
/// [`resolve_opts`] sees static opts only. Only a panel that declares such an
/// opt and was sent a value reads the daemon's cached fleet view.
async fn resolve_selection(
    state: &AppState,
    spec: &PanelSpec,
    params: &HashMap<String, String>,
) -> Result<(Vec<String>, String, std::collections::BTreeMap<String, String>), (StatusCode, String)> {
    let bad = |msg: String| (StatusCode::BAD_REQUEST, msg);
    let mut requested = extract_opt_params(params);
    let machine = match (spec.roster_opt, requested.contains_key(ROSTER_MACHINE_OPT)) {
        (Some(_), true) => {
            // The same names the CLI's `--machine` accepts: every roster id and
            // this machine's own (`FleetView::selector_names`).
            let roster = crate::fleet_view::cached_view(state).await.map_err(|(code, msg)| (code, msg.to_string()))?.selector_names();
            let remote_on = requested.get("remote").is_some_and(|v| v == "on");
            resolve_roster_opt(spec, &mut requested, &roster, remote_on).map_err(bad)?
        }
        _ => None,
    };
    let resolved = resolve_opts(spec, &requested).map_err(bad)?;
    Ok(with_roster_choice(spec, compose_argv(spec, &resolved), variant_key(spec.id, &resolved), opts_map(&resolved), machine.as_deref()))
}

/// The ONE output filter for a caller that is not this machine or a token
/// holder, applied to every panel's response whatever the panel (the verb has
/// already rendered its remote form, see the module doc), to stdout and
/// stderr alike: roster addresses read "(address hidden)", the daemon user's
/// home prefix reads `~` (a `DARKMUX_HOME` outside it reads
/// `$DARKMUX_HOME`), and every value in `w` ([`crate::panel_withheld`]) reads
/// "(shown on this machine only)". stderr is redacted, not dropped, so a
/// panel that failed still tells the remote viewer why. Matches are whole
/// tokens only, and a machine id or name is never hidden. When anything was
/// withheld (here, or by the verb's remote form), `withheld` carries the one
/// notice naming the command and `machine`.
fn redact_for_remote(body: &mut PanelResponse, r: &Redaction, w: &Withheld, machine: Option<&str>) {
    let mut withheld = false;
    for text in [&mut body.ansi_text, &mut body.stderr_tail] {
        let shown = redact_panel_stdout(text, r, w);
        withheld |= shown != *text || shown.contains(panel_audience::WITHHELD);
        *text = shown;
    }
    body.withheld = if withheld {
        panel_audience::notice(&format!("darkmux {}", body.argv.join(" ")), machine)
    } else {
        String::new()
    };
}

/// The cache and single-flight key for a selection rendered for `remote`: a
/// verb's remote form is a different output, so it never shares an entry with
/// this machine's.
fn audience_key(key: &str, remote: bool) -> String {
    if remote {
        format!("{key}\u{0}remote")
    } else {
        key.to_string()
    }
}

pub(crate) async fn panel_handler(
    Path(id): Path<String>,
    raw_query: axum::extract::RawQuery,
    params: Query<HashMap<String, String>>,
    peer: Option<axum::extract::ConnectInfo<std::net::SocketAddr>>,
    headers: axum::http::HeaderMap,
    State(state): State<AppState>,
) -> Result<axum::Json<PanelResponse>, (StatusCode, String)> {
    let remote = !crate::caller_is_local_or_holds_token(peer.map(|c| c.0), &headers);
    let mut body = run_panel(&id, raw_query.0, params.0, remote, &headers, &state).await?;
    if remote {
        // Both sets read the disk (the roster, config.json, the profile and
        // fixture registries), so they are derived off the async runtime, and
        // reused while what they read is unchanged (see `redaction`'s caches).
        let (r, w, machine) = tokio::task::spawn_blocking(|| {
            (Redaction::derive_cached(), crate::redaction::panel_withheld_cached(), darkmux_flow::resolve_machine_id())
        })
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("redacting panel \"{id}\": {e}\n")))?;
        redact_for_remote(&mut body, &r, &w, machine.as_deref());
    }
    Ok(axum::Json(body))
}

async fn run_panel(
    id: &str,
    raw_query: Option<String>,
    params: HashMap<String, String>,
    remote: bool,
    headers: &axum::http::HeaderMap,
    state: &AppState,
) -> Result<PanelResponse, (StatusCode, String)> {
    let spec = admit_panel_request(id, headers)?;
    // A repeated query key is ambiguous (the map below would keep one of the
    // two), so it is refused rather than resolved last-wins.
    if let Some(key) = raw_query.as_deref().and_then(duplicate_query_key) {
        return Err((
            StatusCode::BAD_REQUEST,
            format!("query key \"{key}\" appears more than once on panel \"{}\": send each key once\n", spec.id),
        ));
    }
    // Canonical &'static id straight off the spec — one table, no second
    // lookup that could drift out from under it.
    let id: &'static str = spec.id;

    // `opt.<name>` query params. An unknown name or value is a 400, never
    // silently ignored.
    let (final_argv, key, opts_echo) = resolve_selection(state, &spec, &params).await?;
    let key = audience_key(&key, remote);

    // (#1911) Lenient on read: a malformed `cols` (`abc`, empty, or past
    // `u16`) resolves to the default width rather than failing the whole
    // request. The typed `Query<PanelParams>` extractor this replaced
    // rejected those with a 400 before the handler ran. The change is
    // deliberate and matches this project's lenient-on-read convention
    // (and the hash grammar's "invalid params drop silently to default"),
    // but it IS a behavior change — pinned by `cols_is_lenient_on_read`.
    // Note the asymmetry with `opt.*` above, which fails CLOSED: a wrong
    // width is a cosmetic misread, a wrong option is a different command.
    let cols = clamp_cols(parse_cols(&params));

    // Manual-only panels (TTL 0) are floored server-side, keyed by BASE id
    // — see `admit_manual_run`'s own doc for why that must never be the
    // variant key — and by audience. A remote caller inside its window is
    // answered with the last remote run, never a new probe: polling from
    // the tailnet can neither close this machine's window nor make it probe
    // more than once per window.
    let audience = if remote { Audience::Remote } else { Audience::Local };
    let claim = if spec.auto_refresh {
        None
    } else {
        match admit_manual_run(&state.panels, id, audience).await {
            Ok(claim) => Some(claim),
            Err(refused) if remote => return last_remote_run(&state.panels, &key).await.ok_or(refused),
            Err(refused) => return Err(refused),
        }
    };

    // Serve fresh-enough cache without spawning.
    if let Some(body) = cached_if_fresh(&state.panels, &key, spec.cache_ttl).await {
        return Ok(body);
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
        return Ok(body);
    }

    // (#1914) When this panel reads the fleet stream, hand the spawned
    // child the daemon's OWN already-fetched snapshot instead of letting it
    // pay its own full Redis round trip — that read runs ~650ms+ over a
    // tailnet at ordinary fleet activity, and a subprocess cannot reach
    // `fleet_flow_records()`'s in-process 2s cache no matter how warm it is
    // (see the module doc, "every panel until now reads local disk only").
    // `spawn_blocking` because `fleet_flow_records` does a synchronous
    // network call — same reason `runs_handler` wraps it the same way.
    // Best-effort throughout: ANY failure here (join failure, write
    // failure) just skips the env var below, and the child falls back to
    // its own live read (`fleet_records_for_runs`'s own doc) — a snapshot
    // that couldn't be prepared is never a reason to fail the panel.
    let fleet_snapshot = if spec.needs_fleet_snapshot { prepare_fleet_snapshot(id).await } else { None };

    let started = Instant::now();
    let exe = child_exe(&state.panels).map_err(|e| {
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
        // Which form to render: a caller that is not this machine gets the
        // verb's remote form (see the module doc). Removed otherwise, so a
        // daemon started from a shell that happened to set it still serves
        // this machine in full.
        .env_remove(panel_audience::AUDIENCE_ENV)
        // A child must never inherit the daemon's own serve lifecycle env in
        // a way that could confuse it; everything else (DARKMUX_HOME, dirs)
        // is deliberately inherited — the panel must see the same state the
        // operator's own shell would.
        .kill_on_drop(true);

    if remote {
        cmd.env(panel_audience::AUDIENCE_ENV, panel_audience::REMOTE);
    }
    if let Some(file) = &fleet_snapshot {
        // (#1914) Opt-in by the SAME construction as `DARKMUX_PANEL` above:
        // a bare terminal invocation never has this env var set, so it
        // takes the exact same live-Redis path it always has.
        cmd.env(crate::FLEET_SNAPSHOT_ENV_VAR, file.path());
    }

    let output = run_child(&state.panels, &spec, claim, cmd).await?;

    let gather_ms = started.elapsed().as_millis() as u64;
    let ansi_text = capped_stdout(&String::from_utf8_lossy(&output.stdout));
    let stderr_tail = stderr_tail(&String::from_utf8_lossy(&output.stderr));

    let body = PanelResponse {
        panel: id.to_string(),
        argv: final_argv.clone(),
        opts: opts_echo,
        captured_ts_ms: current_millis(),
        gather_ms,
        exit_code: output.status.code(),
        ansi_text,
        // Non-empty only when something went to stderr — surfaced so a
        // failing verb is diagnosable from the panel itself, not just logs.
        stderr_tail,
        withheld: String::new(),
        cols,
        cache_ttl_ms: spec.cache_ttl.as_millis() as u64,
        age_ms: 0,
        auto_refresh: spec.auto_refresh,
    };

    if spec.cache_ttl.is_zero() && !remote {
        // Manual panel: nothing cached (an explicit run is a real run), but
        // the floor's clock advances so the next caller is bounded. Keyed
        // by BASE id — see `admit_manual_run`'s own doc.
        // (#1919) No record here any more — `admit_manual_run` claimed the
        // window when it admitted this run. Recording again on completion
        // would extend the floor by the spawn's own duration.
    } else {
        // A manual panel's REMOTE run is kept (under its remote key) as the
        // last remote run, which a remote caller inside the window reads
        // ([`last_remote_run`]); `cached_if_fresh` never serves it, TTL 0.
        let mut cache = state.panels.cache.lock().await;
        cache.insert(key, CacheEntry { body: body.clone(), captured: SystemTime::now() });
    }
    Ok(body)
}

/// The program a panel spawns: the daemon's own binary (a test may substitute
/// one, see [`PanelState::child_exe`]).
fn child_exe(panels: &PanelState) -> std::io::Result<std::path::PathBuf> {
    #[cfg(test)]
    if let Some(exe) = &panels.child_exe {
        return Ok(exe.clone());
    }
    let _ = panels;
    std::env::current_exe()
}

/// The fleet-snapshot handoff file for a panel that reads the fleet stream, or
/// `None` when it could not be prepared (the child then reads Redis itself).
async fn prepare_fleet_snapshot(id: &str) -> Option<tempfile::NamedTempFile> {
    match tokio::task::spawn_blocking(crate::fleet_flow_records).await {
        Ok(read) => match crate::write_fleet_snapshot_file(&read) {
            Ok(file) => Some(file),
            Err(e) => {
                eprintln!(
                    "darkmux serve: panel \"{id}\": could not write the fleet snapshot \
                     ({e}); the child will read Redis directly"
                );
                None
            }
        },
        Err(e) => {
            eprintln!(
                "darkmux serve: panel \"{id}\": fleet read task failed ({e}); the child \
                 will read Redis directly"
            );
            None
        }
    }
}

/// Run the panel's child under the spec's own spawn bound. A child that does
/// not finish in time is stopped (`kill_on_drop`) and answered as a 504 that
/// says the verb was slow; for a manual-run panel the floor is released too,
/// because the operator got no result and a retry is not a loop.
async fn run_child(
    panels: &PanelState,
    spec: &PanelSpec,
    claim: Option<ManualClaim>,
    mut cmd: tokio::process::Command,
) -> Result<std::process::Output, (StatusCode, String)> {
    let id = spec.id;
    match tokio::time::timeout(spec.spawn_timeout, cmd.output()).await {
        Ok(Ok(output)) => Ok(output),
        Ok(Err(e)) => Err((StatusCode::INTERNAL_SERVER_ERROR, format!("spawning panel \"{id}\": {e}\n"))),
        Err(_) => {
            if let Some(claim) = claim {
                release_manual_run(panels, id, claim).await;
            }
            Err((
                StatusCode::GATEWAY_TIMEOUT,
                format!(
                    "panel \"{id}\" is slow: it did not finish within {}s; try again{}\n",
                    spec.spawn_timeout.as_secs(),
                    if spec.auto_refresh { "" } else { "; this timeout did not use up the manual-run wait" }
                ),
            ))
        }
    }
}

/// Give back the manual-run floor claimed for `id`, so an immediate retry is
/// admitted. Only a timed-out spawn calls this: a run that produced no result
/// must not make the operator wait out the floor. It removes the entry only
/// while it is still `claim`, this run's own: a later run admitted after the
/// window opened holds its own claim, and this one must not erase it.
async fn release_manual_run(panels: &PanelState, id: &'static str, claim: ManualClaim) {
    let mut last = panels.last_manual.lock().await;
    let key = (id, claim.audience);
    if last.get(&key) == Some(&claim.at) {
        last.remove(&key);
    }
}

/// The stderr kept in a response. Known diagnostics (`[darkmux-liveness]`
/// traces, #1311) are dropped: they are written on success too and used to
/// push the real error out of a five-line tail. Of what remains, lines that
/// read as errors are kept first, then the most recent others, up to
/// [`STDERR_TAIL_LINES`], in their original order.
fn stderr_tail(raw: &str) -> String {
    let lines: Vec<&str> = raw.lines().filter(|l| !l.starts_with("[darkmux-liveness]")).collect();
    let is_error = |l: &str| {
        let l = l.trim_start().to_ascii_lowercase();
        // Rust prints `thread 'main' panicked at ...`, so the word is not a prefix.
        l.starts_with("error") || l.starts_with("fatal") || l.contains(" panicked at ")
    };
    let mut keep = vec![false; lines.len()];
    let mut budget = STDERR_TAIL_LINES;
    for (i, _) in lines.iter().enumerate().rev().filter(|(_, l)| is_error(l)).take(budget) {
        keep[i] = true;
        budget -= 1;
    }
    for i in (0..lines.len()).rev().filter(|i| !keep[*i]).take(budget).collect::<Vec<_>>() {
        keep[i] = true;
    }
    lines.iter().zip(&keep).filter(|(_, k)| **k).map(|(l, _)| *l).collect::<Vec<_>>().join("\n")
}

/// `stdout`, cut at a line boundary under [`PANEL_STDOUT_CAP_BYTES`] with a
/// visible note when it was cut.
fn capped_stdout(stdout: &str) -> String {
    if stdout.len() <= PANEL_STDOUT_CAP_BYTES {
        return stdout.to_string();
    }
    let mut end = PANEL_STDOUT_CAP_BYTES;
    while !stdout.is_char_boundary(end) {
        end -= 1;
    }
    let cut = stdout[..end].rfind('\n').map_or(end, |i| i + 1);
    format!(
        "{}[output truncated: {} of {} bytes shown]\n",
        &stdout[..cut],
        cut,
        stdout.len()
    )
}

/// The last remote run of a manual panel, with `age_ms` saying how old it is:
/// what a remote caller inside the panel's floor window is served instead of
/// a new probe. `None` when no remote run has finished yet.
async fn last_remote_run(panels: &PanelState, key: &str) -> Option<PanelResponse> {
    let cache = panels.cache.lock().await;
    let entry = cache.get(key)?;
    let mut body = entry.body.clone();
    body.age_ms = cache_entry_age_ms_at(entry.captured, SystemTime::now());
    Some(body)
}

/// Serve the cached body if it is within `ttl`, with `age_ms` restamped so
/// the client can SEE it got a cached copy (#1286 constraint 4 — cadence and
/// staleness are recorded knobs, never silent).
async fn cached_if_fresh(panels: &PanelState, key: &str, ttl: Duration) -> Option<PanelResponse> {
    if ttl.is_zero() {
        return None;
    }
    let cache = panels.cache.lock().await;
    let entry = cache.get(key)?;
    let now = SystemTime::now();
    if !cache_entry_is_fresh(entry.captured, now, ttl) {
        return None;
    }
    let mut body = entry.body.clone();
    body.age_ms = cache_entry_age_ms_at(entry.captured, now);
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
        // 8 as of `run-list` (#1911), 9 with `profile-list`, 10 with `machine-list` (5.0). Bumping this number is the doctrine
        // decision: an entry is legal only if it neither dispatches a model
        // (#1286) nor mutates, so the worst case of a bug here stays a wrong
        // READING. This now counts 10 BASE VERBS, not 10 id/argv combinations
        // (#1911) — a verb's VARIANTS (its declared `opts`) grow the option
        // space, not this list; see `variant_cross_product_stays_bounded`
        // for that guard instead. If growth ever comes from wanting
        // operator-supplied VALUES with no closed set (`--since <when>`),
        // stop — an open value space is not an allowlist, and that surface
        // is a lens, not a panel. (`profile-list`'s `machine` is closed: the
        // roster's own names, see the module doc.)
        assert_eq!(PANEL_IDS.len(), 10, "allowlist growth is a doctrine decision, not a drive-by");
    }

    /// (#1914, widened #1711) `run-list` and `mission-status` are the ONLY
    /// panels that read the network today (every other entry is a
    /// local-disk read) — so they are the only ones whose spawn should pay
    /// to prepare a fleet snapshot handoff file. A future panel added here
    /// that also reads the fleet stream should flip this to true
    /// deliberately, not by accident — this guard fails loudly the day
    /// that happens to anyone who forgets to make the call.
    #[test]
    fn only_these_ids_need_a_fleet_snapshot() {
        let needs_one: &[&str] = &["run-list", "mission-status"];
        for id in PANEL_IDS {
            let spec = panel_spec(id).unwrap();
            assert_eq!(
                spec.needs_fleet_snapshot,
                needs_one.contains(id),
                "{id}.needs_fleet_snapshot should be {} but was {}",
                needs_one.contains(id),
                spec.needs_fleet_snapshot
            );
        }
    }

    /// Layer 2 of the three-layer guard (#1911): pills are always base
    /// verbs, mechanically. A flag baked into a spec's BASE argv means the
    /// old per-variant-id shape crept back in — `mission-status-all`'s own
    /// argv (`["mission","status","--all"]`) would trip this if it were
    /// ever re-added as a direct entry.
    #[test]
    fn no_panel_bakes_a_flag_into_its_base_argv() {
        for id in PANEL_IDS {
            let spec = panel_spec(id).unwrap();
            assert!(
                spec.argv.iter().all(|t| !t.starts_with('-')),
                "{id} bakes a flag into its argv: flags are opts, ids are verbs"
            );
        }
    }

    #[test]
    fn doctor_is_manual_only_and_uncached() {
        let d = panel_spec("doctor").unwrap();
        assert!(!d.auto_refresh, "doctor probes: auto-polling it is the observer joining the observed");
        assert!(d.cache_ttl.is_zero(), "an explicit doctor run must be a real run");
        // …and every other panel IS auto-refreshable with a real TTL.
        for id in
            ["mission-status", "role-list", "machine-status", "machine-list", "config-list", "flow-status", "lab-fixture-list", "run-list"]
        {
            let s = panel_spec(id).unwrap();
            assert!(s.auto_refresh, "{id}");
            let want = if s.needs_fleet_snapshot { FLEET_PANEL_CACHE_TTL } else { PANEL_CACHE_TTL };
            assert_eq!(s.cache_ttl, want, "{id}");
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
                // …and ONLY the default may. A non-default with an empty
                // fragment produces a second cache entry and a second
                // spawn for a byte-identical command line, and advertises
                // a selection the displayed command cannot show.
                for v in &opt.values[1..] {
                    assert!(
                        !v.argv.is_empty(),
                        "{id}'s opt \"{}\" non-default value \"{}\" carries empty argv: only \
                         values[0] may, or it is a distinct selection that changes nothing",
                        opt.name,
                        v.value
                    );
                }
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
                    // A fragment is a flag, optionally followed by ONE
                    // value for it. Checking only `argv[0]` let a
                    // positional ride along behind a legal-looking flag
                    // (`["--kind","mission","extra-positional"]` passed),
                    // which is weaker than this module's own framing.
                    assert!(
                        v.argv.len() <= 2,
                        "{id}'s opt \"{}\" value \"{}\" has more than a flag and its value: {:?}",
                        opt.name,
                        v.value,
                        v.argv
                    );
                    if let Some(tail) = v.argv.get(1) {
                        assert!(
                            !tail.starts_with('-'),
                            "{id}'s opt \"{}\" value \"{}\" packs a second flag into one fragment: {:?}",
                            opt.name,
                            v.value,
                            v.argv
                        );
                    }
                }
            }
        }
    }

    /// The cache-growth guard #1911 calls for: a bound on the TOTAL variant
    /// cross-product, not just the base-verb count layer 3 already guards.
    /// Today: `mission-status` (2) + `run-list` (4×2×2=16, #2902 added the
    /// `usage` toggle) + `profile-list` (2: its `remote` toggle) + six no-opt
    /// panels (1 each) = 26, plus `machine-list` (1) = 27. A roster `machine` opt
    /// (`profile-list`, `machine-status`) adds one variant per roster machine,
    /// validated by membership, so it is bounded by the roster and is not
    /// counted here.
    #[test]
    fn variant_cross_product_stays_bounded() {
        let mut total = 0usize;
        for id in PANEL_IDS {
            let spec = panel_spec(id).unwrap();
            let variants: usize = spec.opts.iter().map(|o| o.values.len()).product::<usize>().max(1);
            total += variants;
        }
        assert!(
            total <= 27,
            "variant cross-product grew to {total}: bumping the bound is a doctrine \
             decision (#1911), not a drive-by"
        );
    }

    // ── resolve_opts: the 400-on-unknown-option/value guard (#1911) ──

    #[test]
    fn resolve_opts_defaults_every_declared_opt_when_nothing_requested() {
        let spec = panel_spec("run-list").unwrap();
        let requested = HashMap::new();
        let resolved = resolve_opts(&spec, &requested).unwrap();
        assert_eq!(resolved.len(), 3, "one resolved entry per DECLARED opt, defaulted");
        assert_eq!(resolved[0].name, "kind");
        assert_eq!(resolved[0].value, "all");
        assert!(resolved[0].argv.is_empty());
        assert!(resolved[0].is_default);
        assert_eq!(resolved[1].name, "all");
        assert_eq!(resolved[1].value, "recent");
        assert!(resolved[1].argv.is_empty());
        assert!(resolved[1].is_default);
        // (#2902) The usage toggle, off by default.
        assert_eq!(resolved[2].name, "usage");
        assert_eq!(resolved[2].value, "off");
        assert!(resolved[2].argv.is_empty());
        assert!(resolved[2].is_default);
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
        // Insertion order is not iteration order, and `HashMap`'s is
        // randomized PER PROCESS — so a single map would let an
        // implementation that walked `requested` pass roughly half the
        // time. Both permutations, plus an assertion on the resolved
        // NAMES, make this deterministic rather than probabilistic.
        for pairs in [[("all", "all"), ("kind", "mission")], [("kind", "mission"), ("all", "all")]] {
            let mut requested = HashMap::new();
            for (k, v) in pairs {
                requested.insert(k.to_string(), v.to_string());
            }
            let resolved = resolve_opts(&spec, &requested).unwrap();
            let names: Vec<&str> = resolved.iter().map(|r| r.name).collect();
            let declared: Vec<&str> = spec.opts.iter().map(|o| o.name).collect();
            assert_eq!(names, declared, "resolved opts must be in DECLARATION order, not query order");
            let argv = compose_argv(&spec, &resolved);
            assert_eq!(
                argv,
                vec!["run", "list", "--kind", "mission", "--all"],
                "argv must follow declaration order (kind, then all), not query-string order: {argv:?}"
            );
        }
    }

    /// (#2902 step 2b) `run-list` declares `usage` as a flag toggle:
    /// default `off` contributes nothing, `on` contributes the literal
    /// `--usage`, in declaration order AFTER `--kind` and `--all` — so the
    /// console's command line shows the flag the CLI actually receives.
    #[test]
    fn run_list_declares_the_usage_toggle_as_its_last_opt() {
        let spec = panel_spec("run-list").unwrap();
        let usage = spec.opts.iter().find(|o| o.name == "usage").expect("run-list declares a usage opt");
        assert_eq!(usage.values.len(), 2);
        assert_eq!((usage.values[0].value, usage.values[0].argv), ("off", &[][..]));
        assert_eq!((usage.values[1].value, usage.values[1].argv), ("on", &["--usage"][..]));
        assert_eq!(spec.opts.last().map(|o| o.name), Some("usage"), "declared last, so argv order is kind, all, usage");
        let mut requested = HashMap::new();
        requested.insert("usage".to_string(), "on".to_string());
        requested.insert("all".to_string(), "all".to_string());
        let resolved = resolve_opts(&spec, &requested).unwrap();
        assert_eq!(compose_argv(&spec, &resolved), vec!["run", "list", "--all", "--usage"]);
        assert_eq!(variant_key(spec.id, &resolved), "run-list?all=all&usage=on");
        // The other panel with opts does NOT grow the flag.
        assert!(panel_spec("mission-status").unwrap().opts.iter().all(|o| o.name != "usage"));
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

    // ── opts_map: the response echo (#1911) ──────────────────────────

    #[test]
    fn opts_map_echoes_every_declared_opt_including_defaults() {
        let spec = panel_spec("run-list").unwrap();
        let resolved = resolve_opts(&spec, &HashMap::new()).unwrap();
        let map = opts_map(&resolved);
        let expected: std::collections::BTreeMap<String, String> =
            [("kind", "all"), ("all", "recent"), ("usage", "off")].map(|(k, v)| (k.to_string(), v.to_string())).into();
        assert_eq!(map, expected);
    }

    #[test]
    fn opts_map_is_empty_object_for_a_no_opts_panel() {
        let spec = panel_spec("doctor").unwrap();
        let resolved = resolve_opts(&spec, &HashMap::new()).unwrap();
        assert!(opts_map(&resolved).is_empty());
    }

    // ── the `all` opt is the only way to the unlimited board (#1911) ─────

    #[test]
    fn mission_status_all_is_not_a_panel() {
        assert!(
            panel_spec("mission-status-all").is_none(),
            "mission-status-all folded into mission-status's `all` opt (#1911) and its alias \
             is retired: it must not resolve at all"
        );
    }

    #[test]
    fn the_all_opt_composes_the_unlimited_argv_and_its_own_cache_key() {
        let spec = panel_spec("mission-status").unwrap();
        let requested: HashMap<String, String> = [("all".to_string(), "all".to_string())].into();
        let resolved = resolve_opts(&spec, &requested).unwrap();
        assert_eq!(compose_argv(&spec, &resolved), vec!["mission", "status", "--all"]);
        assert_eq!(variant_key(spec.id, &resolved), "mission-status?all=all");
    }

    // ── declaration-table invariants the handler leans on (#1911) ────

    /// `variant_key` builds `id?name=value&name=value`, so a name or value
    /// containing one of those separators could make two DIFFERENT
    /// selections produce the SAME key — and for up to the TTL one would
    /// serve the other's output under the wrong argv. Concretely: an opt
    /// `x` whose values included the literal `1&y=2`, alongside an opt `y`
    /// with value `2`, collides with `{x:"1", y:"2"}`.
    ///
    /// Unreachable from today's table, which is exactly why it is pinned
    /// here rather than left to notice later: this module's own standard
    /// is that only legal combinations can ever become keys.
    #[test]
    fn no_opt_name_or_value_contains_a_variant_key_separator() {
        for id in PANEL_IDS {
            let spec = panel_spec(id).unwrap();
            for opt in spec.opts {
                assert!(
                    !opt.name.contains(['?', '&', '=']),
                    "{id}'s opt name {:?} contains a variant-key separator",
                    opt.name
                );
                for v in opt.values {
                    assert!(
                        !v.value.contains(['?', '&', '=']),
                        "{id}'s opt \"{}\" value {:?} contains a variant-key separator: two \
                         different selections could canonicalize to one cache key",
                        opt.name,
                        v.value
                    );
                }
            }
        }
    }

    /// "Manual" is spelled two ways — `auto_refresh: false` and a zero
    /// `cache_ttl` — and two different sites read different ones: the
    /// floor gate keys off `auto_refresh`. (#1919: the floor CLOCK used to
    /// key off the ttl instead, which is what made this mismatch reachable;
    /// admission now advances it, so both read the same field. The pin is
    /// kept because a spec declaring one without the other is still
    /// self-contradictory.) Historically `last_manual` only
    /// advances when the TTL is zero. An entry with `auto_refresh: false`
    /// and a NON-zero TTL would therefore be checked against a clock that
    /// never ticks, and its floor would silently never fire. Pin them
    /// equal so that entry cannot be declared.
    #[test]
    fn manual_is_spelled_the_same_way_by_both_fields() {
        for id in PANEL_IDS {
            let spec = panel_spec(id).unwrap();
            assert_eq!(
                spec.auto_refresh,
                !spec.cache_ttl.is_zero(),
                "{id} disagrees with itself about being manual: auto_refresh={}, ttl={:?}: the \
                 floor gate reads auto_refresh and the floor CLOCK reads the ttl, so a mismatch \
                 means the floor never fires",
                spec.auto_refresh,
                spec.cache_ttl
            );
        }
    }

    // ── extract_opt_params: the only place a request string is read ───

    #[test]
    fn extract_opt_params_takes_only_the_opt_prefixed_params() {
        let mut raw = HashMap::new();
        raw.insert("cols".to_string(), "80".to_string());
        raw.insert("opt.kind".to_string(), "lab".to_string());
        raw.insert("kind".to_string(), "mission".to_string());
        let got = extract_opt_params(&raw);
        assert_eq!(got.len(), 1, "only `opt.`-prefixed params are options: {got:?}");
        assert_eq!(got.get("kind"), Some(&"lab".to_string()));
    }

    /// A bare `kind=lab` must NOT be honored — the prefix is what
    /// separates an option from any other query param the daemon may grow.
    #[test]
    fn extract_opt_params_ignores_an_unprefixed_lookalike() {
        let mut raw = HashMap::new();
        raw.insert("kind".to_string(), "lab".to_string());
        assert!(extract_opt_params(&raw).is_empty());
    }

    #[test]
    fn extract_opt_params_keeps_an_empty_name_so_it_can_be_rejected() {
        let mut raw = HashMap::new();
        raw.insert("opt.".to_string(), "x".to_string());
        let got = extract_opt_params(&raw);
        assert_eq!(got.get(""), Some(&"x".to_string()), "an empty name must survive to be 400'd, not vanish");
    }

    /// The typed extractor this replaced rejected a malformed `cols` with
    /// a 400 before the handler ran; it now resolves to the default width.
    /// Deliberate (lenient-on-read), but a real behavior change, so it is
    /// pinned rather than left as an artifact of a refactor.
    #[test]
    fn cols_is_lenient_on_read() {
        for bad in ["abc", "", "99999", "-1", "80.5"] {
            let mut raw = HashMap::new();
            raw.insert("cols".to_string(), bad.to_string());
            assert_eq!(parse_cols(&raw), None, "{bad:?} should fall back to the default width");
        }
        let mut good = HashMap::new();
        good.insert("cols".to_string(), "80".to_string());
        assert_eq!(parse_cols(&good), Some(80));
    }

    // ── handler wiring (#1911) ────────────────────────────────────────
    //
    // Every assertion here is on an ERROR path, deliberately. A 200 spawns
    // `std::env::current_exe()`, which under `cargo test` is the test
    // harness itself — so the happy path stays a live-daemon check, and
    // these pin the refusals that must never regress into a spawn.

    async fn panel_get(uri: &str, with_header: bool) -> (StatusCode, String) {
        use axum::body::{to_bytes, Body};
        use axum::http::Request;
        use tower::util::ServiceExt;
        let flows = tempfile::TempDir::new().unwrap();
        let app = crate::build_router_local(flows.path().to_path_buf());
        let mut req = Request::builder().uri(uri);
        if with_header {
            req = req.header(PANEL_HEADER, "1");
        }
        let res = app.oneshot(req.body(Body::empty()).unwrap()).await.unwrap();
        let status = res.status();
        let bytes = to_bytes(res.into_body(), 65536).await.unwrap();
        (status, String::from_utf8_lossy(&bytes).to_string())
    }

    #[tokio::test]
    async fn handler_rejects_an_unknown_opt_name_before_spawning() {
        let (status, body) = panel_get("/panel/run-list?opt.machine=studio", true).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(body.contains("unknown option"), "{body}");
        assert!(body.contains("machine"), "{body}");
        assert!(body.contains("kind"), "must name the legal set: {body}");
    }

    /// `profile-list` takes a machine, validated against the roster: a name
    /// the roster lacks is a 400 naming VALUES, not "unknown option".
    #[tokio::test]
    async fn handler_validates_the_profile_list_machine_against_the_roster() {
        let (status, body) = panel_get("/panel/profile-list?opt.machine=no-such-machine-in-any-roster", true).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(body.contains("unknown value") && body.contains("legal values"), "{body}");
        assert!(!body.contains("unknown option"), "machine is a declared opt of profile-list: {body}");
    }

    #[tokio::test]
    async fn handler_rejects_an_unknown_opt_value_before_spawning() {
        let (status, body) = panel_get("/panel/run-list?opt.kind=bogus", true).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(body.contains("unknown value"), "{body}");
    }

    /// A bare `kind=lab` is not an option — it must be ignored, not
    /// honored. If the prefix check were dropped this would compose
    /// `--kind lab` and return 200 (a spawn) instead of refusing nothing.
    #[tokio::test]
    async fn handler_ignores_an_unprefixed_lookalike_param() {
        let (status, body) = panel_get("/panel/run-list?kind=bogus", true).await;
        assert_ne!(status, StatusCode::BAD_REQUEST, "a non-`opt.` param must not be read as an option: {body}");
    }

    /// An illegal opt value is a 400, never silently ignored.
    #[tokio::test]
    async fn handler_rejects_an_illegal_opt_value() {
        let (status, body) = panel_get("/panel/mission-status?opt.all=bogus", true).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
        assert!(body.contains("unknown value"), "{body}");
    }

    /// The retired `mission-status-all` id is an unknown panel, not a redirect
    /// or an alias.
    #[tokio::test]
    async fn handler_answers_the_retired_mission_status_all_id_with_404() {
        let (status, _) = panel_get("/panel/mission-status-all", true).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn handler_rejects_an_opt_on_a_panel_that_declares_none() {
        let (status, body) = panel_get("/panel/flow-status?opt.kind=lab", true).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(body.contains("(none)"), "{body}");
    }

    #[tokio::test]
    async fn handler_still_refuses_an_unknown_id_and_a_missing_header() {
        let (status, _) = panel_get("/panel/not-a-panel", true).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "the allowlist still closes over ids");
        let (status, _) = panel_get("/panel/run-list", false).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "the CORS-preflight header gate still applies");
    }

    // ── manual-run floor: keyed by base id, never by variant (#1911) ──

    /// (#1919) The defect this fix exists for, as an executable case.
    /// Five racers, one key, a multi-thread runtime: exactly ONE may be
    /// admitted. Against the previous check-then-record shape all five
    /// were, because each released the lock before any of them recorded.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn admit_manual_run_admits_exactly_one_of_five_concurrent_racers() {
        let panels = PanelState::default();
        let admitted = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let mut handles = Vec::new();
        for _ in 0..5 {
            let p = panels.clone();
            let a = admitted.clone();
            handles.push(tokio::spawn(async move {
                if admit_manual_run(&p, "doctor", Audience::Local).await.is_ok() {
                    a.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                }
            }));
        }
        for h in handles {
            h.await.unwrap();
        }
        assert_eq!(
            admitted.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "the floor admitted more than one concurrent racer: check and claim are not atomic, \
             so N browser tabs produce N real doctor probes (#1919)"
        );
    }

    /// (#1919) And the floor is actually INVOKED. Disabling the call site
    /// used to leave the whole crate green: the function was tested, its
    /// invocation was not.
    ///
    /// Deliberately asserts the 429, an ERROR path. A 200 spawns
    /// `current_exe()`, which under `cargo test` is the harness itself.
    #[tokio::test]
    async fn panel_handler_returns_429_when_the_floor_is_already_claimed() {
        use axum::body::Body;
        use axum::http::Request;
        use tower::util::ServiceExt;
        let flows = tempfile::TempDir::new().unwrap();
        let app = crate::build_router_local(flows.path().to_path_buf());
        // Claim the window first, then ask the handler for the same panel.
        // Reaching the state directly is the only way in without spawning.
        let first = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/panel/doctor")
                    .header(PANEL_HEADER, "1")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        // The first call either ran (200/500) or was refused for some other
        // reason; either way it must have CLAIMED the window.
        assert_ne!(first.status(), StatusCode::TOO_MANY_REQUESTS, "first call must not be floored");

        let second = app
            .oneshot(
                Request::builder()
                    .uri("/panel/doctor")
                    .header(PANEL_HEADER, "1")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            second.status(),
            StatusCode::TOO_MANY_REQUESTS,
            "the second immediate request must be floored: if this passes with the call site \
             disabled, nothing pins that the floor runs at all (#1919)"
        );
    }

    #[tokio::test]
    async fn manual_floor_is_open_before_any_run_is_recorded() {
        let panels = PanelState::default();
        assert!(admit_manual_run(&panels, "doctor", Audience::Local).await.is_ok());
    }

    #[tokio::test]
    async fn manual_floor_fires_on_the_second_call_within_the_window() {
        let panels = PanelState::default();
        panels.last_manual.lock().await.insert(("doctor", Audience::Local), (SystemTime::now(), Instant::now()));
        let err = admit_manual_run(&panels, "doctor", Audience::Local).await.unwrap_err();
        assert_eq!(err.0, StatusCode::TOO_MANY_REQUESTS);
        assert!(err.1.contains("floored"), "{}", err.1);
    }

    #[tokio::test]
    async fn manual_floor_is_independent_per_base_id() {
        let panels = PanelState::default();
        panels.last_manual.lock().await.insert(("doctor", Audience::Local), (SystemTime::now(), Instant::now()));
        // A DIFFERENT base id must not be floored by doctor's own run.
        assert!(admit_manual_run(&panels, "some-other-manual-panel", Audience::Local).await.is_ok());
    }

    /// (#2479 audit, MUST FIX 4) The floor OPENING, through the REAL
    /// production call site — `admit_manual_run_at`, lock acquisition and
    /// all — not just the pure `manual_floor_wait` function. Every prior
    /// test of "the floor opens after enough time" called
    /// `manual_floor_wait` directly; every prior test that went through
    /// `admit_manual_run`/`admit_manual_run_at` only ever exercised "just
    /// ran, still floored." Neither alone would have caught a mutation
    /// that swaps the two `SystemTime` arguments at the
    /// `manual_floor_wait` call site inside `admit_manual_run_at` — that
    /// swap makes the wrapper floor EVERY call forever (see
    /// `admit_manual_run_at`'s own doc), and every test that predates this
    /// one stayed green under it because none of them injected a `prev`
    /// old enough, through the wrapper, for the floor to have to open.
    #[tokio::test]
    async fn admit_manual_run_at_opens_the_floor_through_the_real_call_site_after_the_window() {
        let panels = PanelState::default();
        let prev = SystemTime::now();
        panels.last_manual.lock().await.insert(("doctor", Audience::Local), (prev, Instant::now()));
        // Immediately after: still floored (sanity check this scenario
        // actually starts floored, so the assertion below is meaningful).
        assert!(
            admit_manual_run_at(&panels, "doctor", Audience::Local, prev + Duration::from_secs(1)).await.is_err(),
            "sanity: 1s after the previous run must still be floored"
        );

        // Reset, then check well past the window — through the SAME real
        // call site, not the pure function.
        panels.last_manual.lock().await.insert(("doctor", Audience::Local), (prev, Instant::now()));
        let well_past_the_window = prev + MANUAL_MIN_INTERVAL + Duration::from_secs(5);
        assert!(
            admit_manual_run_at(&panels, "doctor", Audience::Local, well_past_the_window).await.is_ok(),
            "the floor must open once MANUAL_MIN_INTERVAL of real elapsed time has passed, \
             through the actual admission path a real request takes: an argument swap in the \
             manual_floor_wait call site would floor this forever"
        );
    }

    /// Renamed from `manual_floor_cannot_be_defeated_by_cycling_opt_values`
    /// (#1911 review): it never cycled an opt value, and it CANNOT — the
    /// parameter does not exist, which is precisely the real guarantee.
    /// The SIGNATURE is the guard; a test cannot observe the absence of a
    /// parameter, so claiming that name here read as coverage of a
    /// property nothing tests. What this does check is the narrower, real
    /// thing: repeated checks against one base id stay floored.
    ///
    /// The floor check's own signature only ever receives the panel's
    /// BASE id (`spec.id`) — see
    /// `admit_manual_run`'s doc. There is no variant-key parameter for a
    /// caller to pass, structurally, not by convention: cycling any
    /// `opt.*` selection on a (hypothetical, since `doctor` has none today)
    /// manual panel with options cannot produce a different key to the
    /// floor than the base id it always uses. Simulated here: once ANY run
    /// of `doctor` is recorded under its base id, EVERY subsequent check
    /// against that same base id floors, regardless of what a caller might
    /// have "meant" by a different selection — because no selection ever
    /// reaches this function at all.
    #[tokio::test]
    async fn manual_floor_stays_floored_across_repeated_checks_on_one_base_id() {
        let panels = PanelState::default();
        panels.last_manual.lock().await.insert(("doctor", Audience::Local), (SystemTime::now(), Instant::now()));
        for _ in 0..3 {
            let err = admit_manual_run(&panels, "doctor", Audience::Local).await.unwrap_err();
            assert_eq!(
                err.0,
                StatusCode::TOO_MANY_REQUESTS,
                "repeated checks against the SAME base id must stay floored: there is no \
                 variant key in scope that could reset it"
            );
        }
    }

    // ── manual-run floor: wall-clock, not process-uptime (#2479) ──

    /// The two ends within the window: a gap one second short of
    /// `min_interval` is still floored, a gap exactly at it is not. Pins
    /// the boundary rather than just "small gap floored, big gap open."
    /// `monotonic_elapsed: Duration::ZERO` throughout this file's
    /// wall-clock-only tests isolates the wall-clock dimension being
    /// pinned — see `manual_floor_wait_monotonic_*` below for the
    /// monotonic dimension on its own.
    #[test]
    fn manual_floor_wait_boundary_is_exact() {
        let prev = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000);
        let min_interval = Duration::from_secs(30);

        let just_short = prev + Duration::from_secs(29);
        assert_eq!(
            manual_floor_wait(prev, just_short, Duration::ZERO, min_interval),
            Some(Duration::from_secs(1)),
            "1s short of the window must still report exactly 1s remaining"
        );

        let exactly_at = prev + Duration::from_secs(30);
        assert_eq!(
            manual_floor_wait(prev, exactly_at, Duration::ZERO, min_interval),
            None,
            "a gap exactly equal to min_interval must admit: the check is >=, not >"
        );
    }

    /// The behavior this whole fix exists for (#2479), made testable
    /// without sleeping the real machine: construct a `prev`/`now` pair
    /// whose WALL-CLOCK gap is hours, the way a laptop closed mid-session
    /// would produce. `SystemTime` arithmetic has no notion of "awake
    /// time" to under-count — unlike `Instant` on macOS (`CLOCK_UPTIME_
    /// RAW`), which would have read a gap this large as only however many
    /// seconds the process was actually awake between the two calls. A
    /// real sleep can't be reproduced in a unit test, but the clock choice
    /// this fix makes CAN be: this pins that `manual_floor_wait` computes
    /// its answer purely from the two `SystemTime` values it's handed, so
    /// it is correct-by-construction regardless of what the process was
    /// doing (running or suspended) in between.
    #[test]
    fn manual_floor_wait_a_multi_hour_gap_is_never_floored() {
        let prev = SystemTime::UNIX_EPOCH + Duration::from_secs(2_000_000);
        let four_hours_later = prev + Duration::from_secs(4 * 60 * 60);
        assert_eq!(
            manual_floor_wait(prev, four_hours_later, Duration::ZERO, Duration::from_secs(30)),
            None,
            "a 4-hour real-time gap must admit outright, not report a remaining wait: the \
             defect this fix closes is exactly a deadline that stays 'in the future' across a \
             gap the operator's own clock has long since cleared"
        );
    }

    /// Sub-window gaps still floor, and report the correct remaining wait
    /// — the fix must not have turned the floor into a no-op.
    #[test]
    fn manual_floor_wait_still_floors_within_the_window() {
        let prev = SystemTime::UNIX_EPOCH + Duration::from_secs(3_000_000);
        let ten_seconds_later = prev + Duration::from_secs(10);
        assert_eq!(
            manual_floor_wait(prev, ten_seconds_later, Duration::ZERO, Duration::from_secs(30)),
            Some(Duration::from_secs(20)),
            "10s into a 30s window must report 20s remaining"
        );
    }

    /// A backward wall-clock jump (NTP correction) with NO offsetting
    /// monotonic progress fails CLOSED — still floored for the full
    /// interval — rather than guessing a gap it cannot measure. Distinct
    /// from the sleep case above: sleep produces a `now` far AFTER `prev`
    /// (which must admit); this produces a `now` BEFORE `prev` (which
    /// must not, absent monotonic evidence otherwise — see the next two
    /// tests for that evidence).
    #[test]
    fn manual_floor_wait_backward_clock_jump_stays_floored() {
        let prev = SystemTime::UNIX_EPOCH + Duration::from_secs(4_000_000);
        let now = prev - Duration::from_secs(5);
        assert_eq!(
            manual_floor_wait(prev, now, Duration::ZERO, Duration::from_secs(30)),
            Some(Duration::from_secs(30)),
            "an unmeasurable (backward) gap, with no monotonic evidence of real elapsed time, \
             must fail closed at the full interval, not open"
        );
    }

    /// (#2479 audit CONSIDER 6) The companion fix: a backward wall-clock
    /// jump does NOT lock the floor for as long as the wall clock stays
    /// behind — the monotonic side bounds it back down to `min_interval`
    /// of REAL elapsed process time, the same worst case the pre-#2479
    /// `Instant`-only code had. Without this, an hour-back NTP correction
    /// would floor the panel for the better part of an hour instead of
    /// `min_interval` (30s).
    #[test]
    fn manual_floor_wait_monotonic_side_opens_the_floor_despite_a_backward_wall_clock_jump() {
        let prev = SystemTime::UNIX_EPOCH + Duration::from_secs(5_000_000);
        let now = prev - Duration::from_secs(3600); // an hour-back NTP step
        let min_interval = Duration::from_secs(30);
        // 35 REAL (monotonic) seconds have genuinely passed since
        // admission, despite the wall clock claiming otherwise.
        let monotonic_elapsed = Duration::from_secs(35);
        assert_eq!(
            manual_floor_wait(prev, now, monotonic_elapsed, min_interval),
            None,
            "35 real monotonic seconds since admission must open a 30s floor, even though the \
             wall clock: having just jumped an hour backward: reads as no time having passed \
             at all"
        );
    }

    /// The same scenario, but the monotonic side hasn't reached
    /// `min_interval` yet either: the remaining wait must come from
    /// whichever side is LARGER (here, the monotonic side, since the wall
    /// side reads zero), not silently ignore it.
    #[test]
    fn manual_floor_wait_monotonic_side_still_floors_before_its_own_window_closes() {
        let prev = SystemTime::UNIX_EPOCH + Duration::from_secs(6_000_000);
        let now = prev - Duration::from_secs(3600);
        let min_interval = Duration::from_secs(30);
        let monotonic_elapsed = Duration::from_secs(20);
        assert_eq!(
            manual_floor_wait(prev, now, monotonic_elapsed, min_interval),
            Some(Duration::from_secs(10)),
            "20 real monotonic seconds into a 30s window must report 10s remaining, computed \
             from the monotonic side since the wall-clock side reads zero"
        );
    }

    // ── whole-response cache: wall-clock, not process-uptime (#2479) ──
    // Same defect class as the fleet cache in `lib.rs` and the manual-run
    // floor above, found during the #2479 audit enumeration rather than
    // named in the original review — `CacheEntry.captured` was the last
    // `Instant`-based OUTSIDE-WORLD clock left in this file.

    #[test]
    fn cache_entry_is_fresh_within_ttl() {
        let captured = SystemTime::UNIX_EPOCH + Duration::from_secs(7_000_000);
        let now = captured + Duration::from_millis(500);
        assert!(cache_entry_is_fresh(captured, now, PANEL_CACHE_TTL));
    }

    #[test]
    fn cache_entry_is_stale_after_ttl_of_pure_awake_time() {
        let captured = SystemTime::UNIX_EPOCH + Duration::from_secs(7_000_000);
        let now = captured + Duration::from_secs(10); // PANEL_CACHE_TTL is 3s
        assert!(!cache_entry_is_fresh(captured, now, PANEL_CACHE_TTL));
    }

    /// The actual defect: a panel cached just before a multi-hour sleep
    /// must not read as fresh on wake, however small an `Instant`-based
    /// clock would have said the gap was.
    #[test]
    fn cache_entry_is_stale_after_a_simulated_multi_hour_sleep_gap() {
        let captured = SystemTime::UNIX_EPOCH + Duration::from_secs(7_000_000);
        let now = captured + Duration::from_secs(4 * 3600);
        assert!(!cache_entry_is_fresh(captured, now, PANEL_CACHE_TTL));
    }

    #[test]
    fn cache_entry_backward_clock_jump_is_treated_as_not_fresh() {
        let captured = SystemTime::UNIX_EPOCH + Duration::from_secs(7_000_000);
        let now = captured - Duration::from_secs(60);
        assert!(
            !cache_entry_is_fresh(captured, now, PANEL_CACHE_TTL),
            "a clock that just stepped backward must not also be trusted to vouch for the cache"
        );
    }

    #[test]
    fn cache_entry_age_ms_reports_the_real_wall_clock_gap() {
        let captured = SystemTime::UNIX_EPOCH + Duration::from_secs(7_000_000);
        let now = captured + Duration::from_secs(4 * 3600);
        assert_eq!(cache_entry_age_ms_at(captured, now), 4 * 3600 * 1000);
    }

    #[test]
    fn cache_entry_age_ms_fails_loud_not_falsely_fresh_on_backward_jump() {
        let captured = SystemTime::UNIX_EPOCH + Duration::from_secs(7_000_000);
        let now = captured - Duration::from_secs(60);
        assert_eq!(cache_entry_age_ms_at(captured, now), u64::MAX);
    }

    // ── profile-list: the roster-valued `machine` opt (5.0) ───────────

    fn roster() -> Vec<String> {
        vec!["studio".to_string(), "mini".to_string()]
    }

    fn req(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
    }

    #[test]
    fn profile_list_is_a_read_panel_over_the_profile_verb() {
        let spec = panel_spec("profile-list").unwrap();
        assert_eq!(spec.argv, &["profile", "list"]);
        assert_eq!(spec.roster_opt, Some(ROSTER_AS_FLAG));
        assert!(!spec.needs_fleet_snapshot);
        let others = PANEL_IDS.iter().filter(|id| !matches!(**id, "profile-list" | "machine-status"));
        assert!(others.into_iter().all(|id| panel_spec(id).unwrap().roster_opt.is_none()), "only these two panels take a machine");
    }

    #[test]
    fn a_roster_machine_becomes_the_roster_own_spelling_in_argv_key_and_echo() {
        let spec = panel_spec("profile-list").unwrap();
        let mut requested = req(&[("machine", "STUDIO")]);
        let machine = resolve_roster_opt(&spec, &mut requested, &roster(), false).unwrap();
        assert_eq!(machine.as_deref(), Some("studio"), "the roster's spelling, not the client's bytes");
        let resolved = resolve_opts(&spec, &requested).unwrap();
        let (argv, key, echo) =
            with_roster_choice(&spec, compose_argv(&spec, &resolved), variant_key(spec.id, &resolved), opts_map(&resolved), machine.as_deref());
        assert_eq!(argv, vec!["profile", "list", "--machine", "studio"]);
        assert_eq!(key, "profile-list?machine=studio");
        assert_eq!(echo.get("machine").map(String::as_str), Some("studio"));
    }

    #[test]
    fn no_machine_is_the_local_list_with_the_plain_key() {
        let spec = panel_spec("profile-list").unwrap();
        for raw in [req(&[]), req(&[("machine", "")])] {
            let mut requested = raw;
            let machine = resolve_roster_opt(&spec, &mut requested, &roster(), false).unwrap();
            assert_eq!(machine, None);
            let resolved = resolve_opts(&spec, &requested).unwrap();
            let (argv, key, echo) =
                with_roster_choice(&spec, compose_argv(&spec, &resolved), variant_key(spec.id, &resolved), opts_map(&resolved), None);
            assert_eq!((argv, key.as_str()), (vec!["profile".to_string(), "list".to_string()], "profile-list"));
            assert!(!echo.contains_key("machine"));
        }
    }

    #[test]
    fn a_machine_that_is_not_in_the_roster_is_refused_naming_the_roster() {
        let spec = panel_spec("profile-list").unwrap();
        for bad in ["nowhere", "studio --remote", "../studio", "studio&remote=on"] {
            let err = resolve_roster_opt(&spec, &mut req(&[("machine", bad)]), &roster(), false).unwrap_err();
            assert!(err.contains("legal values: studio, mini"), "{bad}: {err}");
        }
        let empty = resolve_roster_opt(&spec, &mut req(&[("machine", "studio")]), &[], false).unwrap_err();
        assert!(empty.contains("the roster is empty"), "{empty}");
    }

    /// 5.0: the panel accepts this machine's own name for `profile list
    /// --machine <self>`, as the CLI does, though the roster does not list it.
    #[test]
    fn this_machines_own_name_is_a_legal_machine_though_the_roster_does_not_list_it() {
        use crate::fleet_view::{gather_view, tests as fv, FLEET_VIEW_CACHE_TTL};
        let spec = panel_spec("profile-list").unwrap();
        let s = fv::scripted(fv::identity("laptop", None, Some("nLAPTOP")), vec![fv::entry("studio")]);
        let names = gather_view(&s, FLEET_VIEW_CACHE_TTL).selector_names();
        let machine = resolve_roster_opt(&spec, &mut req(&[("machine", "LAPTOP")]), &names, false).unwrap();
        assert_eq!(machine.as_deref(), Some("laptop"), "the roster's own spelling of this machine");
        assert!(resolve_roster_opt(&spec, &mut req(&[("machine", "studio")]), &names, false).is_ok());
    }

    #[test]
    fn a_machine_and_remote_together_are_refused() {
        let spec = panel_spec("profile-list").unwrap();
        let err = resolve_roster_opt(&spec, &mut req(&[("machine", "studio")]), &roster(), true).unwrap_err();
        assert!(err.contains("cannot be combined"), "{err}");
    }

    #[test]
    fn remote_is_a_static_toggle_and_machine_is_unknown_to_every_other_panel() {
        let spec = panel_spec("profile-list").unwrap();
        let resolved = resolve_opts(&spec, &req(&[("remote", "on")])).unwrap();
        assert_eq!(compose_argv(&spec, &resolved), vec!["profile", "list", "--remote"]);
        assert_eq!(variant_key(spec.id, &resolved), "profile-list?remote=on");
        // A panel with no roster opt leaves `machine` for `resolve_opts` to refuse.
        let run = panel_spec("run-list").unwrap();
        let mut requested = req(&[("machine", "studio")]);
        assert_eq!(resolve_roster_opt(&run, &mut requested, &roster(), false), Ok(None));
        assert!(resolve_opts(&run, &requested).unwrap_err().contains("unknown option \"machine\""));
    }

    // ── machine-list and machine-status's roster machine (5.0) ────────

    #[test]
    fn machine_list_is_a_read_panel_over_the_fleet_view() {
        let spec = panel_spec("machine-list").unwrap();
        assert_eq!(spec.argv, &["machine", "list"]);
        assert!(spec.opts.is_empty() && spec.roster_opt.is_none());
        assert!(!spec.needs_fleet_snapshot);
    }

    /// `machine status` takes the machine as its positional id, not a flag, so
    /// the argv a roster choice produces has no `--machine`.
    #[test]
    fn a_machine_status_roster_machine_is_its_positional_id() {
        let spec = panel_spec("machine-status").unwrap();
        assert_eq!(spec.roster_opt, Some(ROSTER_AS_POSITIONAL));
        let mut requested = req(&[("machine", "STUDIO")]);
        let machine = resolve_roster_opt(&spec, &mut requested, &roster(), false).unwrap();
        let resolved = resolve_opts(&spec, &requested).unwrap();
        let (argv, key, echo) = with_roster_choice(
            &spec,
            compose_argv(&spec, &resolved),
            variant_key(spec.id, &resolved),
            opts_map(&resolved),
            machine.as_deref(),
        );
        assert_eq!(argv, vec!["machine", "status", "--", "studio"]);
        assert_eq!(key, "machine-status?machine=studio");
        assert_eq!(echo.get("machine").map(String::as_str), Some("studio"));
        let err = resolve_roster_opt(&spec, &mut req(&[("machine", "nowhere")]), &roster(), false).unwrap_err();
        assert!(err.contains("legal values: studio, mini"), "{err}");
    }

    #[tokio::test]
    async fn handler_validates_the_machine_status_machine_against_the_roster() {
        let (status, body) = panel_get("/panel/machine-status?opt.machine=no-such-machine-in-any-roster", true).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(body.contains("unknown value") && body.contains("legal values"), "{body}");
    }

    // ── console review fixes (5.0) ────────────────────────────────────

    /// The fleet-reading panels' cold spawn is about 5s, so their TTL must
    /// outlast one spawn; every other panel keeps the short TTL.
    #[test]
    fn fleet_reading_panels_cache_longer_than_one_cold_spawn() {
        assert!(FLEET_PANEL_CACHE_TTL > Duration::from_secs(5), "TTL under one cold spawn re-spawns on every poll");
        for id in PANEL_IDS {
            let spec = panel_spec(id).unwrap();
            if spec.needs_fleet_snapshot {
                assert_eq!(spec.cache_ttl, FLEET_PANEL_CACHE_TTL, "{id}");
            }
        }
    }

    #[test]
    fn doctor_gets_more_spawn_time_than_the_fast_verbs() {
        assert_eq!(panel_spec("doctor").unwrap().spawn_timeout, DOCTOR_SPAWN_TIMEOUT);
        assert!(DOCTOR_SPAWN_TIMEOUT >= Duration::from_secs(21), "doctor measures 7s with black-holed peers");
        for id in PANEL_IDS.iter().filter(|id| **id != "doctor") {
            assert_eq!(panel_spec(id).unwrap().spawn_timeout, PANEL_SPAWN_TIMEOUT, "{id}");
        }
    }

    fn sleeping_child() -> tokio::process::Command {
        let mut cmd = tokio::process::Command::new("sleep");
        cmd.arg("30").kill_on_drop(true);
        cmd
    }

    /// A timed-out manual spawn is worded "slow", is a 504, and does NOT
    /// consume the manual-run floor: an immediate retry is admitted.
    #[tokio::test]
    async fn a_timed_out_manual_spawn_is_slow_not_wedged_and_releases_the_floor() {
        let panels = PanelState::default();
        let mut spec = panel_spec("doctor").unwrap();
        spec.spawn_timeout = Duration::from_millis(50);
        let claim = admit_manual_run(&panels, spec.id, Audience::Local).await.unwrap();
        let (status, body) = run_child(&panels, &spec, Some(claim), sleeping_child()).await.unwrap_err();
        assert_eq!(status, StatusCode::GATEWAY_TIMEOUT);
        assert!(body.contains("slow"), "{body}");
        assert!(!body.contains("wedged"), "{body}");
        admit_manual_run(&panels, spec.id, Audience::Local).await.expect("the timeout must give the manual-run window back");
    }

    /// An auto-refresh panel's timeout has no floor to release, and its
    /// message does not promise one.
    #[tokio::test]
    async fn a_timed_out_auto_panel_is_slow_too() {
        let panels = PanelState::default();
        let mut spec = panel_spec("role-list").unwrap();
        spec.spawn_timeout = Duration::from_millis(50);
        let (status, body) = run_child(&panels, &spec, None, sleeping_child()).await.unwrap_err();
        assert_eq!(status, StatusCode::GATEWAY_TIMEOUT);
        assert!(body.contains("slow") && !body.contains("manual-run wait"), "{body}");
    }

    /// A child that finishes keeps its floor claim: only a timeout releases.
    #[tokio::test]
    async fn a_finished_manual_spawn_still_holds_the_floor() {
        let panels = PanelState::default();
        let spec = panel_spec("doctor").unwrap();
        let claim = admit_manual_run(&panels, spec.id, Audience::Local).await.unwrap();
        let mut ok = tokio::process::Command::new("true");
        ok.kill_on_drop(true);
        run_child(&panels, &spec, Some(claim), ok).await.unwrap();
        assert_eq!(admit_manual_run(&panels, spec.id, Audience::Local).await.unwrap_err().0, StatusCode::TOO_MANY_REQUESTS);
    }

    #[test]
    fn a_repeated_query_key_is_found_even_when_encoded_differently() {
        assert_eq!(duplicate_query_key("cols=80&opt.kind=lab&opt.kind=all"), Some("opt.kind".into()));
        assert_eq!(duplicate_query_key("opt.all=all&opt%2Eall=recent"), Some("opt.all".into()));
        assert_eq!(duplicate_query_key("cols=80&opt.kind=lab&opt.all=all"), None);
        assert_eq!(duplicate_query_key(""), None);
        // The same VALUE twice is still a repeated key.
        assert_eq!(duplicate_query_key("cols=80&cols=80"), Some("cols".into()));
    }

    #[tokio::test]
    async fn handler_refuses_a_repeated_query_key_naming_it() {
        let (status, body) = panel_get("/panel/run-list?opt.kind=lab&opt.kind=all", true).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(body.contains("\"opt.kind\"") && body.contains("more than once"), "{body}");
    }

    #[test]
    fn stderr_tail_drops_liveness_lines_and_keeps_the_real_error() {
        let mut raw = String::from("error: the roster is unreadable\n");
        for i in 0..20 {
            raw.push_str(&format!("[darkmux-liveness] T +{i}ms phase pid=1 case=c\n"));
        }
        let tail = stderr_tail(&raw);
        assert_eq!(tail, "error: the roster is unreadable");
    }

    #[test]
    fn stderr_tail_prefers_error_lines_over_later_noise() {
        let mut raw = String::from("error: first\n");
        for i in 0..30 {
            raw.push_str(&format!("note {i}\n"));
        }
        let tail = stderr_tail(&raw);
        let lines: Vec<&str> = tail.lines().collect();
        assert_eq!(lines.len(), STDERR_TAIL_LINES);
        assert_eq!(lines[0], "error: first", "the error survives and keeps its place: {tail}");
        assert_eq!(*lines.last().unwrap(), "note 29");
    }

    #[test]
    fn stdout_is_capped_at_a_line_with_a_visible_note() {
        let line = "x".repeat(99) + "\n";
        let big = line.repeat(PANEL_STDOUT_CAP_BYTES / 100 + 50);
        let out = capped_stdout(&big);
        assert!(out.len() < big.len());
        assert!(out.contains("[output truncated:"), "{}", &out[out.len().saturating_sub(80)..]);
        assert!(out.starts_with(&line));
        assert_eq!(capped_stdout("small\n"), "small\n");
    }

    /// A flag-shaped roster id never reaches argv: refused by the machine-name
    /// rule even when the roster itself lists it.
    #[test]
    fn a_flag_shaped_roster_id_is_refused() {
        for spec_id in ["profile-list", "machine-status"] {
            let spec = panel_spec(spec_id).unwrap();
            let err = resolve_roster_opt(&spec, &mut req(&[("machine", "--all")]), &["--all".to_string()], false).unwrap_err();
            assert!(err.contains("not a legal machine name"), "{spec_id}: {err}");
        }
    }

    /// Both roster shapes, as argv: a flag value, or `--` then the id.
    #[test]
    fn a_positional_roster_id_follows_a_separator_and_a_flag_id_follows_its_flag() {
        let argv_of = |id: &str| {
            let spec = panel_spec(id).unwrap();
            let mut requested = req(&[("machine", "studio")]);
            let machine = resolve_roster_opt(&spec, &mut requested, &roster(), false).unwrap();
            let resolved = resolve_opts(&spec, &requested).unwrap();
            with_roster_choice(&spec, compose_argv(&spec, &resolved), variant_key(spec.id, &resolved), opts_map(&resolved), machine.as_deref()).0
        };
        assert_eq!(argv_of("machine-status"), ["machine", "status", "--", "studio"]);
        assert_eq!(argv_of("profile-list"), ["profile", "list", "--machine", "studio"]);
    }

    /// The panel table as JSON, for the console's own test to read. Written
    /// by `DARKMUX_REGENERATE_FIXTURES=1`, compared otherwise, so the console
    /// cannot drift from this table without a red test on one side.
    /// The tokens `with_roster_choice` puts before a chosen id: asked of that
    /// function itself, so the rule has one owner.
    fn argv_before_id(spec: &PanelSpec) -> Vec<String> {
        let (argv, _, _) = with_roster_choice(spec, spec.argv.to_vec(), spec.id.to_string(), Default::default(), Some("ID"));
        argv[spec.argv.len()..argv.len() - 1].to_vec()
    }

    #[test]
    fn panel_table_matches_the_generated_fixture() {
        let table: Vec<serde_json::Value> = PANEL_IDS
            .iter()
            .map(|id| {
                let s = panel_spec(id).unwrap();
                serde_json::json!({
                    "id": s.id,
                    "argv": s.argv,
                    "auto_refresh": s.auto_refresh,
                    "opts": s.opts.iter().map(|o| serde_json::json!({
                        "name": o.name,
                        "values": o.values.iter().map(|v| serde_json::json!({"value": v.value, "argv": v.argv})).collect::<Vec<_>>(),
                    })).collect::<Vec<_>>(),
                    "roster_opt": s.roster_opt.map(|r| serde_json::json!({
                        "name": r.name,
                        "flag": r.flag,
                        // The argv tokens that precede the chosen machine's id.
                        "argv_before_id": argv_before_id(&s),
                    })),
                })
            })
            .collect();
        let rendered = serde_json::to_string_pretty(&table).unwrap() + "\n";
        let path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../ui/src/lenses/console/panel-table.generated.json");
        if std::env::var_os("DARKMUX_REGENERATE_FIXTURES").is_some() {
            std::fs::write(&path, &rendered).expect("writing the panel table fixture");
            return;
        }
        let on_disk = std::fs::read_to_string(&path)
            .expect("panel-table.generated.json is missing: regenerate with DARKMUX_REGENERATE_FIXTURES=1");
        assert_eq!(
            rendered, on_disk,
            "the panel table changed: regenerate with `DARKMUX_REGENERATE_FIXTURES=1 cargo nextest run -p \
             darkmux-serve panel_table`, then update ui/src/lenses/console/panels.ts to match"
        );
    }

    // ── the remote-caller output filter (5.0 console review) ───────────

    fn probe_body() -> PanelResponse {
        PanelResponse {
            panel: "machine-status".into(),
            argv: vec!["machine".into(), "status".into()],
            opts: Default::default(),
            captured_ts_ms: 0,
            gather_ms: 1,
            exit_code: Some(1),
            ansi_text: "\x1b[2mregistry:\x1b[0m \x1b[2m/Users/tester/.darkmux/fleet.json\x1b[0m\nfixture: \x1b[2m/Users/tester/fx/a\x1b[0m\n\x1b[1mpeerone\x1b[0m at \x1b[2mpeerone.tailnet.example:8765\x1b[0m\n".into(),
            withheld: String::new(),
            stderr_tail: "the roster address for peerone (`peerone.tailnet.example:8765`) does not resolve\nwarning: env var X (/Users/tester/notebook) is ignored".into(),
            cols: 100,
            cache_ttl_ms: 3000,
            age_ms: 0,
            auto_refresh: true,
        }
    }

    fn redaction() -> Redaction {
        Redaction::from_parts(
            &[("peerone", "peerone.tailnet.example:8765"), ("ten", "10.1.2.3")],
            &[],
            Some("/Users/tester".into()),
            None,
        )
    }

    fn remote_view(body: &mut PanelResponse, r: &Redaction) {
        redact_for_remote(body, r, &Withheld::default(), Some("studio"));
    }

    /// (5.0 review item 7) A remote caller reads stderr REDACTED, never
    /// dropped: a failed panel still says why, with the same facts withheld
    /// as from stdout.
    #[test]
    fn a_remote_caller_reads_stderr_redacted_with_no_roster_address_or_home_path() {
        let mut body = probe_body();
        let w = Withheld::from_values(["/opt/fake/bin/lms".to_string()]);
        body.stderr_tail.push_str("\nerror: `/opt/fake/bin/lms` was not found");
        redact_for_remote(&mut body, &redaction(), &w, Some("studio"));
        let all = format!("{}{}", body.ansi_text, body.stderr_tail);
        assert!(!all.contains("tailnet.example") && !all.contains("/Users/") && !all.contains("/opt/fake"), "{all}");
        assert!(body.ansi_text.contains("at \x1b[2m(address hidden)\x1b[0m"), "{}", body.ansi_text);
        assert!(body.ansi_text.contains("\x1b[2m~/.darkmux/fleet.json\x1b[0m"), "{}", body.ansi_text);
        assert_eq!(
            body.stderr_tail,
            "the roster address for peerone (`(address hidden)`) does not resolve\nwarning: env var X (~/notebook) is ignored\n\
             error: `(shown on this machine only)` was not found",
            "stderr is shown redacted"
        );
        assert_eq!(body.withheld, panel_audience::notice("darkmux machine status", Some("studio")));
    }

    /// Nothing withheld, nothing said: output with no host fact, no
    /// withheld value and no stderr carries no notice.
    #[test]
    fn a_remote_caller_is_told_nothing_when_nothing_was_withheld() {
        let mut body = probe_body();
        body.stderr_tail.clear();
        body.ansi_text = "role list\nanalyst coder\n".into();
        remote_view(&mut body, &redaction());
        assert_eq!((body.stderr_tail.as_str(), body.withheld.as_str()), ("", ""));
    }

    /// The panel extra set: a configured path and an endpoint URL the shared
    /// redaction does not know read "(shown on this machine only)", in a
    /// colored run as in plain text, and the notice says so. A verb's own
    /// remote form (a `(shown on this machine only)` it printed) earns the
    /// notice too.
    #[test]
    fn a_configured_location_is_withheld_and_the_notice_says_so() {
        let w = Withheld::from_values(["/opt/fake/bin/lms".to_string(), "https://myres.example.com/v1".into()]);
        let mut body = probe_body();
        body.stderr_tail.clear();
        body.ansi_text = "lms at \x1b[2m/opt/fake/bin/lms\x1b[0m; endpoint https://myres.example.com/v1\n".into();
        redact_for_remote(&mut body, &redaction(), &w, Some("studio"));
        assert_eq!(
            body.ansi_text,
            "lms at \x1b[2m(shown on this machine only)\x1b[0m; endpoint (shown on this machine only)\n"
        );
        assert!(!body.withheld.is_empty());
        let mut body = probe_body();
        body.stderr_tail.clear();
        body.ansi_text = "\"busy_policy\": \"(shown on this machine only)\"\n".into();
        redact_for_remote(&mut body, &redaction(), &Withheld::default(), Some("studio"));
        assert!(!body.withheld.is_empty(), "the verb's own remote form earns the notice");
    }

    #[test]
    fn the_redaction_set_is_derived_from_the_roster_addresses_and_their_hosts() {
        let r = Redaction::from_parts(
            &[("a", "Peer.Example:8765"), ("b", "[fd7a::1]:8765"), ("c", "10.0.0.4"), ("d", "")],
            &[],
            Some("/".into()),
            None,
        );
        for want in ["Peer.Example:8765", "Peer.Example", "[fd7a::1]:8765", "fd7a::1", "10.0.0.4"] {
            assert!(r.addresses().contains(&want), "{want} in {:?}", r.addresses());
        }
        assert!(r.addresses().windows(2).all(|w| w[0].len() >= w[1].len()), "longest first");
        assert!(r.dirs().is_empty(), "a bare / is not a home to rewrite");
    }

    fn body_with(text: &str) -> PanelResponse {
        let mut b = probe_body();
        b.ansi_text = text.to_string();
        b.stderr_tail.clear();
        b
    }

    fn filtered(r: &Redaction, text: &str) -> String {
        let mut b = body_with(text);
        remote_view(&mut b, r);
        b.ansi_text
    }

    /// Bare MagicDNS names as roster addresses (the form the roster doc
    /// suggests): they are ids too, so ordinary words and ids stay intact.
    #[test]
    fn short_hostnames_do_not_corrupt_ordinary_output() {
        let r = Redaction::from_parts(
            &[("mac", "mac"), ("ana", "ana"), ("mini", "mini"), ("studio", "studio")],
            &[],
            Some("/Users/kain".into()),
            None,
        );
        let text = "darkmux machine list\nmacos aarch64\nanalyst bail-with-explanation\nmini studio ana mac\n";
        assert_eq!(filtered(&r, text), text);
    }

    /// The same short names when the id differs from the address: only whole
    /// tokens go, never a substring of a word.
    #[test]
    fn a_short_address_is_hidden_only_as_a_whole_token() {
        let r = Redaction::from_parts(&[("peerone", "mac.local"), ("peertwo", "ana.local")], &[], None, None);
        assert_eq!(
            filtered(&r, "mac.local is up; macos aarch64; analyst; lmstudio-community/mac.local-x ana.local, (mac.local)"),
            "(address hidden) is up; macos aarch64; analyst; lmstudio-community/mac.local-x (address hidden), ((address hidden))"
        );
    }

    /// A host part that equals a roster id is public, so it is not hidden; the
    /// address with its port still is.
    #[test]
    fn a_host_that_equals_a_roster_id_is_public_but_its_port_form_is_not() {
        let r = Redaction::from_parts(&[("studio", "studio:8765")], &[], None, None);
        assert_eq!(filtered(&r, "studio at studio:8765"), "studio at (address hidden)");
        // A name the roster learned from the machine's own card counts too.
        let r = Redaction::from_parts(&[("peerone", "boxa.tailnet.example:8765")], &["boxa.tailnet.example".to_string()], None, None);
        assert_eq!(filtered(&r, "boxa.tailnet.example"), "boxa.tailnet.example");
    }

    #[test]
    fn a_model_key_that_contains_a_host_as_a_substring_survives() {
        let r = Redaction::from_parts(&[("peerone", "studio.example"), ("peertwo", "studio")], &[], None, None);
        assert_eq!(
            filtered(&r, "lmstudio-community/qwen3-4b studio.example lmstudio:1234 lmstudio.example studio:1234"),
            "lmstudio-community/qwen3-4b (address hidden) lmstudio:1234 lmstudio.example (address hidden):1234"
        );
    }

    /// A bare roster host (no dot) is a word in prose ("LM Studio", "the
    /// Studio is busy"): it is a host fact only where it addresses.
    #[test]
    fn a_bare_roster_host_is_hidden_only_where_it_addresses() {
        let r = Redaction::from_parts(&[("peerone", "studio:8765")], &[], None, None);
        assert_eq!(filtered(&r, "LM Studio: the Studio is busy"), "LM Studio: the Studio is busy");
        assert_eq!(
            filtered(&r, "at studio:8765 and http://studio/x and studio:9000 and me@studio"),
            "at (address hidden) and http://(address hidden)/x and (address hidden):9000 and me@(address hidden)"
        );
    }

    #[test]
    fn the_home_rewrite_respects_path_boundaries_and_covers_darkmux_home() {
        let r = Redaction::from_parts(&[], &[], Some("/srv/kain".into()), Some("/Volumes/x/dm".into()));
        assert_eq!(
            filtered(&r, "/srv/kain/a /srv/kainx/b /Volumes/x/dm/c /Volumes/x/dmz"),
            "~/a /srv/kainx/b $DARKMUX_HOME/c /Volumes/x/dmz"
        );
        // Another account's home reads `~` whoever it is.
        assert_eq!(filtered(&r, "/Users/kainx/b"), "~/b");
        // A DARKMUX_HOME inside HOME is covered by the HOME rewrite.
        let r = Redaction::from_parts(&[], &[], Some("/srv/kain".into()), Some("/srv/kain/.darkmux".into()));
        assert_eq!(filtered(&r, "/srv/kain/.darkmux/x"), "~/.darkmux/x");
    }

    fn app_state() -> AppState {
        AppState {
            flows_dir: std::path::PathBuf::new(),
            sse_open: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            lab_dir: None,
            panels: PanelState::default(),
            fleet: crate::fleet_view::FleetContext::hermetic(),
            live_ingest: None,
        }
    }

    /// Serve `role-list` from a pre-seeded cache (no spawn) to `peer`, with
    /// the roster and HOME the filter derives from set for the call.
    async fn served_to(peer: &str, host: &str) -> PanelResponse {
        let dir = tempfile::tempdir().unwrap();
        let roster = dir.path().join("fleet.json");
        std::fs::write(
            &roster,
            r#"{"version":"2","machines":{"peerone":{"id":"peerone","address":"peerone.tailnet.example:8765","added_unix_ms":1}}}"#,
        )
        .unwrap();
        let (prev_fleet, prev_home) = (std::env::var("DARKMUX_FLEET_FILE").ok(), std::env::var("HOME").ok());
        std::env::set_var("DARKMUX_FLEET_FILE", &roster);
        std::env::set_var("HOME", "/Users/tester");
        let state = app_state();
        // Both forms seeded: this machine's and the remote one (the verb's
        // remote form is cached apart, see `audience_key`). The remote entry
        // holds what an unshaping verb would print, so the daemon's own
        // filter is what is under test.
        for key in [audience_key("role-list", false), audience_key("role-list", true)] {
            state.panels.cache.lock().await.insert(key, CacheEntry { body: probe_body(), captured: SystemTime::now() });
        }
        let mut headers = axum::http::HeaderMap::new();
        headers.insert(PANEL_HEADER, "1".parse().unwrap());
        headers.insert("host", host.parse().unwrap());
        let out = panel_handler(
            Path("role-list".to_string()),
            axum::extract::RawQuery(None),
            Query(HashMap::new()),
            Some(axum::extract::ConnectInfo(peer.parse().unwrap())),
            headers,
            State(state),
        )
        .await;
        for (k, v) in [("DARKMUX_FLEET_FILE", prev_fleet), ("HOME", prev_home)] {
            match v {
                Some(v) => std::env::set_var(k, v),
                None => std::env::remove_var(k),
            }
        }
        out.unwrap().0
    }

    /// The reviewer's probe through the real handler: a remote caller reads
    /// no DNS name and no home path from a Read panel; this machine reads it
    /// unchanged.
    #[tokio::test]
    #[serial_test::serial]
    async fn the_handler_filters_a_remote_caller_and_leaves_this_machine_alone() {
        let remote = served_to("100.64.1.2:50000", "localhost:8765").await;
        let all = format!("{}{}", remote.ansi_text, remote.stderr_tail);
        assert!(!all.contains("tailnet.example") && !all.contains("/Users/"), "{all}");
        assert!(remote.stderr_tail.contains("does not resolve"), "stderr is shown redacted: {}", remote.stderr_tail);
        assert!(!remote.withheld.is_empty());

        let local = served_to("127.0.0.1:50000", "localhost:8765").await;
        let want = probe_body();
        assert_eq!(local.ansi_text, want.ansi_text);
        assert_eq!(local.stderr_tail, want.stderr_tail);
    }

    #[test]
    fn stderr_tail_keeps_a_rust_panic_line() {
        let mut raw = String::from("thread 'main' panicked at src/x.rs:1:1:\nboom\n");
        for i in 0..30 {
            raw.push_str(&format!("note {i}\n"));
        }
        let tail = stderr_tail(&raw);
        assert!(tail.lines().next().unwrap().contains("panicked at"), "{tail}");
    }

    /// The release is only worth having while a timed-out run still sits
    /// inside the floor: the doctor bound must be shorter than the floor.
    #[test]
    fn doctor_times_out_inside_the_manual_floor() {
        assert!(DOCTOR_SPAWN_TIMEOUT < MANUAL_MIN_INTERVAL);
    }

    /// A timed-out run gives back only ITS claim: a later run admitted after
    /// the window opened keeps its own.
    #[tokio::test]
    async fn a_timeout_does_not_erase_a_later_runs_claim() {
        let panels = PanelState::default();
        let mut spec = panel_spec("doctor").unwrap();
        spec.spawn_timeout = Duration::from_millis(50);
        let first = admit_manual_run_at(&panels, "doctor", Audience::Local, SystemTime::now() - Duration::from_secs(120)).await.unwrap();
        // A second run is admitted (the first's window long open) and claims the floor.
        let second = admit_manual_run(&panels, "doctor", Audience::Local).await.unwrap();
        assert_ne!(first, second);
        // The first run now times out: it must not release the second's claim.
        run_child(&panels, &spec, Some(first), sleeping_child()).await.unwrap_err();
        assert_eq!(admit_manual_run(&panels, "doctor", Audience::Local).await.unwrap_err().0, StatusCode::TOO_MANY_REQUESTS);
    }

    /// ONE state: a remote caller is served through the spawn path, then this
    /// machine. The cache holds the unfiltered body, so this machine still
    /// reads it unchanged.
    #[tokio::test]
    #[serial_test::serial]
    async fn one_state_serves_a_remote_caller_then_this_machine_unredacted() {
        let dir = tempfile::tempdir().unwrap();
        let roster = dir.path().join("fleet.json");
        // The child prints a line naming the roster's address.
        let child = dir.path().join("child.sh");
        std::fs::write(&child, "#!/bin/sh\necho \"peer at example-host.example:8765 role list [$DARKMUX_PANEL_AUDIENCE]\"\n").unwrap();
        std::fs::set_permissions(&child, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
        // (#2976) macOS scans a freshly written executable on its first exec,
        // which can outlast the panel's spawn bound on a loaded host: exec it
        // once outside the bound.
        let _ = std::process::Command::new(&child).output();
        std::fs::write(
            &roster,
            r#"{"version":"2","machines":{"peerone":{"id":"peerone","address":"example-host.example:8765","added_unix_ms":1}}}"#,
        )
        .unwrap();
        let prev = std::env::var("DARKMUX_FLEET_FILE").ok();
        std::env::set_var("DARKMUX_FLEET_FILE", &roster);
        let mut state = app_state();
        state.panels.child_exe = Some(child);
        let serve = |peer: &str| {
            let mut headers = axum::http::HeaderMap::new();
            headers.insert(PANEL_HEADER, "1".parse().unwrap());
            headers.insert("host", "localhost:8765".parse().unwrap());
            panel_handler(
                Path("role-list".to_string()),
                axum::extract::RawQuery(None),
                Query(HashMap::new()),
                Some(axum::extract::ConnectInfo(peer.parse().unwrap())),
                headers,
                State(state.clone()),
            )
        };
        let remote = serve("100.64.1.2:50000").await.unwrap().0;
        let local = serve("127.0.0.1:50000").await.unwrap().0;
        match prev {
            Some(v) => std::env::set_var("DARKMUX_FLEET_FILE", v),
            None => std::env::remove_var("DARKMUX_FLEET_FILE"),
        }
        assert_eq!(remote.ansi_text.trim(), "peer at (address hidden) role list [remote]");
        assert_eq!(
            local.ansi_text.trim(),
            "peer at example-host.example:8765 role list []",
            "this machine's form is cached apart from the remote one, and unfiltered"
        );
    }

    /// (5.0 review item 4) A caller that is not this machine can neither lock
    /// this machine out of `doctor` nor make it probe on demand. The
    /// manual-run floor is kept per audience, so a remote run never claims
    /// this machine's window; and a remote caller inside the remote window is
    /// served the last remote run (its `age_ms` says how old) rather than a
    /// new probe. This machine's own floor is unchanged.
    #[tokio::test]
    #[serial_test::serial]
    async fn a_remote_caller_cannot_floor_this_machines_doctor_or_probe_on_demand() {
        let dir = tempfile::tempdir().unwrap();
        let runs = dir.path().join("runs.log");
        let child = dir.path().join("child.sh");
        std::fs::write(&child, format!("#!/bin/sh\necho run >> '{}'\necho \"doctor [$DARKMUX_PANEL_AUDIENCE]\"\n", runs.display())).unwrap();
        std::fs::set_permissions(&child, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
        // (#2976) The first exec of a fresh executable is scanned; do it
        // outside the panel's spawn bound.
        let _ = std::process::Command::new(&child).output();
        std::fs::remove_file(&runs).unwrap();
        let mut state = app_state();
        state.panels.child_exe = Some(child.clone());
        let serve = |peer: &'static str| {
            let mut headers = axum::http::HeaderMap::new();
            headers.insert(PANEL_HEADER, "1".parse().unwrap());
            headers.insert("host", "localhost:8765".parse().unwrap());
            panel_handler(
                Path("doctor".to_string()),
                axum::extract::RawQuery(None),
                Query(HashMap::new()),
                Some(axum::extract::ConnectInfo(peer.parse().unwrap())),
                headers,
                State(state.clone()),
            )
        };
        let spawned = || std::fs::read_to_string(&runs).map(|t| t.lines().count()).unwrap_or(0);
        let remote = serve("100.64.1.2:50000").await.expect("a remote caller runs doctor").0;
        assert!(remote.ansi_text.contains("doctor [remote]"), "{}", remote.ansi_text);
        let local = serve("127.0.0.1:50000").await;
        assert!(local.is_ok(), "a remote run must not floor this machine's doctor: {:?}", local.err());
        assert!(local.unwrap().0.ansi_text.contains("doctor []"));
        assert_eq!(spawned(), 2);
        let again = serve("100.64.9.9:50000").await.expect("a remote caller inside the window is answered").0;
        assert_eq!(spawned(), 2, "a remote caller inside the window does not start a probe");
        assert_eq!(again.ansi_text, remote.ansi_text, "it reads the last remote run");
        let (code, _) = serve("127.0.0.1:50000").await.expect_err("this machine's own floor still holds");
        assert_eq!(code, StatusCode::TOO_MANY_REQUESTS);
    }

    /// (5.0) EVERY registered panel, served through the real handler and spawn
    /// path to a viewer that is not this machine: 200 (never a refusal for
    /// being remote), the child told to render its remote form, the shared
    /// redaction applied, stderr shown redacted, and one calm notice naming the
    /// command and this machine. This machine gets the child's own output,
    /// with no remote form asked for and nothing withheld. A panel added to
    /// the table is in `PANEL_IDS`, so it cannot skip this.
    #[tokio::test]
    #[serial_test::serial]
    async fn every_panel_serves_a_remote_caller_its_redacted_form_and_this_machine_in_full() {
        let dir = tempfile::tempdir().unwrap();
        let roster = dir.path().join("fleet.json");
        std::fs::write(
            &roster,
            r#"{"version":"2","machines":{"peerone":{"id":"peerone","address":"example-host.example:8765","added_unix_ms":1}}}"#,
        )
        .unwrap();
        // The child names the audience it was asked to render for, a roster
        // address, and writes a diagnostic to stderr.
        let child = dir.path().join("child.sh");
        std::fs::write(
            &child,
            "#!/bin/sh\necho \"audience=[$DARKMUX_PANEL_AUDIENCE] peer at example-host.example:8765\"\necho 'warning: something local' >&2\n",
        )
        .unwrap();
        std::fs::set_permissions(&child, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
        // (#2976) The first exec of a fresh executable is scanned; do it
        // outside the panel's spawn bound.
        let _ = std::process::Command::new(&child).output();
        let saved: Vec<(&str, Option<String>)> =
            ["DARKMUX_FLEET_FILE", "DARKMUX_MACHINE_ID", panel_audience::AUDIENCE_ENV].iter().map(|k| (*k, std::env::var(k).ok())).collect();
        std::env::set_var("DARKMUX_FLEET_FILE", &roster);
        std::env::set_var("DARKMUX_MACHINE_ID", "studio");
        // A daemon started from a shell that happened to export the audience
        // variable must still serve this machine in full.
        std::env::set_var(panel_audience::AUDIENCE_ENV, panel_audience::REMOTE);
        let serve = |id: &'static str, peer: &'static str| {
            let mut state = app_state();
            state.panels.child_exe = Some(child.clone());
            let mut headers = axum::http::HeaderMap::new();
            headers.insert(PANEL_HEADER, "1".parse().unwrap());
            headers.insert("host", "localhost:8765".parse().unwrap());
            panel_handler(
                Path(id.to_string()),
                axum::extract::RawQuery(None),
                Query(HashMap::new()),
                Some(axum::extract::ConnectInfo(peer.parse().unwrap())),
                headers,
                State(state),
            )
        };
        let mut failures = Vec::new();
        for id in PANEL_IDS {
            let remote = match serve(id, "100.64.1.2:50000").await {
                Ok(b) => serde_json::to_value(&b.0).unwrap(),
                Err((code, msg)) => {
                    failures.push(format!("{id}: a remote caller was refused {code}: {msg}"));
                    continue;
                }
            };
            let text = remote["ansi_text"].as_str().unwrap_or_default();
            if !text.contains("audience=[remote]") {
                failures.push(format!("{id}: the child was not asked for its remote form: {text}"));
            }
            if text.contains("example-host") {
                failures.push(format!("{id}: a roster address reached a remote caller: {text}"));
            }
            if remote["stderr_tail"] != "warning: something local" {
                failures.push(format!("{id}: a remote caller did not read stderr (redacted): {}", remote["stderr_tail"]));
            }
            let notice = remote["withheld"].as_str().unwrap_or_default();
            let command = format!("darkmux {}", panel_spec(id).unwrap().argv.join(" "));
            if !(notice.contains("shown on this machine only") && notice.contains(&command) && notice.contains("studio")) {
                failures.push(format!("{id}: no notice naming `{command}` and the machine: {notice:?}"));
            }
            let local = serde_json::to_value(&serve(id, "127.0.0.1:50000").await.unwrap().0).unwrap();
            let text = local["ansi_text"].as_str().unwrap_or_default();
            if !text.contains("audience=[] peer at example-host.example:8765") {
                failures.push(format!("{id}: this machine did not get the full output: {text}"));
            }
            if local["stderr_tail"] != "warning: something local" || local["withheld"].as_str().unwrap_or_default() != "" {
                failures.push(format!("{id}: this machine lost its stderr or was told something was withheld: {local}"));
            }
        }
        for (k, v) in saved {
            match v {
                Some(v) => std::env::set_var(k, v),
                None => std::env::remove_var(k),
            }
        }
        assert!(failures.is_empty(), "{}", failures.join("\n"));
    }

    // ── real panel output: colored, punctuated, linked ─────────────────

    fn real_redaction() -> Redaction {
        Redaction::from_parts(
            &[("studio", "studio.tailnet.example:8765"), ("mini", "100.64.1.2"), ("macbox", "mac.local")],
            &[],
            Some("/Users/kain".into()),
            Some("/Users/kainx/dm".into()),
        )
    }

    /// Every SGR form a panel child emits (the sequence ends in `m`, which is
    /// a word letter: the escape must be a token boundary).
    #[test]
    fn an_address_or_path_next_to_a_color_code_is_still_redacted() {
        let r = real_redaction();
        for (input, want) in [
            ("roster: \x1b[2m/Users/kain/.darkmux/fleet.json", "roster: \x1b[2m~/.darkmux/fleet.json"),
            ("\x1b[2mstudio.tailnet.example\x1b[0m", "\x1b[2m(address hidden)\x1b[0m"),
            ("\x1b[0mstudio.tailnet.example:8765", "\x1b[0m(address hidden)"),
            ("\x1b[2m100.64.1.2", "\x1b[2m(address hidden)"),
            ("\x1b[1mmac.local", "\x1b[1m(address hidden)"),
            ("\x1b[38;5;208m/Users/kain\x1b[39m", "\x1b[38;5;208m~\x1b[39m"),
        ] {
            assert_eq!(filtered(&r, input), want, "{input:?}");
        }
    }

    /// Punctuation after a token is a boundary: end of a sentence, a trailing dot.
    #[test]
    fn a_trailing_dot_does_not_protect_an_address_or_a_path() {
        let r = real_redaction();
        for (input, want) in [
            ("is studio.tailnet.example.", "is (address hidden)."),
            ("at 100.64.1.2.", "at (address hidden)."),
            ("in /Users/kain.", "in ~."),
            ("(100.64.1.2), mac.local.\n", "((address hidden)), (address hidden).\n"),
        ] {
            assert_eq!(filtered(&r, input), want, "{input:?}");
        }
        // A dot followed by a word character still continues the name.
        assert_eq!(filtered(&r, "100.64.1.2.5 mac.local.example"), "100.64.1.2.5 mac.local.example");
    }

    #[test]
    fn an_osc8_link_to_a_hidden_target_loses_its_target_and_keeps_its_label() {
        let r = real_redaction();
        let file = "\x1b]8;;file:///Users/kain/.darkmux/fleet.json\x1b\\fleet.json\x1b]8;;\x1b\\";
        assert_eq!(filtered(&r, file), "\x1b]8;;\x1b\\fleet.json\x1b]8;;\x1b\\");
        let url = "\x1b]8;id=1;http://studio.tailnet.example:8765/#lens=runs\x07label\x1b]8;;\x07";
        assert_eq!(filtered(&r, url), "\x1b]8;;\x1b\\label\x1b]8;;\x1b\\");
        // A link that names nothing private is left alone (the board's own links).
        let own = "\x1b]8;;/#mission=m-1\x1b\\m-1\x1b]8;;\x1b\\";
        assert_eq!(filtered(&r, own), own);
        // A hidden address in the LABEL is redacted as plain text.
        assert_eq!(
            filtered(&r, "\x1b]8;;/x\x1b\\studio.tailnet.example\x1b]8;;\x1b\\"),
            "\x1b]8;;/x\x1b\\(address hidden)\x1b]8;;\x1b\\"
        );
    }

    #[test]
    fn darkmux_home_is_trimmed_and_nested_at_a_path_boundary() {
        // `/Users/kainx/dm` is not inside `/Users/kain`, so it is its own rewrite.
        let r = real_redaction();
        assert_eq!(filtered(&r, "/Users/kainx/dm/cfg /Users/kain/a"), "$DARKMUX_HOME/cfg ~/a");
        // A trailing slash on either directory does not stop the match.
        let r = Redaction::from_parts(&[], &[], Some("/Users/kain/".into()), Some("/Volumes/x/dm/".into()));
        assert_eq!(filtered(&r, "/Users/kain/a /Volumes/x/dm/b"), "~/a $DARKMUX_HOME/b");
        // Inside HOME at a boundary: covered by the HOME rewrite.
        let r = Redaction::from_parts(&[], &[], Some("/Users/kain".into()), Some("/Users/kain/dm/".into()));
        assert_eq!(filtered(&r, "/Users/kain/dm/x"), "~/dm/x");
    }

    #[test]
    fn the_earlier_over_redaction_fixtures_still_hold_with_colors() {
        let r = Redaction::from_parts(&[("peerone", "mac"), ("peertwo", "studio")], &[], Some("/Users/kain".into()), None);
        let text = "\x1b[1mdarkmux machine list\x1b[0m\nmacos aarch64 analyst lmstudio-community/x\n";
        assert_eq!(filtered(&r, text), text);
    }

    // ── canonical forms of the directories ─────────────────────────────

    /// A directory given through a symlink (`/tmp` on macOS is `/private/tmp`)
    /// is printed by some verbs in its resolved form: both must be rewritten.
    #[cfg(unix)]
    #[test]
    fn a_symlinked_directory_is_redacted_in_its_canonical_form_too() {
        let dir = tempfile::tempdir().unwrap();
        let real = std::fs::canonicalize(dir.path()).unwrap().join("real");
        std::fs::create_dir_all(real.join("fx")).unwrap();
        let link = dir.path().join("link");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        let (link, real) = (link.to_string_lossy().to_string(), real.to_string_lossy().to_string());
        assert_ne!(link, real);
        // As DARKMUX_HOME (outside HOME) ...
        let r = Redaction::from_parts(&[], &[], Some("/Users/kain".into()), Some(link.clone()));
        assert_eq!(filtered(&r, &format!("{link}/a {real}/fx")), "$DARKMUX_HOME/a $DARKMUX_HOME/fx");
        // ... and as HOME.
        let r = Redaction::from_parts(&[], &[], Some(format!("{link}/")), None);
        assert_eq!(filtered(&r, &format!("{link}/a {real}/fx")), "~/a ~/fx");
    }

    /// A DARKMUX_HOME whose canonical form lies inside HOME's canonical form is
    /// covered by the HOME rewrite, at a path boundary.
    #[cfg(unix)]
    #[test]
    fn a_canonical_darkmux_home_inside_home_is_not_listed_twice() {
        let dir = tempfile::tempdir().unwrap();
        let home = std::fs::canonicalize(dir.path()).unwrap();
        std::fs::create_dir_all(home.join("dm")).unwrap();
        let (h, dm) = (home.to_string_lossy().to_string(), home.join("dm").to_string_lossy().to_string());
        let r = Redaction::from_parts(&[], &[], Some(h.clone()), Some(dm.clone()));
        assert_eq!(filtered(&r, &format!("{dm}/x")), "~/dm/x");
        assert!(r.dirs().iter().all(|(_, label)| *label == "~"), "{:?}", r.dirs());
        // A sibling that merely shares the prefix is not inside it.
        std::fs::create_dir_all(format!("{h}x/dm")).ok();
        let r = Redaction::from_parts(&[], &[], Some(h.clone()), Some(format!("{h}x/dm")));
        assert_eq!(filtered(&r, &format!("{h}x/dm/y")), "$DARKMUX_HOME/y");
        std::fs::remove_dir_all(format!("{h}x")).ok();
    }

    // ── escapes a remote caller is sent ────────────────────────────────

    #[test]
    fn only_sgr_and_osc8_survive_for_a_remote_caller() {
        let r = real_redaction();
        for (input, want) in [
            // Non-8 OSC (window title, cwd report), BEL and ST terminated: dropped whole.
            ("a\x1b]0;/Users/kain/x\x07b", "ab"),
            ("a\x1b]7;file:///Users/kain\x1b\\b", "ab"),
            // Non-SGR CSI dropped; SGR kept, rebuilt from its parameters.
            ("a\x1b[2Jb\x1b[1;31mc\x1b[0m", "ab\x1b[1;31mc\x1b[0m"),
            ("\x1b[mx", "\x1b[mx"),
            // A charset designation is dropped.
            ("a\x1b(Bb", "ab"),
            // OSC 8: params stripped, target kept, terminator normalized to ST.
            ("\x1b]8;id=7;/#mission=m-1\x07m-1\x1b]8;;\x07", "\x1b]8;;/#mission=m-1\x1b\\m-1\x1b]8;;\x1b\\"),
        ] {
            assert_eq!(filtered(&r, input), want, "{input:?}");
        }
    }

    /// An escape that is malformed or never terminated loses only its
    /// introducer: the rest is text, and is redacted as text.
    #[test]
    fn an_unterminated_or_malformed_escape_is_text_that_is_redacted() {
        let r = real_redaction();
        for (input, want) in [
            ("\x1b[/Users/kain/.darkmux", "~/.darkmux"),
            ("x \x1b]0;/Users/kain no terminator", "x 0;~ no terminator"),
            ("x \x1b]8;;http://studio.tailnet.example:8765/", "x 8;;http://(address hidden)/"),
            ("tail \x1b[", "tail "),
            ("tail \x1b", "tail "),
        ] {
            assert_eq!(filtered(&r, input), want, "{input:?}");
        }
    }

    /// U+009B is a one-character CSI: the same rules as `ESC [`.
    #[test]
    fn a_c1_csi_is_treated_as_an_escape() {
        let r = real_redaction();
        assert_eq!(filtered(&r, "a\u{9b}2Jb"), "ab");
        assert_eq!(filtered(&r, "\u{9b}1mbold"), "\x1b[1mbold");
        let got = filtered(&r, "\u{9b}/Users/kain/x");
        assert!(!got.contains("kain") && !got.contains('\u{9b}'), "{got:?}");
    }

    /// An OSC 8 target carrying control characters is not passed on.
    #[test]
    fn an_osc8_target_with_a_control_character_is_dropped() {
        let r = real_redaction();
        assert_eq!(filtered(&r, "\x1b]8;;a\x01b\x1b\\L"), "\x1b]8;;\x1b\\L");
    }
}
