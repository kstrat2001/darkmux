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
pub const CLEARED_STATE_VARS: &[&str] = &[
    "DARKMUX_AUDIT_DIR",
    "DARKMUX_PROFILES",
    "DARKMUX_TEMPLATES_DIR",
    "DARKMUX_SKILLS_DIR",
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

}
