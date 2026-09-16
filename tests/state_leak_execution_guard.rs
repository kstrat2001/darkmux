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
//! paragraph said it was. `enumerate_units` used to walk only the ROOT
//! `tests/*.rs` plus `-p <member> --lib`, and `--lib` neither builds nor
//! runs a package's own integration targets — so the eleven (now twelve)
//! under `crates/darkmux-{crew,fleet,gestalt,lab,profiles,types}/tests/`
//! sat outside this check, the same boundary `crates/*/tests/` still sits
//! outside for the text scan. Proven with a decoy at
//! `crates/darkmux-crew/tests/rev2733_decoy_leak.rs` writing into both
//! the real home and the state root: text scan EXIT=0, enumeration
//! EXIT=0, gated check EXIT=0 reporting `clean`, files present.
//!
//! **(#2736 item 1) Closed.** `Unit::PackageTest` now walks every workspace
//! member's own `tests/*.rs` the same way the root walk does. All twelve
//! targets censused clean before this landed, so this closes a coverage
//! hole rather than a live defect. Measured cost: the twelve added units
//! run in ~49s wall-clock sequentially (2026-09-16, this machine) against
//! the ~210s of CI margin #2736 measured — comfortably inside it. If a
//! future addition to `crates/*/tests/` ever pushes this sweep close to
//! the step's budget, re-measure before adding more rather than assuming
//! the old headroom still holds.
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
    /// (#2736 item 1) An integration target under a WORKSPACE MEMBER's own
    /// `tests/`, e.g. `crates/darkmux-crew/tests/mock_dispatch_proof.rs`.
    /// `-p <member> --lib` neither builds nor runs these — `--lib` is
    /// scoped to the package's library crate, and an integration target is
    /// its own separate binary — so they sat outside every unit above,
    /// the same boundary the root `tests/*.rs` walk sits outside of for
    /// `-p darkmux --lib`. Twelve exist today; all twelve census clean, so
    /// this closes a coverage hole rather than a live defect. `(package,
    /// target)`.
    PackageTest(String, String),
}

impl Unit {
    fn label(&self) -> String {
        match self {
            Unit::IntegrationTarget(t) => format!("test --test {t}"),
            Unit::PackageLib(p) => format!("test -p {p} --lib"),
            Unit::PackageBins(p) => format!("test -p {p} --bins"),
            Unit::PackageTest(p, t) => format!("test -p {p} --test {t}"),
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
            Unit::PackageTest(p, t) => vec![
                "test".into(),
                "-p".into(),
                p.clone(),
                "--test".into(),
                t.clone(),
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
        let member_dir = manifest_dir.join(path);

        // (#2736 item 4) The package's REAL name, read from ITS OWN
        // Cargo.toml rather than assumed from the directory basename. The
        // two disagree for nothing in this workspace today, but assuming
        // they always will is exactly the class of thing this whole file
        // exists to stop doing — a member directory renamed independently
        // of its package (or vice versa) would otherwise resolve to a
        // package name that does not exist, and `cargo test -p <that>`
        // fails with "package ID specification did not match any
        // packages", a message that points at the manifest, not at
        // vacuity, but which the OLD code could never produce because it
        // never looked.
        let member_manifest = std::fs::read_to_string(member_dir.join("Cargo.toml")).ok();
        let pkg_name = member_manifest
            .as_deref()
            .and_then(|m| {
                m.lines().find_map(|l| {
                    let (key, value) = l.split_once('=')?;
                    (key.trim() == "name").then(|| value.trim().trim_matches('"').to_string())
                })
            })
            .filter(|n| !n.is_empty())
            .unwrap_or_else(|| path.rsplit('/').next().unwrap_or("").to_string());
        if pkg_name.is_empty() {
            continue;
        }

        // (#2736 item 4) Which unit-test invocation actually builds and
        // runs this member's `#[cfg(test)]` modules depends on whether it
        // HAS a library target. `-p <pkg> --lib` on a bin-only member
        // fails with "no library targets found in package `<pkg>`" — a
        // message that (like the manifest-mismatch case above) points the
        // reader at vacuity rather than at the real cause. Every member
        // in this workspace today has a `src/lib.rs`; this branch exists
        // so a FUTURE bin-only member gets `--bins` automatically instead
        // of silently failing loudly for the wrong stated reason.
        if member_dir.join("src/lib.rs").exists() {
            units.push(Unit::PackageLib(pkg_name.clone()));
        } else {
            units.push(Unit::PackageBins(pkg_name.clone()));
        }

        // (#2736 item 1) The member's OWN `tests/*.rs` — integration
        // targets `-p <member> --lib`/`--bins` above never builds or
        // runs, because `--lib`/`--bins` are scoped to the package's
        // library/binary crate and an integration target is its own
        // separate binary. Reuses the `pkg_name` just resolved above
        // rather than re-deriving it from the directory a second time.
        if let Ok(entries) = std::fs::read_dir(member_dir.join("tests")) {
            for entry in entries.flatten() {
                let test_path = entry.path();
                if !test_path.extension().is_some_and(|e| e == "rs") {
                    continue;
                }
                if let Some(stem) = test_path.file_stem().and_then(|s| s.to_str()) {
                    units.push(Unit::PackageTest(pkg_name.clone(), stem.to_string()));
                }
            }
        }
    }
    // The root package itself: `members` names it as ".", which carries no
    // crate name and is skipped by the loop above. It is bin-only, so its
    // unit tests are `--bins`.
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
    let package_tests: Vec<(&String, &String)> = units
        .iter()
        .filter_map(|u| match u {
            Unit::PackageTest(p, t) => Some((p, t)),
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
    // (#2736 item 1) Twelve integration targets live under `crates/*/tests/`
    // today (`-p <member> --lib` never touches them). The floor is looser
    // than the measured count on purpose — it exists to catch the walk
    // going BLIND (a renamed `tests/` dir, a `members` scrape break), not
    // to pin an exact number every new target has to bump.
    assert!(
        package_tests.len() >= 10,
        "only {} package-scoped integration target(s) enumerated under crates/*/tests/; the \
         walk is looking at an empty or wrong set of member directories and its green proves \
         nothing: {package_tests:?}",
        package_tests.len()
    );
    assert!(
        package_tests.iter().any(|(p, _)| **p == "darkmux-crew"),
        "no `crates/darkmux-crew/tests/*.rs` target was enumerated; that member's own \
         integration tests are exactly what `-p darkmux-crew --lib` never runs (#2736 item 1)"
    );
}

/// What one child's libtest summary line(s) reported, summed across every
/// such line found in either stream — a `--bins` invocation covering more
/// than one bin target can print more than one `test result:` line.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
struct LibtestSummary {
    passed: usize,
    failed: usize,
    ignored: usize,
}

/// (#2735, #2736 item 3) Parse libtest's own `test result: ok|FAILED. N
/// passed; M failed; K ignored; ...` summary line(s) out of a child's
/// captured stdout+stderr. Returns `None` when no such line appears at all
/// anywhere in either stream — proof no test binary ever ran a libtest
/// harness to completion.
///
/// Split out as its own pure function, with its own tests below, because
/// the boolean it used to back (`saw_summary`, a bare `"test result:"`
/// substring match) had no test of its own: `grep -rn saw_summary` finds
/// only its own three lines, so deleting the floor it backed left every
/// always-on suite green (#2736 item 3). And the boolean itself proved
/// less than it looked like it proved: seven e2e units under a
/// `CARGO_TARGET_DIR` override failed `FleetHarness::boot()` inside
/// EVERY `#[test]` fn, so libtest still printed its own summary —
/// `test result: FAILED. 0 passed; 7 failed; …` — a real, non-empty
/// `"test result:"` line that satisfied the old floor while every one of
/// those units ran zero test bodies to completion (#2735).
///
/// `passed == 0` alone is not quite the whole story either, found while
/// landing #2736 item 1: `darkmux-crew/tests/{dispatch_panic_thread_leak_
/// proof,mock_dispatch_proof}.rs` are ENTIRELY `#[ignore]`d Docker-gated
/// proofs, so they legitimately print `0 passed; 0 failed; N ignored`
/// every time, by design, forever. Reporting `failed`/`ignored` alongside
/// `passed` (rather than collapsing straight to a bool) is what lets the
/// caller tell that KNOWN, intentional shape apart from the #2735 vacuity
/// bug it would otherwise be indistinguishable from — both look like
/// "zero completed" from `passed` alone.
fn parse_libtest_summary(stdout: &str, stderr: &str) -> Option<LibtestSummary> {
    fn field_before(tokens: &[&str], marker: &str) -> Option<usize> {
        let pos = tokens.iter().position(|t| t.starts_with(marker))?;
        tokens.get(pos.checked_sub(1)?)?.parse::<usize>().ok()
    }

    let mut total: Option<LibtestSummary> = None;
    for text in [stdout, stderr] {
        for line in text.lines() {
            if !line.contains("test result:") {
                continue;
            }
            let tokens: Vec<&str> = line.split_whitespace().collect();
            let acc = total.get_or_insert_with(LibtestSummary::default);
            acc.passed += field_before(&tokens, "passed").unwrap_or(0);
            acc.failed += field_before(&tokens, "failed").unwrap_or(0);
            acc.ignored += field_before(&tokens, "ignored").unwrap_or(0);
        }
    }
    total
}

/// The red-prove for `parse_libtest_summary`'s `None` case, and (paired
/// with the tests below) the thing #2736 item 3 says has never existed: a
/// test that drives the anti-vacuity floor with a deliberately
/// non-executing invocation, with no `cargo` spawn required. Delete the
/// `summary.is_some()` assertion in `no_test_unit_writes_into_a_sentinel_
/// state_tree` and this one still passes — it is scoped to the parser, not
/// the caller — but deleting `parse_libtest_summary` itself (or hardcoding
/// its result) turns this red immediately, along with the tests below.
#[test]
fn parse_libtest_summary_reports_none_when_no_summary_line_ever_printed() {
    // A binary that never reached libtest's own harness at all — killed,
    // or failed to link — prints no `test result:` line in either stream.
    assert_eq!(
        parse_libtest_summary("", "error: linking with `cc` failed: exit status: 1"),
        None
    );
}

/// The exact shape #2735 measured: the binary starts, every `#[test]` fn
/// fails during FIXTURE SETUP (not the behavior under test), and libtest
/// still prints its own honest `FAILED` summary. `saw_summary` alone
/// called this `clean`; the caller must be able to tell "ran and stayed
/// clean" from "ran and proved nothing" apart, and `passed == 0, failed >
/// 0` is exactly that signal.
#[test]
fn parse_libtest_summary_reports_zero_passed_for_the_2735_all_tests_failed_shape() {
    let stdout = "running 7 tests\n\
                  test fleet_status_deep_reports_all_nodes ... FAILED\n\
                  test harness_smoke ... FAILED\n\n\
                  failures:\n\n\
                  ---- fleet_status_deep_reports_all_nodes stdout ----\n\
                  thread 'fleet_status_deep_reports_all_nodes' panicked at 'spawning darkmux: \
                  No such file or directory (os error 2)'\n\n\
                  test result: FAILED. 0 passed; 7 failed; 0 ignored; 0 measured; 0 filtered out; \
                  finished in 0.31s\n\n";
    assert_eq!(
        parse_libtest_summary(stdout, ""),
        Some(LibtestSummary { passed: 0, failed: 7, ignored: 0 })
    );
}

/// The OTHER zero-passed shape, found live while landing #2736 item 1: a
/// target whose entire suite is `#[ignore]`d (Docker-gated), which prints
/// `0 passed; 0 failed; N ignored` by design. `failed == 0 && ignored > 0`
/// is what the caller uses to tell this apart from the #2735 shape above —
/// both have `passed == 0`, and only this field trio distinguishes them.
#[test]
fn parse_libtest_summary_reports_zero_passed_zero_failed_for_an_all_ignored_target() {
    let stdout = "running 4 tests\n\
                  test dispatch_i2596_forwards_the_cli_timeout_override_into_the_container ... \
                  ignored, requires Docker + a local darkmux-runtime:latest image\n\n\
                  test result: ok. 0 passed; 0 failed; 4 ignored; 0 measured; 0 filtered out; \
                  finished in 0.00s\n";
    assert_eq!(
        parse_libtest_summary(stdout, ""),
        Some(LibtestSummary { passed: 0, failed: 0, ignored: 4 })
    );
}

/// `--bins` can build and run more than one bin target in one invocation,
/// each printing its own `test result:` line — every field must be summed
/// across lines, not just taken from the first.
#[test]
fn parse_libtest_summary_sums_every_summary_line_present() {
    let stdout = "test result: ok. 3 passed; 0 failed; 1 ignored; 0 measured; 0 filtered out; \
                  finished in 0.01s\n\n\
                  test result: ok. 2 passed; 1 failed; 0 ignored; 0 measured; 0 filtered out; \
                  finished in 0.02s\n";
    assert_eq!(
        parse_libtest_summary(stdout, ""),
        Some(LibtestSummary { passed: 5, failed: 1, ignored: 1 })
    );
}

/// The check reads both streams — a summary line in stderr must count the
/// same as one in stdout.
#[test]
fn parse_libtest_summary_finds_a_summary_line_in_stderr_too() {
    let stderr = "test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; \
                  finished in 0.00s\n";
    assert_eq!(
        parse_libtest_summary("", stderr),
        Some(LibtestSummary { passed: 1, failed: 0, ignored: 0 })
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
    let mut inconclusive: Vec<String> = Vec::new();
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
        let stdout_text = String::from_utf8_lossy(&out.stdout).into_owned();
        let stderr_text = String::from_utf8_lossy(&out.stderr).into_owned();

        // A unit that ran NOTHING censuses as zero and would report clean
        // — the verdict is a file count, and no files is exactly what an
        // unexecuted target produces. Measured here, not imagined: the
        // first cut of this list drove the root package with `--lib`,
        // which exits 101 with "no library targets found in package
        // `darkmux`" having run no test binary at all, and the sweep
        // called it clean for eleven minutes. `test result:` is libtest's
        // own summary line, so its presence is proof a test binary STARTED
        // — but not proof any test in it ran to completion (#2735: seven
        // e2e units failed `FleetHarness::boot()` in every `#[test]` fn
        // under a `CARGO_TARGET_DIR` override and still printed a `FAILED`
        // summary). `parse_libtest_summary` is `None` for the former case;
        // the latter, and the OTHER zero-passed shape found while landing
        // #2736 item 1, are both handled below.
        let summary = parse_libtest_summary(&stdout_text, &stderr_text);
        assert!(
            summary.is_some(),
            "`cargo {}` produced no libtest summary line, so no test binary ran and its \
             empty census proves nothing. A unit that cannot execute must fail loudly, \
             not pass quietly.\nstderr:\n{}",
            unit.label(),
            stderr_text.chars().take(2000).collect::<String>(),
        );
        let summary = summary.expect("checked above");

        if summary.passed == 0 {
            if summary.failed == 0 && summary.ignored > 0 {
                // (#2736 item 1 follow-up) The ENTIRE suite is `#[ignore]`d
                // — e.g. `darkmux-crew`'s Docker-gated dispatch proofs,
                // which print `0 passed; 0 failed; N ignored` every time
                // BY DESIGN, forever. This is not the #2735 vacuity bug
                // (nothing failed either — libtest is reporting its own
                // honest "administratively skipped", not a broken fixture)
                // but it is not evidence of cleanliness either: nothing
                // executed, so it earns no `clean` credit and is reported
                // separately rather than silently inflating either count.
                // Known limitation, named rather than guarded against: a
                // test that genuinely started leaking and was then marked
                // `#[ignore]` to dodge this check would look identical.
                eprintln!(
                    "skipped (all {} test(s) #[ignore]d, none ran): cargo {}",
                    summary.ignored,
                    unit.label()
                );
                continue;
            }
            // (#2735) A test binary that ran but completed ZERO tests
            // (and did not merely skip them all) proves nothing about
            // leaks either way — its empty census is not evidence of
            // cleanliness, it is evidence the unit never reached its own
            // test bodies. Reported as its own category so it cannot
            // silently inflate `clean`.
            inconclusive.push(format!(
                "  cargo {} — ran (child exit {:?}) but 0 tests passed to completion ({} \
                 failed, {} ignored); an empty census from a unit that never executed its own \
                 tests is INCONCLUSIVE, not clean\n{}",
                unit.label(),
                out.status.code(),
                summary.failed,
                summary.ignored,
                stderr_text.chars().take(2000).collect::<String>(),
            ));
            continue;
        }

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

    // The message below names the e2e harness's release-binary resolver in
    // PROSE rather than by its symbol. `cli.rs`'s spawn guard is a scan over
    // an allowlist of SPELLINGS whose only exclusion is a leading `//`, so the
    // symbol appearing inside this string literal reads to it as a spawn site
    // and fails the guard. This file spawns `cargo`, never darkmux. Do not
    // "restore" the symbol name here.
    assert!(
        inconclusive.is_empty(),
        "(#2735) {} of {} test unit(s) printed a libtest summary but completed ZERO tests, so \
         their empty census is INCONCLUSIVE rather than evidence of cleanliness — a unit whose \
         tests never reach their own bodies cannot report `clean`. The known cause is a harness \
         that ignores `CARGO_TARGET_DIR` while `run_cargo_build_release` honors it (fixed in \
         the e2e harness's release-binary resolver), but treat this as \"investigate \
         why\", not \"assume that\":\n{}",
        inconclusive.len(),
        units.len(),
        inconclusive.join("\n"),
    );
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
