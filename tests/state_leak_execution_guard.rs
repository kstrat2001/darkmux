//! (#2717) The execution-side half of the isolation guard: run a test
//! target under a sentinel state tree and count what it wrote.
//!
//! # Why this exists next to the text scan
//!
//! `tests/cli.rs`'s
//! `every_darkmux_spawn_in_the_tests_dir_goes_through_an_isolating_helper`
//! reads SOURCE, looking for four recognized spellings inside a marker
//! fence. It is a good fast pre-check and it is kept — a source scan names
//! the offending LINE, which a file census cannot — but four escapes were
//! demonstrated against it, each leaving it at EXIT=0 with files leaked:
//!
//! 1. the token list is an allowlist of SPELLINGS, so a resolver under a
//!    different name, or a `PATH`-resolved `Command::new("darkmux")`, is
//!    invisible;
//! 2. the `>= 2` anti-vacuity floor tolerates single-token drift, so
//!    deleting the one token that caught four of the five original
//!    offenders leaves it green with a raw unfenced spawn present;
//! 3. a fenced block that spawns darkmux raw while calling the neutralizer
//!    on a DECOY `Command` passes both assertions;
//! 4. it checks that the call APPEARS on a non-`//` line, never that it
//!    RUNS — a `/* … */` block comment (which `is_code` does not exclude,
//!    and which is what an editor produces when a selection is commented
//!    out), a string literal, or a `#[cfg(any())]` item all satisfy it.
//!
//! Escape 4 is the one reachable by accident, and it re-enters this
//! issue's own harm through the mechanism built to prevent it.
//!
//! None of the four is closed by more tokens or a wider walk. This target
//! observes the EFFECT instead, so all four collapse into one check — and
//! it reaches shapes the scan cannot see at all: unknown spellings,
//! decoys, grandchildren spawned through a shell, in-process writes that
//! never spawn anything at all, and destinations nobody thought to walk.
//!
//! It is NOT a superset of the text scan, and an earlier draft of this
//! paragraph said it was. `enumerate_units` walks the ROOT `tests/*.rs`
//! plus `-p <member> --lib`, and `--lib` neither builds nor runs a
//! package's own integration targets — so the eleven under
//! `crates/darkmux-{crew,fleet,gestalt,lab,profiles,types}/tests/` are
//! outside this check, the same boundary `crates/*/tests/` already sits
//! outside for the text scan. Proven with a decoy at
//! `crates/darkmux-crew/tests/rev2733_decoy_leak.rs` writing into both
//! the real home and the state root: text scan EXIT=0, enumeration
//! EXIT=0, gated check EXIT=0 reporting `clean`, files present. All
//! eleven real targets census clean today, so this is a coverage hole and
//! not a live defect — widening the enumeration is a follow-up, because
//! eleven more units is a change to the CI budget (210s of margin) and
//! needs its own measurement. The boundary is stated here for the reason
//! `tests/cli.rs` states its own: a guard that overclaims is how the next
//! reader skips the check.
//!
//! # The two assertions, and why one of them is free
//!
//! `every_test_unit_the_leak_check_would_run_is_enumerated_from_disk`
//! runs on every `cargo test` and costs milliseconds. It exists because
//! the previous guard's fatal property was a HARDCODED path: it scanned
//! one file and was blind to the other ten targets by construction. So
//! there is no list here to drift — the units are read off disk and out of
//! the workspace manifest every run, and the test asserts the enumeration
//! is non-empty and still finds the files it was written to cover.
//!
//! `no_test_unit_writes_into_a_sentinel_state_tree` is the measurement. It
//! runs each unit as a child `cargo test` under a
//! [`StateLeakSentinel`](darkmux_types::test_isolation::StateLeakSentinel)
//! and asserts the census is EMPTY. It is gated behind
//! `DARKMUX_LEAK_CHECK=1` because it re-runs the suite — CI runs it as its
//! own job; locally, set the variable (optionally with
//! `DARKMUX_LEAK_CHECK_ONLY=<substring>` to narrow to one unit).
//!
//! # Two design rules, both learned from a measurement that got it wrong
//!
//! **The verdict is the file count, never the exit code.** In #2710's own
//! measurements three of four targets went red under leak conditions but
//! ALL FOUR leaked, and one stayed GREEN while leaking. A child's status
//! is reported here for context and is never asserted on.
//!
//! **Every destination is counted, not the convenient one.** The census
//! covers writes that honor `DARKMUX_HOME` AND writes that ignore it and
//! re-derive from `$HOME`, and it sizes the audit chain by record —
//! append-only and chained, so a fabricated record cannot be removed
//! without breaking it. A previous sweep counted mission directories and
//! missed flow records entirely; another counted two directories and
//! missed 8,263 temp directories (#2707).
//!
//! # One pin, and everything else cleared
//!
//! The sentinel pins exactly one variable — `DARKMUX_HOME` — and REMOVES
//! the rest. That asymmetry is load-bearing and was got wrong first: a
//! variable that outranks the root is the right thing to CLEAR and the
//! wrong thing to PIN, because pinning it overrides every per-test guard
//! inside the target with one shared value. With `DARKMUX_CREW_DIR` pinned
//! this way, 31 `darkmux-lab` crawl tests fail on the shared directory;
//! with the pin removed, 2,416 of 2,416 pass. Those failures were the
//! harness, not the code — and a harness that manufactures findings is
//! worse than no harness. `StateLeakSentinel`'s own doc carries the rule
//! and its unit test holds it.
//!
//! One consequence, stated so the check is not over-trusted: with
//! `DARKMUX_AUDIT_DIR` cleared and `config()` empty in test builds, the
//! hash-chained sink is OFF, so this check counts the audit directory but
//! cannot cause a write to it. That is what an ordinary developer machine
//! looks like. The operator who has enabled the sink is a separate
//! exposure, measured and filed as #2730.

use darkmux_types::test_isolation::StateLeakSentinel;

/// One thing the check can run: an integration target in this crate's
/// `tests/`, a workspace package's `--lib` unit tests, or — for the root
/// package, which has no library target — its `--bins`.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
enum Unit {
    IntegrationTarget(String),
    PackageLib(String),
    /// The root package is bin-only. `cargo test -p darkmux --lib` exits
    /// 101 with "no library targets found in package `darkmux`" and runs
    /// NOTHING — and because the verdict here is a file count, a unit that
    /// ran nothing censuses as zero and reports CLEAN. That is this
    /// check's own vacuity hole, found by reading its output rather than
    /// trusting it, and `--bins` is what actually runs `src/`'s
    /// `#[cfg(test)]` modules. The `ran_tests` floor below is the general
    /// fix; this variant is the specific one.
    PackageBins(String),
}

impl Unit {
    fn label(&self) -> String {
        match self {
            Unit::IntegrationTarget(t) => format!("test --test {t}"),
            Unit::PackageLib(p) => format!("test -p {p} --lib"),
            Unit::PackageBins(p) => format!("test -p {p} --bins"),
        }
    }

    fn cargo_args(&self) -> Vec<String> {
        match self {
            Unit::IntegrationTarget(t) => {
                vec!["test".into(), "--test".into(), t.clone(), "--".into(), "--test-threads=4".into()]
            }
            Unit::PackageLib(p) => vec![
                "test".into(),
                "-p".into(),
                p.clone(),
                "--lib".into(),
                "--".into(),
                "--test-threads=4".into(),
            ],
            Unit::PackageBins(p) => vec![
                "test".into(),
                "-p".into(),
                p.clone(),
                "--bins".into(),
                "--".into(),
                "--test-threads=4".into(),
            ],
        }
    }
}

/// Read the units off disk and out of the workspace manifest, every run.
///
/// Deliberately NOT a hardcoded list. The guard this one supplements was
/// blind to ten of eleven integration targets because it named one path in
/// source, and that blindness is the whole reason it needed replacing — a
/// list of targets rots exactly the same way, and rots silently.
fn enumerate_units() -> Vec<Unit> {
    let manifest_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut units = Vec::new();

    // Integration targets: every top-level `tests/*.rs`. Subdirectories
    // (`tests/e2e/`, `tests/parity/`) are modules of those targets, not
    // targets themselves, so they are not separate units — they are still
    // COVERED, because running the target that includes them runs them.
    let entries = std::fs::read_dir(manifest_dir.join("tests"))
        .unwrap_or_else(|e| panic!("reading tests/: {e}"));
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.extension().is_some_and(|e| e == "rs") {
            continue;
        }
        if let Some(stem) = path.file_stem().and_then(|s| s.to_str()) {
            units.push(Unit::IntegrationTarget(stem.to_string()));
        }
    }

    // Workspace packages, read out of the root manifest's `members` list.
    // An in-process write into the operator's tree needs no spawn at all
    // (#2718: a `-p darkmux-crew --lib` run wrote a findings file, 29 flow
    // records and a liveness log with every state variable exported), so
    // unit-test targets are units here too — the class a spawn-shaped
    // guard cannot see in principle.
    let root_manifest = std::fs::read_to_string(manifest_dir.join("Cargo.toml"))
        .unwrap_or_else(|e| panic!("reading Cargo.toml: {e}"));
    // `members` is a single-line array today; the scrape handles both
    // shapes by slicing between the brackets rather than assuming one
    // entry per line.
    let members = root_manifest
        .split_once("members")
        .and_then(|(_, rest)| rest.split_once('['))
        .and_then(|(_, rest)| rest.split_once(']'))
        .map(|(inside, _)| inside.to_string())
        .unwrap_or_default();
    for raw in members.split(',') {
        let path = raw.trim().trim_matches('"').trim();
        if path.is_empty() || path == "." {
            continue;
        }
        let name = path.rsplit('/').next().unwrap_or("");
        if !name.is_empty() {
            units.push(Unit::PackageLib(name.to_string()));
        }
    }
    // The root package itself: `members` names it as ".", which carries no
    // crate name. It is bin-only, so its unit tests are `--bins`.
    units.push(Unit::PackageBins("darkmux".to_string()));

    units.sort();
    units.dedup();
    units
}

/// The one unit that still leaks, with its count measured on this branch
/// and the reason the fix is not a per-test guard. Tracked as #2732.
///
/// This is an EXEMPTION list, which is the shape this target exists to
/// argue against — so it is bounded three ways. The entry names a measured
/// COUNT, not a spelling. The assertion is `0 < files <= cap`, so growth
/// fails AND a unit that has been fixed also fails until its entry is
/// deleted. And the entry has to name a unit this check actually runs, so
/// it cannot outlive the thing it describes.
///
/// `mission_launch::launch()` emits the dependency-free liveness floor's
/// first marker (`process-start`) before anything else, and about
/// twenty-four tests in `src/mission_launch.rs` call it. Giving each one
/// `IsolatedState` requires `#[serial_test::serial]`, because that guard
/// mutates process-global environment — and those tests are not serial
/// today. Serializing two dozen of them is a real throughput change to the
/// suite and wants its own measurement.
///
/// Measured with `DARKMUX_HOME` UNSET — the ordinary developer machine —
/// the test-build default sends it to `/tmp/darkmux-test-isolated`
/// instead, so the operator's real tree is untouched. The exposure is an
/// operator who exports `DARKMUX_HOME`, and the artifact is a prunable
/// heartbeat file rather than a chained record.
const KNOWN_UNISOLATED_UNITS: &[(&str, usize, &str)] = &[(
    "test -p darkmux --bins",
    1,
    "mission_launch::launch()'s `process-start` liveness marker, from ~24 non-serial \
     tests; #2732",
)];

/// The free half: the enumeration itself, asserted on every `cargo test`.
///
/// Anti-vacuity in both directions — a walk that silently returned nothing
/// (a renamed directory, a manifest reformat that breaks the `members`
/// scrape) would make the measurement below pass while measuring nothing,
/// which is precisely the shape the guard this replaces failed in.
#[test]
fn every_test_unit_the_leak_check_would_run_is_enumerated_from_disk() {
    let units = enumerate_units();

    let targets: Vec<&String> = units
        .iter()
        .filter_map(|u| match u {
            Unit::IntegrationTarget(t) => Some(t),
            _ => None,
        })
        .collect();
    let packages: Vec<&String> = units
        .iter()
        .filter_map(|u| match u {
            Unit::PackageLib(p) | Unit::PackageBins(p) => Some(p),
            _ => None,
        })
        .collect();

    for (label, _, _) in KNOWN_UNISOLATED_UNITS {
        assert!(
            units.iter().any(|u| u.label() == *label),
            "KNOWN_UNISOLATED_UNITS names `{label}`, which is not a unit this check runs. \
             A stale exemption is an exemption nobody re-measures."
        );
    }

    assert!(
        targets.len() >= 10,
        "only {} integration target(s) enumerated; this check is looking at an empty or \
         wrong tree and its green proves nothing: {targets:?}",
        targets.len()
    );
    assert!(
        packages.len() >= 8,
        "only {} workspace package(s) scraped from Cargo.toml's `members`; the scrape has \
         broken and every in-process writer is now invisible to this check: {packages:?}",
        packages.len()
    );
    // Named explicitly: this file is the one that has to keep covering
    // itself, `cli.rs` holds the text-scan pre-check, and `darkmux-crew`
    // is the package #2718 measured the in-process leak in.
    for expected in ["cli", "state_files_owner_only_mode", "state_leak_execution_guard"] {
        assert!(
            targets.iter().any(|t| *t == expected),
            "the tests/ walk did not reach `{expected}`, one of the targets this check \
             exists to cover"
        );
    }
    assert!(
        packages.iter().any(|p| *p == "darkmux-crew"),
        "`darkmux-crew` is not in the enumerated packages; it is the package whose `--lib` \
         run was measured writing a findings file, 29 flow records and a liveness log into \
         a pinned root (#2718)"
    );
}

/// The measurement. Gated, because it re-runs the suite.
///
/// Set `DARKMUX_LEAK_CHECK=1` to run it; add
/// `DARKMUX_LEAK_CHECK_ONLY=<substring>` to narrow to one unit, which is
/// what makes red-proving a single target cheap.
#[test]
fn no_test_unit_writes_into_a_sentinel_state_tree() {
    if std::env::var_os("DARKMUX_LEAK_CHECK").is_none() {
        eprintln!(
            "skipped: set DARKMUX_LEAK_CHECK=1 to run the execution-side leak check \
             (DARKMUX_LEAK_CHECK_ONLY=<substring> narrows it to one unit)"
        );
        return;
    }
    let only = std::env::var("DARKMUX_LEAK_CHECK_ONLY").ok();
    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".to_string());
    let manifest_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));

    let units: Vec<Unit> = enumerate_units()
        .into_iter()
        // Running this target inside itself would recurse; the child would
        // see DARKMUX_LEAK_CHECK inherited and start its own sweep.
        .filter(|u| !matches!(u, Unit::IntegrationTarget(t) if t == "state_leak_execution_guard"))
        .filter(|u| only.as_ref().is_none_or(|o| u.label().contains(o.as_str())))
        .collect();
    assert!(
        !units.is_empty(),
        "no unit matched DARKMUX_LEAK_CHECK_ONLY={only:?}; a filter that matches nothing \
         passes this test while measuring nothing"
    );

    let mut offenders: Vec<String> = Vec::new();
    let mut clean = 0usize;
    for unit in &units {
        let sentinel = StateLeakSentinel::new();
        let mut cmd = std::process::Command::new(&cargo);
        cmd.current_dir(manifest_dir).args(unit.cargo_args());
        sentinel.apply(&mut cmd);
        // The child must not inherit the sweep flags, or every unit it
        // runs starts a sweep of its own.
        cmd.env_remove("DARKMUX_LEAK_CHECK");
        cmd.env_remove("DARKMUX_LEAK_CHECK_ONLY");
        // Cargo's jobserver file descriptors do not survive into an
        // unrelated child cleanly; letting it negotiate its own avoids a
        // warning storm on every unit.
        cmd.env_remove("CARGO_MAKEFLAGS");

        let out = cmd.output().unwrap_or_else(|e| panic!("spawning `cargo {}`: {e}", unit.label()));
        let census = sentinel.census();

        // A unit that ran NOTHING censuses as zero and would report clean
        // — the verdict is a file count, and no files is exactly what an
        // unexecuted target produces. Measured here, not imagined: the
        // first cut of this list drove the root package with `--lib`,
        // which exits 101 with "no library targets found in package
        // `darkmux`" having run no test binary at all, and the sweep
        // called it clean for eleven minutes. `test result:` is libtest's
        // own summary line, so its presence is proof a test binary ran.
        let saw_summary = String::from_utf8_lossy(&out.stdout).contains("test result:")
            || String::from_utf8_lossy(&out.stderr).contains("test result:");
        assert!(
            saw_summary,
            "`cargo {}` produced no libtest summary line, so no test binary ran and its \
             empty census proves nothing. A unit that cannot execute must fail loudly, \
             not pass quietly.\nstderr:\n{}",
            unit.label(),
            String::from_utf8_lossy(&out.stderr).chars().take(2000).collect::<String>(),
        );

        // The status is CONTEXT, never the verdict. Three of four targets
        // went red under #2710's leak conditions but all four leaked, and
        // one stayed green while leaking.
        if let Some((_, cap, why)) = KNOWN_UNISOLATED_UNITS.iter().find(|(l, _, _)| *l == unit.label())
        {
            let n = census.leaked_files();
            assert!(
                n > 0,
                "`cargo {}` is on KNOWN_UNISOLATED_UNITS but leaked NOTHING. If it has \
                 been fixed, delete its entry — an exemption nobody removes is how a \
                 measured list turns back into a list of excuses.",
                unit.label()
            );
            assert!(
                n <= *cap,
                "`cargo {}` leaked {n} file(s), above its recorded ceiling of {cap} \
                 ({why}). The exemption records a MEASURED residual, not a licence to \
                 grow:\n{}",
                unit.label(),
                census.report(12),
            );
            eprintln!("known residual: cargo {} — {n}/{cap} file(s) ({why})", unit.label());
            clean += 1;
            continue;
        }
        if census.leaked_files() == 0 {
            clean += 1;
            eprintln!("clean: cargo {} (child exit {:?})", unit.label(), out.status.code());
            continue;
        }
        offenders.push(format!(
            "  cargo {} — {} file(s) reached a darkmux state destination (child exit {:?})\n{}",
            unit.label(),
            census.leaked_files(),
            out.status.code(),
            census.report(12),
        ));
    }

    assert!(
        offenders.is_empty(),
        "(#2717) {} of {} test unit(s) wrote darkmux state outside their own fixtures. \
         Run in an ordinary terminal these writes land in the operator's REAL `~/.darkmux` \
         — their actual roster, missions, findings and flow stream — and, for the audit \
         chain, as records that cannot be removed without breaking it. The fix is for the \
         test to bring `darkmux_types::test_isolation::IsolatedState` (in-process) or to \
         spawn through a helper that calls `neutralize_state_vars` first (subprocess):\n{}",
        offenders.len(),
        units.len(),
        offenders.join("\n"),
    );
    assert!(clean > 0, "no unit was actually measured; this green is vacuous");
}
