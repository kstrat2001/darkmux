//! (#2695/#2697/#2698) One RAII guard that pins EVERY darkmux write
//! destination under a single throwaway root.
//!
//! # Why this exists
//!
//! Test isolation in this repo used to be enforced **per variable**: each
//! test suite grew its own little guard pinning the one environment
//! variable whose leak had most recently been noticed — a `CrewGuard` for
//! `DARKMUX_CREW_DIR`, a `FlowsDirGuard` for `DARKMUX_FLOWS_DIR`, a
//! `DarkmuxHomeGuard` for `DARKMUX_HOME` (later amended to also pin the
//! crew dir, because `user_state_root()` resolves that first). Every such
//! guard is correct about its own variable and silent about the other
//! twelve, so each newly-added write destination leaks until someone
//! notices — and "someone notices" has meant a reviewer running with a
//! sentinel directory, four separate times.
//!
//! Three measured consequences, filed as #2697, #2695 and #2698:
//!
//! * `darkmux-serve`'s `runs::` tests hold a `CrewGuard` — which isolates
//!   mission state — and then emit fabricated `mission close` records
//!   ("mission errored", "mission completed (degraded)") through a flow
//!   sink that resolves `flows_dir()`, which that guard does not pin. Once
//!   in the operator's stream those records feed `run list`, the viewer,
//!   and — when `audit.enabled` — the hash-chained audit sink, where they
//!   cannot be removed without breaking the chain.
//! * The isolation guard test itself only neutralized `DARKMUX_HOME`, so
//!   it went RED for anyone who pinned a scratch `DARKMUX_CREW_DIR` (the
//!   careful thing to do) and stayed GREEN for anyone who pinned nothing
//!   — exactly backwards from what a guard is for.
//! * A guard's restore could be made a no-op with the suite fully green,
//!   because nothing asserted the restore.
//!
//! The structural answer is one guard that pins the whole set, derived
//! from the resolvers rather than from the last incident, plus a keystone
//! test that asserts the *isolation property* over that set. Remembering
//! to combine three guards does not scale; that failure to scale is the
//! bug class itself.
//!
//! # What "the whole set" means
//!
//! Two lists, because the variables are not all the same kind of thing.
//!
//! [`PINNED_STATE_VARS`] are variables that only ever NAME a location.
//! Setting one to a path under the guard's root is pure isolation: no
//! behavior changes, the write just lands somewhere throwaway.
//!
//! [`CLEARED_STATE_VARS`] are variables whose PRESENCE carries meaning
//! beyond the location, so pinning them would change what the code under
//! test does. `DARKMUX_AUDIT_DIR` is the sharp one — `audit_enabled()` is
//! true whenever it is set, so pinning it would switch the hash-chained
//! sink ON for every guarded test. These are removed instead, which is
//! still isolation: with the override gone, each falls back to a default
//! derived from the root, and the root is pinned.
//!
//! # The residual, stated out loud
//!
//! Clearing the env tier does not neutralize the **config tier**. A
//! resolver reads `env > config.json > default`, and a crate whose test
//! build does not enable `darkmux-types/test-support` gets a `config()`
//! that reads the operator's real `~/.darkmux/config.json`. So a
//! destination the operator has relocated in config — this machine has
//! `dirs.notebook` set, and `hooks.outbox_dir` has no env var at all —
//! can still escape a root-only pin. That is precisely why
//! [`PINNED_STATE_VARS`] pins the env tier for every destination that HAS
//! an env var: the env tier outranks config, so pinning it closes the
//! config tier too. The destinations left exposed are the ones with no
//! env var, and the keystone test in `darkmux-doctor`
//! (`every_state_root_resolves_under_an_isolated_darkmux_home`) asserts
//! over those as well, so the exposure is measured rather than assumed.
//!
//! # Two further limits, stated rather than discovered later
//!
//! **`DARKMUX_HOME` is itself a presence-changes-behavior variable, and it
//! is in [`PINNED_STATE_VARS`] anyway.** `paths::resolve` returns
//! `paths_from_root(root, Scope::User)` from the `DARKMUX_HOME` tier
//! BEFORE it reaches the `ResolveScope` match, so while this guard is held
//! `ForceProject` and `ForceUser` resolve to the same root with
//! `scope = User`. That is the right trade — the whole point is one root —
//! but it means the project-scope branch is unreachable under the guard,
//! and a test that asserts on `DarkmuxPaths::scope` or on project-vs-user
//! resolution is silently testing nothing while holding an
//! [`IsolatedState`]. Such a test has to drive `paths::resolve` with
//! `DARKMUX_HOME` removed.
//!
//! **The subpath half of the contract is weaker than the root half.**
//! `every_pinned_variable_points_under_the_isolated_root` asserts
//! `path == state.join(subpath)` using the same list entry it read `path`
//! from, so it is tautological in `subpath`: mutating an entry's subpath
//! (`"flows"` → `"flowz"`) is MISSED by that test and by the doctor
//! keystone, and caught only incidentally by
//! `darkmux-crew`'s `lifecycle::` tests, which happen to hardcode
//! `guard.join("flows")`. The root claim IS held; the "mirrors the
//! built-in default layout" claim is documentation, and the
//! `identity.md`/`identity.json` entry below is what a drifted one looks
//! like. Blast radius is low — a wrong subpath still isolates — but a test
//! planting a fixture at the production path will not find it.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

/// Variables that NAME a write destination and nothing more. The guard
/// points each at a path under its own throwaway root.
///
/// Each entry is `(variable, subpath under the root)`. The subpath mirrors
/// the layout the built-in defaults produce, so a guarded test sees the
/// same shape it would see in production — just rooted somewhere
/// disposable.
///
/// Derived from the resolvers, NOT from any incident: every accessor in
/// `config_access` that returns a path and reads an env tier appears here,
/// plus `darkmux_crew::loader::user_state_root`'s `DARKMUX_CREW_DIR`
/// (which outranks `DARKMUX_HOME`, and is the one that made the keystone
/// test fail) and the `DARKMUX_HOME` root itself.
pub const PINNED_STATE_VARS: &[(&str, &str)] = &[
    // The root. Every default below derives from it; pinned first so the
    // rest are belt-and-braces rather than load-bearing.
    ("DARKMUX_HOME", ""),
    // OUTRANKS `DARKMUX_HOME` in `user_state_root()`/`crew_root()`, which
    // is what every `missions/`, `phases/`, `roles/`, `crews/` and
    // `skills/` write resolves through. The variable names the directory
    // CONTAINING those subdirs, so it pins to the root itself, not a
    // subpath.
    ("DARKMUX_CREW_DIR", ""),
    ("DARKMUX_FLOWS_DIR", "flows"),
    ("DARKMUX_FINDINGS_DIR", "findings"),
    ("DARKMUX_MODS_DIR", "mods"),
    ("DARKMUX_LAB_DIR", "runs"),
    ("DARKMUX_NOTEBOOK_DIR", "notebook"),
    ("DARKMUX_ACK_DIR", "acks"),
    ("DARKMUX_FLEET_FILE", "fleet.json"),
    // `.md`, not `.json`: `crew::dispatch::identity_path()`'s default is
    // `<root>/identity.md` (documented at
    // `config_access::identity_path_override`). The subpath is the SHAPE
    // claim this list makes, so a wrong one hands a future test that
    // plants a fixture at `state.join("identity.md")` a silent `None`.
    ("DARKMUX_IDENTITY_PATH", "identity.md"),
];

/// Variables the guard REMOVES rather than pins, because their presence
/// means something beyond "the destination is here".
///
/// * `DARKMUX_AUDIT_DIR` — `config_access::audit_enabled()` is true
///   whenever this is set (the documented env path to turning the
///   hash-chained sink on; there is deliberately no
///   `DARKMUX_AUDIT_ENABLED`). Pinning it would enable audit for every
///   guarded test. Removed instead: the caller's default is
///   `<root>/audit`, and the root is pinned.
/// * `DARKMUX_PROFILES` — short-circuits the whole profile search chain
///   and attributes load errors to itself. Removing it restores the
///   normal chain, whose first user-scope candidate derives from the
///   pinned root.
/// * `DARKMUX_TEMPLATES_DIR` / `DARKMUX_SKILLS_DIR` — search-path
///   PREPENDS (read side, not write). An ambient value would have a
///   guarded test reading the operator's real templates/skills; removing
///   it leaves only the root-derived candidates.
/// * `DARKMUX_REDIS_URL` (#2736 item 2) — a live, NETWORK-ADDRESSED
///   destination, not a path under any root this guard controls. Pinning
///   it to nothing meaningful would be no isolation at all; removing it
///   is what actually stops a guarded test's flow writes from reaching a
///   real Redis. Measured before this entry existed: neither
///   [`neutralize_state_vars`] nor [`StateLeakSentinel::apply`] touched
///   it, so both left it exactly as ambient as an unguarded spawn.
///   `darkmux_flow::isolate_test_env_once()` scrubs it too, in-process
///   and lazily, for the same reason from the flow crate's side — this
///   entry is what makes EVERY guard here do the same thing, spawn or
///   no.
/// * `DARKMUX_FLEET_SNAPSHOT_FILE` (#2736 item 2) — names a handoff file
///   `darkmux-serve` writes for a spawned CLI panel to read INSTEAD OF a
///   live fleet read (`fleet_records_for_runs`). An ambient value here
///   would have a guarded test silently reading someone else's snapshot
///   rather than the live path it thinks it is exercising; removing it
///   restores the live path, the same shape as `DARKMUX_PROFILES` above.
///
/// **`DARKMUX_ENV_AUDIT_LOG` is deliberately NOT here.** It looks like it
/// belongs — `still set` is exactly the shape the two entries above
/// closed — but it is INSTRUMENTATION CONFIG, not a state destination: it
/// tells `env_audit::audit_env_read` where to log which `DARKMUX_*` keys
/// get READ, and at least one existing test
/// (`darkmux-doctor`'s `the_guards_variable_list_covers_every_destination_
/// the_resolvers_read`) sets it BEFORE constructing [`IsolatedState`]
/// specifically so every read the guard's OWN construction performs is
/// captured. Clearing it here would silently blind that test the moment
/// the guard runs — measured, not assumed, before this doc paragraph was
/// written. Out of scope by construction, the same way a non-file
/// destination is (see [`StateLeakSentinel`]'s doc).
pub const CLEARED_STATE_VARS: &[&str] = &[
    "DARKMUX_AUDIT_DIR",
    "DARKMUX_PROFILES",
    "DARKMUX_TEMPLATES_DIR",
    "DARKMUX_SKILLS_DIR",
    "DARKMUX_REDIS_URL",
    "DARKMUX_FLEET_SNAPSHOT_FILE",
];

/// (#2698) RAII pin for the runtime inactivity budget —
/// `DARKMUX_INACTIVITY_TIMEOUT_SECONDS`, a documented operator knob read
/// LIVE per access.
///
/// Same bug class as the directory leaks above, one axis over: a fixture
/// whose assertion is measured against a threshold the ENVIRONMENT owns.
/// `stale_after_ms()` is `inactivity_timeout_seconds() * 2`, so a test
/// that places a record "5 seconds after the last one" and expects that to
/// read *fresh* is quietly asserting that the operator has not exported a
/// small budget — and one that places a sample a day later and expects it
/// *excluded* is asserting they have not exported a large one. Measured on
/// `crates/darkmux-serve/src/lib_tests.rs`: `cargo test -p darkmux-serve
/// --lib` is EXIT=0 with the knob unset and at `600`, and EXIT=101 at both
/// `1` and `86400`. An operator who has set it ran a red suite for a
/// reason unconnected to their change.
///
/// This is the clock rule applied to the DENOMINATOR: freeze the budget
/// the distance is measured against, not just the timestamps.
///
/// It lives here beside [`IsolatedState`] because three separate copies of
/// this guard had already grown in three test modules (`mission_status`,
/// `runs`, and — absent entirely — `lib_tests`, which is why those two
/// tests were left behind). Copy number four is not the fix.
///
/// Caller must hold `#[serial_test::serial]`.
pub struct InactivityBudget {
    prev: Option<OsString>,
}

impl InactivityBudget {
    const VAR: &'static str = "DARKMUX_INACTIVITY_TIMEOUT_SECONDS";

    /// Pin the budget to an explicit number of seconds.
    pub fn seconds(secs: u64) -> Self {
        let prev = std::env::var_os(Self::VAR);
        // SAFETY: caller holds #[serial_test::serial].
        unsafe { std::env::set_var(Self::VAR, secs.to_string()) };
        Self { prev }
    }

    /// Pin the budget to the BUILT-IN default by clearing the override —
    /// the only way to reach the default tier, which a test asserting the
    /// shipped value has to do.
    pub fn unset() -> Self {
        let prev = std::env::var_os(Self::VAR);
        // SAFETY: caller holds #[serial_test::serial].
        unsafe { std::env::remove_var(Self::VAR) };
        Self { prev }
    }
}

impl Drop for InactivityBudget {
    fn drop(&mut self) {
        // SAFETY: caller holds #[serial_test::serial].
        unsafe {
            match &self.prev {
                Some(v) => std::env::set_var(Self::VAR, v),
                None => std::env::remove_var(Self::VAR),
            }
        }
    }
}

/// Pins every darkmux state destination under one throwaway root for the
/// lifetime of the value, and restores the previous environment exactly on
/// drop — including variables that were UNSET, which are removed again
/// rather than left behind as the empty string.
///
/// The caller must hold `#[serial_test::serial]`: this mutates
/// process-global environment, and `std::env::set_var` is `unsafe` in
/// Rust 2024 for precisely that reason.
///
/// ```ignore
/// #[test]
/// #[serial_test::serial]
/// fn writes_nothing_into_the_operators_tree() {
///     let state = IsolatedState::new();
///     // … exercise code that writes darkmux state …
///     assert!(some_written_path.starts_with(state.path()));
/// }
/// ```
pub struct IsolatedState {
    tmp: tempfile::TempDir,
    /// Every variable this guard touched, with the value it displaced.
    /// `None` means the variable was unset and must be removed again.
    prev: Vec<(&'static str, Option<OsString>)>,
}

impl IsolatedState {
    /// Pin everything. Creates the root eagerly (a `TempDir`), then sets
    /// [`PINNED_STATE_VARS`] and removes [`CLEARED_STATE_VARS`].
    pub fn new() -> Self {
        let tmp = tempfile::TempDir::new().expect("IsolatedState: could not create a temp root");
        let root = tmp.path().to_path_buf();
        let mut prev: Vec<(&'static str, Option<OsString>)> = Vec::new();

        for (var, subpath) in PINNED_STATE_VARS {
            prev.push((var, std::env::var_os(var)));
            let target = if subpath.is_empty() { root.clone() } else { root.join(subpath) };
            // SAFETY: caller holds #[serial_test::serial].
            unsafe { std::env::set_var(var, &target) };
        }
        for var in CLEARED_STATE_VARS {
            prev.push((var, std::env::var_os(var)));
            // SAFETY: caller holds #[serial_test::serial].
            unsafe { std::env::remove_var(var) };
        }

        Self { tmp, prev }
    }

    /// The throwaway root. Every pinned destination is at or under it.
    pub fn path(&self) -> &Path {
        self.tmp.path()
    }

    /// A path under the isolated root, for a test that wants to plant a
    /// fixture where a resolver will find it.
    pub fn join(&self, sub: impl AsRef<Path>) -> PathBuf {
        self.tmp.path().join(sub)
    }
}

impl Default for IsolatedState {
    fn default() -> Self {
        Self::new()
    }
}

/// (#2710) [`IsolatedState`]'s SUBPROCESS half: strip every darkmux state
/// variable from a child's environment, so a spawned binary derives all of
/// them from whatever root the caller pins next instead of inheriting the
/// ambient shell's.
///
/// # Why this lives here and not in a test binary
///
/// It used to be a private `fn` inside the `tests/cli.rs` test BINARY. A
/// test binary exports nothing, so every other integration target that
/// spawns darkmux had the choice between copying it and doing nothing, and
/// four of them did nothing — `state_files_owner_only_mode`,
/// `fleet_concurrent_add_no_lost_writes`,
/// `lab_concurrent_register_no_lost_writes` and `lab_init_idempotent`, each
/// pinning `HOME`, removing `DARKMUX_HOME`, and neutralizing none of the
/// other twelve. Measured at that head with the dir set exported to
/// sentinels and a fake `$HOME`: `state_files_owner_only_mode` wrote
/// `fleet.json`, its lock, and `audit/<today>.jsonl` holding a BLAKE3-
/// chained `mission start` for a fabricated mission, stamped with the
/// operator's real `machine_id` and `machine_uid`. Chained records cannot
/// be removed without breaking the chain.
///
/// One copy per call site is the bug class #2697 named. This is the fix
/// for it: one definition, in the crate every integration target already
/// depends on.
///
/// # Ordering — call this FIRST, then pin
///
/// `std::process::Command` applies `.env` and `.env_remove` in call order.
/// A `.env` AFTER this call wins; a `.env` BEFORE it is erased. So the one
/// idiom everywhere is:
///
/// ```ignore
/// let mut cmd = std::process::Command::new(bin);
/// neutralize_state_vars(&mut cmd);                   // clear the whole set
/// cmd.env("HOME", root).env("DARKMUX_HOME", root);   // then pin
/// ```
///
/// # `DARKMUX_HOME` is cleared like the rest, deliberately
///
/// It gets no exception here. A caller that wants it pinned sets it after
/// (`tests/cli.rs`'s `darkmux_std_cmd`); a caller whose whole point is to
/// exercise DEFAULT resolution simply does not set it, and gets a removal
/// rather than the ambient shell's value. That second shape is what the
/// four targets above are for, and it is exactly what made them the most
/// exposed of the set: they pinned `HOME` and then inherited a
/// `DARKMUX_CREW_DIR` / `DARKMUX_AUDIT_DIR` that OUTRANKS it.
///
/// Taken from [`PINNED_STATE_VARS`] and [`CLEARED_STATE_VARS`] rather than
/// written out, so a destination added to either list is covered at every
/// spawn site the moment it lands.
pub fn neutralize_state_vars(cmd: &mut std::process::Command) {
    for (var, _) in PINNED_STATE_VARS {
        cmd.env_remove(var);
    }
    for var in CLEARED_STATE_VARS {
        cmd.env_remove(var);
    }
}


/// (#2717) [`IsolatedState`]'s EXECUTION-SIDE counterpart: a throwaway
/// state tree a whole test target can be run under, plus a census of
/// everything that landed in it.
///
/// # Why this exists, when a text scan already does
///
/// `tests/cli.rs`'s
/// `every_darkmux_spawn_in_the_tests_dir_goes_through_an_isolating_helper`
/// scans SOURCE for four recognized spellings inside a marker fence. Four
/// escapes were demonstrated against it, each leaving it at EXIT=0 while
/// files leaked:
///
/// * a resolver named anything other than the one recognized function
///   name is invisible, and so is a `PATH`-resolved
///   `Command::new("darkmux")`;
/// * deleting the single token that caught four of the five original
///   offenders leaves the guard green, because the `>= 2` anti-vacuity
///   floor is still met by the remaining fences;
/// * a fenced block that spawns darkmux raw while calling the neutralizer
///   on a DECOY `Command` passes both assertions;
/// * the scan checks that the call APPEARS on a non-`//` line, never that
///   it RUNS — so a block comment (`/* … */`, which `is_code` does not
///   exclude and which is what an editor produces when a selection is
///   commented out), a string literal, or a `#[cfg(any())]` item all
///   satisfy it.
///
/// Adding tokens loses to the next spelling; widening the walk loses to
/// the decoy. This observes the EFFECT instead: run the target under a
/// tree nothing legitimate should touch, then count. That catches every
/// shape at once — unknown spellings, decoys, grandchildren spawned
/// through a shell, in-process writes that never spawn anything at all,
/// and destinations in directories nobody thought to walk. It is also the
/// measurement that found every leak on record here (#2693, #2697, #2708,
/// #2710, #2718); none was found by reading code.
///
/// # What it pins, and why BOTH halves are counted
///
/// Two failure shapes, so two destinations:
///
/// * a write that HONORS the state variables lands under [`root`]; and
/// * a write that ignores them and re-derives from `$HOME` — which is
///   what an ordinary developer machine looks like, since almost nobody
///   exports these — lands under `<home>/.darkmux`.
///
/// [`census`] counts both. Counting only the convenient one is a measured
/// failure mode: one sweep counted mission directories and missed flow
/// records entirely; another counted two directories and missed 8,263
/// temp directories (#2707).
///
/// # It PINS the root and CLEARS everything else, and that asymmetry is
/// the whole design
///
/// **A variable that outranks the root is the right thing to CLEAR and the
/// wrong thing to PIN.** `DARKMUX_CREW_DIR` outranks `DARKMUX_HOME` in
/// `user_state_root()`; `DARKMUX_LAB_DIR`, `DARKMUX_FINDINGS_DIR` and
/// `DARKMUX_MODS_DIR` outrank it in their own accessors. Pinning one of
/// those to a single scratch path does not isolate the target — it
/// OVERRIDES every per-test guard inside it with one shared value, so
/// tests that isolate themselves correctly are forced onto one directory
/// and collide. Measured on `darkmux-lab`: with `DARKMUX_CREW_DIR` pinned,
/// 31 crawl tests fail on the shared directory (a rename hits `ENOENT`
/// after a sibling's cleanup); with the pin removed, 2,416 of 2,416 pass.
/// Same binary, same diff — the failures were the harness, not the code.
///
/// Clearing has neither problem: each test's own isolation applies
/// normally, and anything that isolates NOTHING falls through to the
/// pinned root or to `$HOME`, where the census sees it. So the sentinel
/// pins exactly one variable — `DARKMUX_HOME`, the root, which every
/// default derives from and which any test's own pin overrides — and
/// removes the rest. It is the same asymmetry [`IsolatedState`] makes for
/// the same reason, one layer out.
///
/// # The audit chain: counted, not forced, and the gap is named
///
/// `DARKMUX_AUDIT_DIR` is CLEARED like the rest, even though its presence
/// is what enables the hash-chained sink. Pinning it would turn the audit
/// destination on — which is the destination that matters most, since an
/// append-only chained record cannot be removed without breaking the chain
/// (#2697) — but it is also a variable that outranks the root, so the rule
/// above applies to it unchanged, and pinning it binds the whole binary's
/// memoized default sink before any test runs (#2730).
///
/// [`census`] still WALKS both `audit/` directories and sizes them by
/// record, so a write that reaches one is reported. What the check cannot
/// do is make one happen: with the variable unset and `config()` empty in
/// test builds, the sink is off, which is exactly what an ordinary
/// developer machine looks like. The operator who has turned it on is
/// covered by #2730 rather than by this harness.
///
/// # The census is FILE-ONLY, stated so this check is never over-trusted
/// (#2736 item 2)
///
/// [`census`] walks two directory trees on disk and sizes what it finds
/// there. That is a complete answer for every destination this workspace
/// resolves to a PATH — which is every entry in [`PINNED_STATE_VARS`] —
/// but it is structurally no answer at all for a destination that is
/// never a path in the first place. `DARKMUX_REDIS_URL` names a live
/// network endpoint, not a file; a write that reached one would leave
/// nothing on disk for [`census`] to find, clean or not. `CLEARED_STATE_
/// VARS` closes the REALISTIC exposure by removing the variable so the
/// guarded process cannot address a Redis at all — but that is a
/// mitigation at the CONFIGURATION layer, not a capability this census
/// gained. A future non-file destination (another network sink, a
/// message queue) is out of scope for [`census`] BY CONSTRUCTION, not by
/// oversight one entry away from being fixed; closing that gap needs a
/// different instrument, not one more directory in this walk.
///
/// [`root`]: StateLeakSentinel::root
/// [`census`]: StateLeakSentinel::census
pub struct StateLeakSentinel {
    tmp: tempfile::TempDir,
}

/// What [`StateLeakSentinel::census`] found. Files, never exit codes.
///
/// The exit code is deliberately NOT part of the verdict. In #2710's own
/// measurements three of four targets went red under leak conditions but
/// ALL FOUR leaked, and one stayed green while leaking — so a check that
/// keys on the status observes the wrong thing, in the direction that
/// misses defects.
#[derive(Debug, Default)]
pub struct LeakCensus {
    /// Every regular file under the sentinel root, relative to it.
    pub under_root: Vec<PathBuf>,
    /// Every regular file under `<home>/.darkmux`, relative to that.
    pub under_home: Vec<PathBuf>,
    /// `(path, record count)` for each audit-chain file, so a chained
    /// append is reported by SIZE and not merely by existence.
    pub audit_records: Vec<(PathBuf, usize)>,
    /// Orphaned temp directories left in the sentinel's `TMPDIR`.
    /// REPORTED, never asserted on: a `TempDir` legitimately leaks its
    /// directory when a test panics, so a non-zero count here is a lead
    /// (#2707's 8,263) rather than a finding.
    pub orphan_tempdirs: usize,
}

impl LeakCensus {
    /// The number the assertion is made on: files that reached a darkmux
    /// state destination.
    pub fn leaked_files(&self) -> usize {
        self.under_root.len() + self.under_home.len()
    }

    /// A human-readable census, listing at most `cap` paths per
    /// destination so one badly-behaved target cannot bury the rest.
    pub fn report(&self, cap: usize) -> String {
        let mut out = String::new();
        let mut section = |label: &str, files: &[PathBuf]| {
            out.push_str(&format!("    {label}: {} file(s)\n", files.len()));
            for f in files.iter().take(cap) {
                out.push_str(&format!("      {}\n", f.display()));
            }
            if files.len() > cap {
                out.push_str(&format!("      … and {} more\n", files.len() - cap));
            }
        };
        section("under the pinned state root", &self.under_root);
        section("under <sentinel home>/.darkmux", &self.under_home);
        if !self.audit_records.is_empty() {
            out.push_str("    hash-chained audit records (cannot be removed without \
                          breaking the chain):\n");
            for (p, n) in &self.audit_records {
                out.push_str(&format!("      {n} record(s) in {}\n", p.display()));
            }
        }
        if self.orphan_tempdirs > 0 {
            out.push_str(&format!(
                "    (reported, not asserted) {} orphaned temp director(ies)\n",
                self.orphan_tempdirs
            ));
        }
        out
    }
}

impl StateLeakSentinel {
    /// Create the tree. Nothing is written into it by this call, so a
    /// census taken immediately after is empty by construction.
    pub fn new() -> Self {
        let tmp =
            tempfile::TempDir::new().expect("StateLeakSentinel: could not create a temp root");
        for sub in ["home", "root", "tmp"] {
            std::fs::create_dir_all(tmp.path().join(sub))
                .expect("StateLeakSentinel: could not create the sentinel tree");
        }
        Self { tmp }
    }

    /// The stand-in `$HOME`. Verified to be what `dirs::home_dir()`
    /// returns while it is applied — see
    /// `the_sentinel_home_is_what_dirs_home_dir_resolves_to`, which is the
    /// check that makes every `under_home` count meaningful rather than
    /// assumed.
    pub fn home(&self) -> PathBuf {
        self.tmp.path().join("home")
    }

    /// The pinned darkmux root. Every `DARKMUX_*` destination points at or
    /// under it.
    pub fn root(&self) -> PathBuf {
        self.tmp.path().join("root")
    }

    /// Point a child process at the sentinel: redirect `$HOME`, pin
    /// `DARKMUX_HOME` to [`root`](Self::root), and REMOVE every other
    /// darkmux state variable.
    ///
    /// One pin, and it is the root. See the type doc for why every
    /// variable that outranks the root is cleared instead — pinning one
    /// overrides the per-test isolation inside the target and manufactures
    /// failures that look like defects.
    ///
    /// `CARGO_HOME` / `RUSTUP_HOME` are re-pinned to whatever the parent
    /// resolved, because moving `$HOME` would otherwise send cargo's own
    /// registry and toolchain into the sentinel and drown the census in
    /// build artifacts.
    pub fn apply(&self, cmd: &mut std::process::Command) {
        cmd.env("HOME", self.home());
        cmd.env("TMPDIR", self.tmp.path().join("tmp"));
        cmd.env("DARKMUX_HOME", self.root());
        for (var, _) in PINNED_STATE_VARS.iter().filter(|(v, _)| *v != "DARKMUX_HOME") {
            cmd.env_remove(var);
        }
        for var in CLEARED_STATE_VARS {
            cmd.env_remove(var);
        }
        for var in ["CARGO_HOME", "RUSTUP_HOME"] {
            if let Some(v) = std::env::var_os(var) {
                cmd.env(var, v);
            } else if let Some(home) = dirs::home_dir() {
                let fallback = match var {
                    "CARGO_HOME" => home.join(".cargo"),
                    _ => home.join(".rustup"),
                };
                cmd.env(var, fallback);
            }
        }
    }

    /// Count everything that landed. See [`LeakCensus`].
    pub fn census(&self) -> LeakCensus {
        fn walk(dir: &Path, base: &Path, out: &mut Vec<PathBuf>) {
            let Ok(entries) = std::fs::read_dir(dir) else { return };
            for entry in entries.flatten() {
                let path = entry.path();
                match entry.file_type() {
                    Ok(t) if t.is_dir() => walk(&path, base, out),
                    Ok(_) => out.push(path.strip_prefix(base).unwrap_or(&path).to_path_buf()),
                    Err(_) => {}
                }
            }
        }

        let mut census = LeakCensus::default();
        let root = self.root();
        walk(&root, &root, &mut census.under_root);
        census.under_root.sort();

        let home_state = self.home().join(".darkmux");
        walk(&home_state, &home_state, &mut census.under_home);
        census.under_home.sort();

        for audit_dir in [root.join("audit"), home_state.join("audit")] {
            let Ok(entries) = std::fs::read_dir(&audit_dir) else { continue };
            let mut found: Vec<(PathBuf, usize)> = entries
                .flatten()
                .map(|e| e.path())
                .filter(|p| p.is_file())
                .map(|p| {
                    let n = std::fs::read_to_string(&p)
                        .map(|s| s.lines().filter(|l| !l.trim().is_empty()).count())
                        .unwrap_or(0);
                    (p, n)
                })
                .collect();
            found.sort();
            census.audit_records.extend(found);
        }

        census.orphan_tempdirs = std::fs::read_dir(self.tmp.path().join("tmp"))
            .map(|e| e.flatten().count())
            .unwrap_or(0);
        census
    }
}

impl Default for StateLeakSentinel {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for IsolatedState {
    fn drop(&mut self) {
        for (var, prev) in &self.prev {
            // SAFETY: caller holds #[serial_test::serial].
            unsafe {
                match prev {
                    Some(v) => std::env::set_var(var, v),
                    None => std::env::remove_var(var),
                }
            }
        }
    }
}

// ─── (#2707) Per-process scratch directories that do not accumulate ───
//
// A test process that pins nothing still needs somewhere to write. Five
// sites answered that the same way — `$TMPDIR/<name>-<pid>`, created by
// hand, never removed — and the result was measured on one developer
// machine as 12,745 entries in the temp directory, 8,263 of them from
// `darkmux-flow-test-<pid>` alone and 297 from `darkmux-cli-tests-<pid>`,
// each of the latter holding a whole isolated `$HOME` tree.
//
// Disk is the least of it. Every create and delete flows through the OS
// filesystem-event daemon that feeds the desktop index, so an unbounded
// tree is a permanent background cost on an indexed volume; per-pid
// naming makes collisions unlikely rather than impossible; and — the one
// that actually cost review time — a leak audit that counts files under
// `~/.darkmux` and reports zero is scoped to two directories, not to "no
// test wrote anywhere it should not have". This whole population is
// invisible to exactly that method.
//
// The fallback is not the bug, so it is not removed. What is removed is
// the abandonment.

/// Every per-process scratch directory this process has handed out, keyed
/// by prefix.
///
/// Doubles as the memo (one directory per prefix per process, so repeated
/// calls are stable and cheap) and as the removal list
/// [`remove_registered_scratch_dirs`] walks at exit.
static SCRATCH_DIRS: std::sync::OnceLock<std::sync::Mutex<std::collections::BTreeMap<String, PathBuf>>> =
    std::sync::OnceLock::new();

fn scratch_dirs() -> &'static std::sync::Mutex<std::collections::BTreeMap<String, PathBuf>> {
    SCRATCH_DIRS.get_or_init(|| std::sync::Mutex::new(std::collections::BTreeMap::new()))
}

/// Remove every scratch directory this process registered.
///
/// Registered once with `atexit(3)`, so it runs when the process ends
/// normally — `main` returning, or `std::process::exit`.
///
/// # Why not `Drop`
///
/// There is no value to drop. The directory has to outlive every caller
/// (a `LocalFileSink` resolves it per record write, for the whole life of
/// the test binary), so it lives in a `static` — and Rust never runs
/// destructors for statics. A `Drop` guard here would be a guard that
/// never fires, which is worse than none: it reads like cleanup.
///
/// # Why `atexit` is not enough on its own
///
/// A process killed hard never reaches its exit handlers, and in this
/// repo that is routine rather than hypothetical: `.config/nextest.toml`
/// sets a per-test `terminate-after`, which exists precisely to kill a
/// hung test process, and CLAUDE.md records it firing twice. So the
/// creation path also sweeps siblings whose owning process is gone (see
/// [`sweep_dead_siblings`]). The two halves cover different failures:
/// `atexit` makes the common case immediate, the sweep makes the
/// population bounded no matter how a process died — and drains whatever
/// backlog a machine already carries.
///
/// `try_lock`, never `lock`: a deadlock during process teardown would
/// hang the test binary, which is a far worse outcome than one directory
/// surviving until the next run sweeps it.
extern "C" fn remove_registered_scratch_dirs() {
    let Some(registry) = SCRATCH_DIRS.get() else {
        return;
    };
    let Ok(dirs) = registry.try_lock() else {
        return;
    };
    for dir in dirs.values() {
        let _ = std::fs::remove_dir_all(dir);
    }
}

/// Does a process with this id exist?
///
/// `kill(pid, 0)` delivers no signal — POSIX defines signal 0 as an
/// existence-and-permission probe. Three answers, and only one of them
/// means "gone":
///
/// * `0` — the process exists and we may signal it.
/// * `EPERM` — the process exists and belongs to another user. Alive, and
///   emphatically not ours to clean up after.
/// * `ESRCH` — no such process.
///
/// Anything unexpected is treated as alive, because the conservative
/// direction here is to keep a directory, never to remove one.
#[cfg(unix)]
fn pid_is_alive(pid: i32) -> bool {
    // SAFETY: `kill` with signal 0 performs no delivery. `pid` is checked
    // `> 0` by the caller, so this can never address a process GROUP
    // (`kill(0, …)`) or every process (`kill(-1, …)`).
    if unsafe { libc::kill(pid, 0) } == 0 {
        return true;
    }
    std::io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH)
}

/// No portable existence probe off unix, so nothing is ever swept there:
/// the `atexit` half still runs, and a leftover is left alone rather than
/// removed on a guess.
#[cfg(not(unix))]
fn pid_is_alive(_pid: i32) -> bool {
    true
}

/// Remove `<prefix>-<pid>` directories in the temp root whose owning
/// process is gone.
///
/// Deliberately narrow, because this deletes things:
///
/// * the name must be EXACTLY `<prefix>-<all ascii digits>`, so
///   `darkmux-flow-test-123` matches while `darkmux-flow-test-123-keep`
///   and `darkmux-flow-testing-1` do not;
/// * the pid must be `> 0` (never a process-group or broadcast id) and
///   must not be our own;
/// * the process must be provably gone, not merely unreachable;
/// * the entry must be a real directory, not a symlink wearing the
///   right name.
///
/// Pid reuse resolves in the safe direction: a recycled pid now belonging
/// to an unrelated live process reads as alive, so the directory is kept
/// rather than removed.
fn sweep_dead_siblings(prefix: &str) {
    let me = std::process::id();
    let Ok(entries) = std::fs::read_dir(std::env::temp_dir()) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        let Some(rest) = name.strip_prefix(prefix).and_then(|r| r.strip_prefix('-')) else {
            continue;
        };
        if rest.is_empty() || !rest.bytes().all(|b| b.is_ascii_digit()) {
            continue;
        }
        let Ok(pid) = rest.parse::<i32>() else { continue };
        if pid <= 0 || pid as u32 == me || pid_is_alive(pid) {
            continue;
        }
        // A REAL directory, never a symlink to one. The name is
        // predictable ahead of creation, so a symlink planted at it would
        // turn this into a recursive delete of whatever it points at —
        // the same pre-planted-name hazard #2158 closed on the dispatch
        // out-dir. `DirEntry::file_type` describes the entry itself, not
        // its target, so a symlink fails this test and is skipped.
        if !entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
            continue;
        }
        let _ = std::fs::remove_dir_all(entry.path());
    }
}

/// A scratch directory under the system temp root, owned by this process
/// and cleaned up after it.
///
/// Returns `$TMPDIR/<prefix>-<pid>` — the same name the five hand-rolled
/// call sites this replaces produced, so nothing downstream had to learn
/// a new path — created if absent, memoized per prefix, and removed
/// again by the two mechanisms described on
/// [`remove_registered_scratch_dirs`].
///
/// # Contract
///
/// * **Test-only.** The module is `#[cfg(any(test, feature =
///   "test-support"))]`, so this cannot be reached from a release build.
///   A production path that wants a temp directory wants a different
///   thing: a dispatch's out-dir holds the run's prompt, trajectory and
///   checkpoint, and is kept ON PURPOSE.
/// * **Best effort on creation**, matching every site it replaces: a temp
///   root that cannot be written is already a broken environment, and a
///   panic on the flow sink's per-record resolve path would be a worse
///   failure than the write error the caller already handles.
/// * **The directory starts empty.** A recycled pid can land on a name a
///   long-dead process left behind — the sweep only removes what it can
///   prove is dead, and a name whose pid is now OURS is never swept — so
///   the existing tree is removed before creation rather than reused.
///
/// # Panics
///
/// On a prefix that is empty or carries anything but ASCII alphanumerics,
/// `-` and `_`. The prefix is a literal at every call site, so this is a
/// compile-time-shaped mistake caught at the first call; a prefix
/// carrying `/` or `..` would make the sweep's name matching mean
/// something other than what its doc says.
pub fn process_scratch_dir(prefix: &str) -> PathBuf {
    assert!(
        !prefix.is_empty()
            && prefix.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_'),
        "a scratch-dir prefix must be a non-empty run of ASCII alphanumerics, `-` and `_`; \
         got {prefix:?}"
    );

    // A poisoned registry is not a reason to stop cleaning up: the map
    // holds paths, and a panic elsewhere cannot have left one half-built.
    let mut registry = scratch_dirs().lock().unwrap_or_else(|e| e.into_inner());
    if let Some(dir) = registry.get(prefix) {
        return dir.clone();
    }

    static ATEXIT: std::sync::Once = std::sync::Once::new();
    ATEXIT.call_once(|| {
        // SAFETY: `atexit` takes an `extern "C" fn()` and this one only
        // removes paths this process itself registered above. Registered
        // AFTER `scratch_dirs()` has initialized the `OnceLock`, so the
        // handler's `SCRATCH_DIRS.get()` can never be `None` at exit.
        unsafe {
            libc::atexit(remove_registered_scratch_dirs);
        }
    });

    sweep_dead_siblings(prefix);

    let dir = std::env::temp_dir().join(format!("{prefix}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let _ = std::fs::create_dir_all(&dir);
    registry.insert(prefix.to_string(), dir.clone());
    dir
}

#[cfg(test)]
mod tests {
    use super::*;

    /// [`InactivityBudget`]'s restore, asserted for all three shapes it
    /// has to handle: displacing a value, displacing nothing, and the
    /// `unset()` constructor.
    ///
    /// Added because the PR's own mutation gate caught this as a MISSED
    /// mutant — `<impl Drop for InactivityBudget>::drop` replaced with
    /// `()` and every test still green. That is precisely the #2698
    /// finding this change exists to fix, reproduced in the new guard: an
    /// unasserted restore is not a restore. A leaked budget pin is not
    /// cosmetic either, since the value is read LIVE per access, so it
    /// would silently become the threshold every later test in the process
    /// measures its staleness fixtures against.
    #[test]
    #[serial_test::serial]
    fn the_budget_guard_restores_what_it_displaced() {
        const VAR: &str = "DARKMUX_INACTIVITY_TIMEOUT_SECONDS";
        let ambient = std::env::var_os(VAR);

        // Shape 1: it displaced a value.
        // SAFETY: #[serial].
        unsafe { std::env::set_var(VAR, "4242") };
        {
            let _b = InactivityBudget::seconds(600);
            assert_eq!(std::env::var(VAR).as_deref(), Ok("600"), "the pin must take effect");
        }
        assert_eq!(
            std::env::var(VAR).as_deref(),
            Ok("4242"),
            "the displaced value must come back"
        );

        // Shape 2: `unset()` over a value.
        {
            let _b = InactivityBudget::unset();
            assert!(
                std::env::var_os(VAR).is_none(),
                "unset() must reach the built-in default tier by REMOVING the override"
            );
        }
        assert_eq!(
            std::env::var(VAR).as_deref(),
            Ok("4242"),
            "unset() must restore the value it removed"
        );

        // Shape 3: it displaced nothing — the variable must be REMOVED
        // again, not left behind pinned at whatever this test chose.
        // SAFETY: #[serial].
        unsafe { std::env::remove_var(VAR) };
        {
            let _b = InactivityBudget::seconds(7);
            assert_eq!(std::env::var(VAR).as_deref(), Ok("7"));
        }
        assert!(
            std::env::var_os(VAR).is_none(),
            "a variable that was UNSET must be unset again — a leaked budget pin becomes the \
             threshold every later test in this process is measured against"
        );

        // SAFETY: #[serial].
        unsafe {
            match ambient {
                Some(v) => std::env::set_var(VAR, v),
                None => std::env::remove_var(VAR),
            }
        }
    }

    /// (#2710) [`neutralize_state_vars`]'s contract, asserted the same way
    /// `tests/cli.rs` asserts its spawn helper: by reading the `Command`
    /// back, which needs no subprocess.
    ///
    /// Every variable either list knows about must come back as an
    /// explicit REMOVAL — `Some(&None)` in `get_envs()` terms — with
    /// nothing left to inherit. `DARKMUX_HOME` is included with no
    /// exception, which is the difference from the private copy this
    /// replaced: a caller that wants it pinned sets it AFTER the call, and
    /// the four targets whose whole point is default resolution get the
    /// removal they were writing by hand.
    #[test]
    fn neutralize_state_vars_removes_every_variable_in_both_lists() {
        use std::ffi::OsStr;

        let mut cmd = std::process::Command::new("/nonexistent-never-spawned");
        // Pre-set one of each kind, so "removed" is a real observation
        // rather than an artifact of the variable never having been named.
        cmd.env("DARKMUX_HOME", "/darkmux-sentinel-home");
        cmd.env("DARKMUX_AUDIT_DIR", "/darkmux-sentinel-audit");
        neutralize_state_vars(&mut cmd);

        let envs: std::collections::BTreeMap<&OsStr, Option<&OsStr>> = cmd.get_envs().collect();
        for (var, _) in PINNED_STATE_VARS {
            assert_eq!(
                envs.get(OsStr::new(*var)),
                Some(&None),
                "{var} is a darkmux write destination; a spawn that leaves it to be inherited \
                 points the child at whatever the ambient shell names — the operator's own \
                 tree in an ordinary terminal"
            );
        }
        for var in CLEARED_STATE_VARS {
            assert_eq!(
                envs.get(OsStr::new(*var)),
                Some(&None),
                "{var}'s PRESENCE changes behavior, so an inherited value is not merely a \
                 misplaced write. DARKMUX_AUDIT_DIR is the sharp one: it turns the \
                 hash-chained audit sink on, and chained records cannot be removed without \
                 breaking the chain."
            );
        }
    }

    /// The ordering rule the doc states, asserted rather than trusted: a
    /// `.env` AFTER the call must SURVIVE it.
    ///
    /// This is the half a caller gets wrong silently. `tests/cli.rs`
    /// pins `DARKMUX_HOME` and the e2e harness pins six more; if
    /// `neutralize_state_vars` were ever called after those pins instead of
    /// before, every one would be erased and the child would fall back to
    /// defaults under a `HOME` that may not be pinned either — with no
    /// compile error and, for a passing test, no symptom.
    #[test]
    fn a_pin_applied_after_the_call_survives_it() {
        use std::ffi::OsStr;

        let mut cmd = std::process::Command::new("/nonexistent-never-spawned");
        neutralize_state_vars(&mut cmd);
        cmd.env("DARKMUX_HOME", "/pinned-after");

        let envs: std::collections::BTreeMap<&OsStr, Option<&OsStr>> = cmd.get_envs().collect();
        assert_eq!(
            envs.get(OsStr::new("DARKMUX_HOME")),
            Some(&Some(OsStr::new("/pinned-after"))),
            "a pin applied after neutralize_state_vars must win — the documented idiom is \
             `neutralize, then pin`, and it only works if Command applies calls in order"
        );
    }

    /// The two lists must stay disjoint — a variable that is both pinned
    /// and cleared would have its restore entry pushed twice and its
    /// final value decided by list order, which is exactly the kind of
    /// silent ordering dependence this guard exists to remove.
    #[test]
    fn the_pinned_and_cleared_sets_do_not_overlap() {
        for (var, _) in PINNED_STATE_VARS {
            assert!(
                !CLEARED_STATE_VARS.contains(var),
                "{var} is both pinned and cleared; pick one"
            );
        }
    }

    /// [`PINNED_STATE_VARS`] is the SPECIFICATION of what isolation means
    /// here, so it gets an assertion rather than only a comment: while the
    /// guard lives, every entry must resolve under the guard's root, at
    /// its documented subpath, and never into the operator's real tree.
    ///
    /// This is what makes each entry load-bearing. Measured during the
    /// fix: deleting the `DARKMUX_FLOWS_DIR` entry alone left BOTH the
    /// keystone test and the real leak probe green, because `DARKMUX_HOME`
    /// is pinned too and `flows_dir_default()` re-roots off it — so every
    /// individual entry looks removable when judged only by "does the
    /// suite still pass". That reasoning is how a list like this erodes
    /// one entry at a time until a build WITHOUT
    /// `darkmux-types/test-support` — where `config()` reads the
    /// operator's real `config.json` and `config.dirs.<x>` outranks the
    /// root-derived default — leaks again. The list is the contract; this
    /// test holds the contract.
    #[test]
    #[serial_test::serial]
    fn every_pinned_variable_points_under_the_isolated_root() {
        let real = dirs::home_dir().map(|h| h.join(".darkmux"));
        let state = IsolatedState::new();
        for (var, subpath) in PINNED_STATE_VARS {
            let value = std::env::var_os(var)
                .unwrap_or_else(|| panic!("{var} is listed in PINNED_STATE_VARS but was not set"));
            let path = PathBuf::from(value);
            assert!(
                path.starts_with(state.path()),
                "{var} must resolve under the isolated root, got {}",
                path.display()
            );
            let expected =
                if subpath.is_empty() { state.path().to_path_buf() } else { state.join(subpath) };
            assert_eq!(path, expected, "{var} must pin to its documented subpath");
            if let Some(real) = real.as_ref() {
                assert!(
                    !path.starts_with(real),
                    "{var} pinned into the operator's real tree: {}",
                    path.display()
                );
            }
        }
    }

    /// The other list's contract: a cleared variable must be ABSENT while
    /// the guard lives. `DARKMUX_AUDIT_DIR` is the one that matters most —
    /// its mere presence is what turns the hash-chained audit sink on — so
    /// "cleared" has to mean removed, never set-to-empty.
    #[test]
    #[serial_test::serial]
    fn every_cleared_variable_is_absent_while_the_guard_lives() {
        let saved: Vec<(&str, Option<OsString>)> =
            CLEARED_STATE_VARS.iter().map(|v| (*v, std::env::var_os(v))).collect();
        // SAFETY: #[serial]. Give each one a value first, so "absent" is a
        // real observation rather than a coincidence of the ambient shell.
        unsafe {
            for var in CLEARED_STATE_VARS {
                std::env::set_var(var, "/darkmux-sentinel-cleared");
            }
        }
        {
            let _state = IsolatedState::new();
            for var in CLEARED_STATE_VARS {
                assert!(
                    std::env::var_os(var).is_none(),
                    "{var} is listed in CLEARED_STATE_VARS but survived the guard"
                );
            }
        }
        // SAFETY: #[serial].
        unsafe {
            for (var, prev) in saved {
                match prev {
                    Some(v) => std::env::set_var(var, v),
                    None => std::env::remove_var(var),
                }
            }
        }
    }

    /// (#2736 item 2) The test above iterates `CLEARED_STATE_VARS` itself,
    /// so it is tautological in exactly the dimension that matters: a
    /// destination variable missing from the list is invisible to it, by
    /// construction, forever. This test names the two real destinations
    /// #2736 measured as `still set` — `DARKMUX_REDIS_URL` (a live
    /// network endpoint) and `DARKMUX_FLEET_SNAPSHOT_FILE` (a read-side
    /// handoff override) — as LITERAL strings, never through the const, so
    /// it stays red-provable: deleting either from `CLEARED_STATE_VARS`
    /// turns THIS test red without needing to also remember to update it.
    #[test]
    #[serial_test::serial]
    fn the_isolation_guard_clears_redis_url_and_fleet_snapshot_file() {
        let saved_redis = std::env::var_os("DARKMUX_REDIS_URL");
        let saved_snapshot = std::env::var_os("DARKMUX_FLEET_SNAPSHOT_FILE");
        // SAFETY: #[serial].
        unsafe {
            std::env::set_var("DARKMUX_REDIS_URL", "redis://127.0.0.1:6379/0");
            std::env::set_var("DARKMUX_FLEET_SNAPSHOT_FILE", "/tmp/darkmux-sentinel-snapshot.json");
        }
        {
            let _state = IsolatedState::new();
            assert!(
                std::env::var_os("DARKMUX_REDIS_URL").is_none(),
                "DARKMUX_REDIS_URL survived the guard — a guarded test/process can still \
                 address a real, live Redis instance (#2736 item 2)"
            );
            assert!(
                std::env::var_os("DARKMUX_FLEET_SNAPSHOT_FILE").is_none(),
                "DARKMUX_FLEET_SNAPSHOT_FILE survived the guard — a guarded process can still \
                 read a stale or foreign fleet-snapshot handoff instead of the live path it \
                 thinks it is exercising (#2736 item 2)"
            );
        }
        // SAFETY: #[serial].
        unsafe {
            match saved_redis {
                Some(v) => std::env::set_var("DARKMUX_REDIS_URL", v),
                None => std::env::remove_var("DARKMUX_REDIS_URL"),
            }
            match saved_snapshot {
                Some(v) => std::env::set_var("DARKMUX_FLEET_SNAPSHOT_FILE", v),
                None => std::env::remove_var("DARKMUX_FLEET_SNAPSHOT_FILE"),
            }
        }
    }

    /// Restoration is the half a guard can silently lose (#2698 finding
    /// 1: a restore made a no-op left the suite fully green, 100 passed).
    /// So it is asserted per variable, for BOTH shapes — a variable that
    /// had a value must get that exact value back, and one that was unset
    /// must be UNSET again, not left as an empty string.
    #[test]
    #[serial_test::serial]
    fn every_touched_variable_is_restored_exactly() {
        // One of each shape, chosen from the two lists so the test covers
        // the pinned path and the cleared path.
        const HAD_VALUE: &str = "DARKMUX_FLOWS_DIR";
        const WAS_UNSET: &str = "DARKMUX_MODS_DIR";
        const CLEARED_WITH_VALUE: &str = "DARKMUX_AUDIT_DIR";

        let saved: Vec<(&str, Option<OsString>)> = [HAD_VALUE, WAS_UNSET, CLEARED_WITH_VALUE]
            .iter()
            .map(|v| (*v, std::env::var_os(v)))
            .collect();

        // SAFETY: #[serial].
        unsafe {
            std::env::set_var(HAD_VALUE, "/darkmux-sentinel-flows");
            std::env::remove_var(WAS_UNSET);
            std::env::set_var(CLEARED_WITH_VALUE, "/darkmux-sentinel-audit");
        }

        {
            let state = IsolatedState::new();
            assert!(
                PathBuf::from(std::env::var(HAD_VALUE).unwrap()).starts_with(state.path()),
                "a pinned variable must resolve under the isolated root while the guard lives"
            );
            assert!(
                PathBuf::from(std::env::var(WAS_UNSET).unwrap()).starts_with(state.path()),
                "a pinned variable is pinned whether or not it had a previous value"
            );
            assert!(
                std::env::var_os(CLEARED_WITH_VALUE).is_none(),
                "a cleared variable must be REMOVED while the guard lives — \
                 DARKMUX_AUDIT_DIR's presence is what enables the audit sink"
            );
        }

        assert_eq!(
            std::env::var_os(HAD_VALUE).as_deref(),
            Some(std::ffi::OsStr::new("/darkmux-sentinel-flows")),
            "the displaced value must come back byte-for-byte"
        );
        assert!(
            std::env::var_os(WAS_UNSET).is_none(),
            "a variable that was UNSET must be removed again, not left as an empty string"
        );
        assert_eq!(
            std::env::var_os(CLEARED_WITH_VALUE).as_deref(),
            Some(std::ffi::OsStr::new("/darkmux-sentinel-audit")),
            "a cleared variable's displaced value must come back too"
        );

        // SAFETY: #[serial]. Put the ambient environment back.
        unsafe {
            for (var, prev) in saved {
                match prev {
                    Some(v) => std::env::set_var(var, v),
                    None => std::env::remove_var(var),
                }
            }
        }
    }

    /// (#2717) The claim every `under_home` count rests on: while the
    /// sentinel's `HOME` is applied, `dirs::home_dir()` returns it.
    ///
    /// Asserted rather than assumed because the whole execution-side check
    /// is built on it — if `dirs::home_dir()` consulted something else
    /// (`getpwuid`, a platform API), every default-tier write would land
    /// in the OPERATOR's tree during a leak check and be counted as zero.
    /// A measurement that reports clean because it is looking at the wrong
    /// directory is worse than no measurement.
    #[test]
    #[serial_test::serial]
    fn the_sentinel_home_is_what_dirs_home_dir_resolves_to() {
        let sentinel = StateLeakSentinel::new();
        let prev = std::env::var_os("HOME");
        // SAFETY: #[serial].
        unsafe { std::env::set_var("HOME", sentinel.home()) };
        let observed = dirs::home_dir();
        // SAFETY: #[serial]. Restore before asserting, so a failure does
        // not leave every later test in this process under the sentinel.
        unsafe {
            match prev {
                Some(v) => std::env::set_var("HOME", v),
                None => std::env::remove_var("HOME"),
            }
        }
        assert_eq!(
            observed.as_deref(),
            Some(sentinel.home().as_path()),
            "dirs::home_dir() must follow $HOME; the execution-side leak check counts \
             <home>/.darkmux, and if home_dir() resolved elsewhere that count would be \
             structurally zero while the operator's real tree took the writes"
        );
    }

    /// Anti-vacuity for the assertion the whole check makes: a sentinel
    /// nothing has run against censuses as EMPTY. Without this, a census
    /// that always returned zero — a wrong base path, an unreadable
    /// directory — would read as "every target is clean".
    #[test]
    fn an_untouched_sentinel_censuses_as_empty() {
        let sentinel = StateLeakSentinel::new();
        let census = sentinel.census();
        assert_eq!(census.leaked_files(), 0, "a fresh sentinel must start empty");
        assert!(census.audit_records.is_empty());
    }

    /// The census must see BOTH destinations and must size the audit chain
    /// by RECORD, not by file. A write that honors the pinned variables
    /// lands under the root; one that re-derives from `$HOME` — the shape
    /// an ordinary developer machine produces, since almost nobody exports
    /// these — lands under `<home>/.darkmux`. Counting only the convenient
    /// one is the measured failure mode this type's doc names.
    #[test]
    fn the_census_counts_both_destinations_and_sizes_the_audit_chain() {
        let sentinel = StateLeakSentinel::new();
        let root = sentinel.root();
        std::fs::create_dir_all(root.join("flows")).unwrap();
        std::fs::write(root.join("flows/2026-01-01.jsonl"), "a\nb\n").unwrap();
        std::fs::create_dir_all(root.join("findings/sess/1")).unwrap();
        std::fs::write(root.join("findings/sess/1/finding.json"), "{}").unwrap();
        std::fs::create_dir_all(root.join("audit")).unwrap();
        std::fs::write(root.join("audit/2026-01-01.jsonl"), "r1\nr2\nr3\n").unwrap();
        let home_state = sentinel.home().join(".darkmux");
        std::fs::create_dir_all(&home_state).unwrap();
        std::fs::write(home_state.join("fleet.json"), "{}").unwrap();

        let census = sentinel.census();
        assert_eq!(
            census.under_root.len(),
            3,
            "the root half of the census missed a file: {:?}",
            census.under_root
        );
        assert_eq!(
            census.under_home,
            vec![PathBuf::from("fleet.json")],
            "the $HOME half of the census is the one that catches a write which ignores \
             the pinned variables entirely"
        );
        assert_eq!(census.leaked_files(), 4);
        assert_eq!(
            census.audit_records.iter().map(|(_, n)| *n).sum::<usize>(),
            3,
            "the audit chain is reported by RECORD count — it is append-only and chained, \
             so a fabricated record cannot be removed without breaking the chain"
        );
        assert!(census.report(10).contains("fleet.json"));
    }

    /// `apply` pins exactly ONE variable — the root — and REMOVES every
    /// other darkmux state variable, including `DARKMUX_AUDIT_DIR`.
    ///
    /// This is the assertion that keeps the harness from manufacturing its
    /// own failures. A variable that outranks the root (`DARKMUX_CREW_DIR`
    /// in `user_state_root()`, `DARKMUX_LAB_DIR`/`DARKMUX_FINDINGS_DIR`/
    /// `DARKMUX_MODS_DIR` in their accessors) pinned to one scratch path
    /// overrides every per-test guard in the target and forces them onto a
    /// shared directory: measured on `darkmux-lab`, 31 crawl tests fail
    /// that way and 2,416 of 2,416 pass with the pin removed.
    ///
    /// The `CARGO_HOME` half is not hygiene either: moving `$HOME` without
    /// it sends cargo's registry and target cache into the sentinel, and
    /// the census then reports thousands of files for every target, which
    /// makes the real signal unreadable.
    #[test]
    fn apply_pins_only_the_root_and_clears_every_variable_that_outranks_it() {
        use std::ffi::OsStr;

        let sentinel = StateLeakSentinel::new();
        let mut cmd = std::process::Command::new("/nonexistent-never-spawned");
        // Pre-set the sharp ones, so "removed" is a real observation and
        // not an artifact of the variable never having been named.
        cmd.env("DARKMUX_CREW_DIR", "/darkmux-sentinel-crew");
        cmd.env("DARKMUX_AUDIT_DIR", "/darkmux-sentinel-audit");
        sentinel.apply(&mut cmd);
        let envs: std::collections::BTreeMap<&OsStr, Option<&OsStr>> = cmd.get_envs().collect();

        assert_eq!(
            envs.get(OsStr::new("HOME")).copied().flatten(),
            Some(sentinel.home().as_os_str()),
            "the census counts <home>/.darkmux, so $HOME has to move"
        );
        assert_eq!(
            envs.get(OsStr::new("DARKMUX_HOME")).copied().flatten(),
            Some(sentinel.root().as_os_str()),
            "the root is the ONE pin — every default derives from it, and a test's own \
             pin still overrides it"
        );
        for (var, _) in PINNED_STATE_VARS.iter().filter(|(v, _)| *v != "DARKMUX_HOME") {
            assert_eq!(
                envs.get(OsStr::new(*var)),
                Some(&None),
                "{var} outranks the root or derives from it; pinning it would override \
                 every per-test guard in the target with one shared value and make \
                 correctly-isolated tests collide"
            );
        }
        for var in CLEARED_STATE_VARS {
            assert_eq!(
                envs.get(OsStr::new(*var)),
                Some(&None),
                "{var} must be removed. DARKMUX_AUDIT_DIR is the one worth stating: its \
                 presence enables the hash-chained sink, and pinning it would bind the \
                 whole binary's memoized default sink before any test runs (#2730)"
            );
        }
        assert!(
            envs.get(OsStr::new("CARGO_HOME")).copied().flatten().is_some(),
            "CARGO_HOME must be re-pinned outside the sentinel, or cargo's own registry \
             lands in the census and buries the signal"
        );
    }

    // ─── (#2707) process_scratch_dir ──────────────────────────────────

    /// Remove a set of temp-root fixtures however the test ends.
    ///
    /// The sweep tests have to plant their fixtures as SIBLINGS in the
    /// real temp root — that is the only place the sweep looks — and the
    /// ones they plant to prove a name is SPARED are, by construction,
    /// names nothing will ever collect. A plain removal on the last line
    /// is the exact shape #2707 is about: it runs only when the test
    /// passed. Measured while building this module: a red run left
    /// `<prefix>-+<pid>` and `<prefix>-victim-<pid>` behind.
    struct TempFixtures(Vec<PathBuf>);

    impl TempFixtures {
        fn dir(&mut self, path: PathBuf) -> PathBuf {
            std::fs::create_dir_all(&path).unwrap();
            self.0.push(path.clone());
            path
        }

        fn track(&mut self, path: PathBuf) -> PathBuf {
            self.0.push(path.clone());
            path
        }
    }

    impl Drop for TempFixtures {
        fn drop(&mut self) {
            for path in &self.0 {
                // `remove_dir_all` does not follow a symlink, and a
                // planted one has to go too.
                if std::fs::symlink_metadata(path).map(|m| m.file_type().is_symlink()).unwrap_or(false)
                {
                    let _ = std::fs::remove_file(path);
                } else {
                    let _ = std::fs::remove_dir_all(path);
                }
            }
        }
    }

    /// A prefix no other test in this binary uses, so each case gets its
    /// own memo slot and its own directory.
    fn unique_prefix(label: &str) -> String {
        use std::sync::atomic::{AtomicU32, Ordering};
        static N: AtomicU32 = AtomicU32::new(0);
        format!("darkmux-scratch-selftest-{label}-{}", N.fetch_add(1, Ordering::Relaxed))
    }

    /// The point of the fallback, asserted: a process that pinned nothing
    /// still has somewhere real to write.
    ///
    /// This is the half an over-eager cleanup breaks. A fix that removed
    /// the directory too early — or never created it — would leave the
    /// flow sink writing into a path that does not exist, and a sink
    /// whose writes all fail looks exactly like a test with nothing to
    /// say. So existence, writability and readback are all asserted, not
    /// just the returned path.
    #[test]
    #[serial_test::serial]
    fn the_scratch_dir_exists_and_is_actually_writable() {
        let prefix = unique_prefix("writable");
        let dir = process_scratch_dir(&prefix);

        assert!(
            dir.starts_with(std::env::temp_dir()),
            "the scratch dir must live under the temp root, got {}",
            dir.display()
        );
        assert_eq!(
            dir.file_name().and_then(|n| n.to_str()),
            Some(format!("{prefix}-{}", std::process::id()).as_str()),
            "the name is `<prefix>-<pid>` — the five call sites this replaces all built that \
             name by hand, and downstream fixtures were written against it"
        );
        assert!(dir.is_dir(), "the scratch dir must exist on disk, not merely be named");

        let probe = dir.join("probe.jsonl");
        std::fs::write(&probe, b"{}\n").expect("the scratch dir must accept a write");
        assert_eq!(std::fs::read(&probe).unwrap(), b"{}\n");
    }

    /// One directory per prefix per process — the memo — and different
    /// prefixes are genuinely different directories.
    #[test]
    #[serial_test::serial]
    fn the_scratch_dir_is_memoized_per_prefix() {
        let a = unique_prefix("memo");
        let b = unique_prefix("memo");

        assert_eq!(
            process_scratch_dir(&a),
            process_scratch_dir(&a),
            "repeated calls with one prefix must return the same directory — the flow sink \
             resolves this per record write"
        );
        assert_ne!(
            process_scratch_dir(&a),
            process_scratch_dir(&b),
            "two prefixes must not share a directory"
        );
    }

    /// The recycled-pid clause: a leftover tree at this process's own name
    /// is removed before the directory is handed out, never reused.
    #[test]
    #[serial_test::serial]
    fn a_recycled_pids_leftovers_are_cleared_rather_than_reused() {
        let prefix = unique_prefix("recycled");
        let planted =
            std::env::temp_dir().join(format!("{prefix}-{}", std::process::id())).join("stale");
        std::fs::create_dir_all(&planted).unwrap();
        // The parent here IS `<prefix>-<pid>`, which the helper registers
        // and removes at exit, so no extra tracking is needed.
        std::fs::write(planted.join("old.jsonl"), b"records from a dead process\n").unwrap();

        let dir = process_scratch_dir(&prefix);

        assert!(dir.is_dir());
        assert_eq!(
            std::fs::read_dir(&dir).unwrap().count(),
            0,
            "a directory handed out with a previous process's records still in it would make \
             a test read another run's data as its own"
        );
    }

    /// The sweep, both directions at once: a directory whose owning
    /// process is gone is removed, and one whose process is alive is not.
    ///
    /// `pid 1` is the live case deliberately — it is always running and
    /// belongs to root, so `kill(1, 0)` answers `EPERM` rather than `0`.
    /// That is the branch a naive `== 0` existence check gets wrong, and
    /// getting it wrong means deleting a live process's directory.
    #[test]
    #[serial_test::serial]
    #[cfg(unix)]
    fn the_sweep_removes_a_dead_pids_dir_and_spares_a_live_ones() {
        let prefix = unique_prefix("sweep");
        let tmp = std::env::temp_dir();

        // Two pids that are definitively gone: spawn, wait, reap.
        let reaped = || {
            let mut child = std::process::Command::new("/bin/sh")
                .args(["-c", "exit 0"])
                .spawn()
                .expect("spawning a throwaway child");
            let pid = child.id();
            child.wait().expect("reaping the throwaway child");
            pid
        };
        let dead_pid = reaped();
        let planted_pid = reaped();

        let mut fixtures = TempFixtures(Vec::new());
        let dead_dir = fixtures.dir(tmp.join(format!("{prefix}-{dead_pid}")));
        let live_dir = fixtures.dir(tmp.join(format!("{prefix}-1")));
        // `<prefix>-+<dead pid>`: the case the explicit all-digits name
        // check is FOR. `"+123".parse::<i32>()` succeeds and yields 123,
        // so a parse-only rule would read this as the dead pid and delete
        // a directory whose name this helper never produces and does not
        // own. Measured: with the all-digits check deleted, this is the
        // only assertion in the module that goes red.
        let signed_dead_dir = fixtures.dir(tmp.join(format!("{prefix}-+{dead_pid}")));

        // A SYMLINK wearing a dead pid's name. The name is predictable
        // ahead of creation, so this is the pre-planted-name hazard
        // #2158 closed on the dispatch out-dir, arriving at the sweep:
        // following it would recursively delete whatever it points at.
        let victim = fixtures.dir(tmp.join(format!("{prefix}-victim-{planted_pid}")));
        std::fs::write(victim.join("precious.txt"), b"do not delete\n").unwrap();
        let planted = fixtures.track(tmp.join(format!("{prefix}-{planted_pid}")));
        std::os::unix::fs::symlink(&victim, &planted).unwrap();

        assert!(!pid_is_alive(dead_pid as i32), "sanity: the reaped child must read as gone");
        assert!(
            pid_is_alive(1),
            "sanity: pid 1 must read as alive — it answers EPERM, not 0, and treating EPERM \
             as 'gone' would delete a live process's scratch dir"
        );

        process_scratch_dir(&prefix);

        assert!(
            !dead_dir.exists(),
            "the dead process's directory must be swept: {}",
            dead_dir.display()
        );
        assert!(
            live_dir.is_dir(),
            "a live process's directory must be left alone: {}",
            live_dir.display()
        );
        assert!(
            signed_dead_dir.is_dir(),
            "the name must be EXACTLY `<prefix>-<digits>`; a `+`-signed tail parses as a pid \
             but is not a name this helper writes: {}",
            signed_dead_dir.display()
        );

        assert!(
            std::fs::symlink_metadata(&planted).unwrap().file_type().is_symlink(),
            "a symlink at a dead pid's name must be left exactly as it was, never followed"
        );
        assert!(
            victim.join("precious.txt").exists(),
            "the symlink's target must be untouched — nothing was ever deleted through it"
        );
    }

    /// The sweep deletes things, so its name matching is asserted to be
    /// exact rather than prefix-ish. Both shapes here would be destroyed
    /// by a `starts_with`-only match, and neither belongs to this helper.
    #[test]
    #[serial_test::serial]
    fn the_sweep_spares_names_that_merely_begin_with_the_prefix() {
        let prefix = unique_prefix("exactness");
        let tmp = std::env::temp_dir();
        let mut fixtures = TempFixtures(Vec::new());

        // `<prefix>-<digits>-<something>`: a dead pid in the name, but a
        // tail that says this is not one of ours.
        let suffixed = fixtures.dir(tmp.join(format!("{prefix}-1-keepme")));
        // `<prefix><more>-<digits>`: a different prefix that happens to
        // start with the same bytes.
        let extended = fixtures.dir(tmp.join(format!("{prefix}extra-1")));
        // A digit-free tail.
        let worded = fixtures.dir(tmp.join(format!("{prefix}-notapid")));

        process_scratch_dir(&prefix);

        for d in [&suffixed, &extended, &worded] {
            assert!(d.is_dir(), "the sweep must not touch {}", d.display());
        }
    }

    /// The exit handler's body, exercised directly.
    ///
    /// `atexit` firing is not observable from inside the process that
    /// registered it, so this asserts the half that is: given a
    /// registered directory, the handler removes it. That the handler is
    /// REGISTERED is held by `process_scratch_dir`'s `Once` above; that
    /// it actually fires was measured by counting directories in the temp
    /// root before and after a real test run (see the PR).
    #[test]
    #[serial_test::serial]
    fn the_exit_handler_removes_every_registered_directory() {
        let prefix = unique_prefix("atexit");
        let dir = process_scratch_dir(&prefix);
        std::fs::write(dir.join("some.jsonl"), b"{}\n").unwrap();
        assert!(dir.is_dir(), "sanity: the directory exists before the handler runs");

        remove_registered_scratch_dirs();

        assert!(
            !dir.exists(),
            "the exit handler must remove a registered directory and everything under it: {}",
            dir.display()
        );
    }

    /// A prefix that could make the sweep's name matching mean something
    /// other than what its doc says is refused at the first call, not
    /// quietly accepted.
    #[test]
    fn a_path_bearing_prefix_is_refused() {
        for bad in ["", "../escape", "with/slash", "with space"] {
            let err = std::panic::catch_unwind(|| process_scratch_dir(bad)).unwrap_err();
            let msg = err
                .downcast_ref::<String>()
                .cloned()
                .or_else(|| err.downcast_ref::<&str>().map(|s| s.to_string()))
                .unwrap_or_default();
            assert!(
                msg.contains("scratch-dir prefix"),
                "a refused prefix must say why; got {msg:?} for {bad:?}"
            );
        }
    }
}
