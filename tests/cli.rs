//! Integration tests for the darkmux CLI.
//!
//! These spawn the compiled binary and assert its observable surface:
//! exit codes, stdout/stderr shape, and behavior across the basic
//! subcommands. Tests that need a real `lms` are skipped when the
//! `DARKMUX_LMS_BIN` is unset (i.e., not in CI without LMStudio).

use assert_cmd::Command;
use predicates::prelude::*;
use std::collections::BTreeMap;
use std::fs;
use tempfile::TempDir;

// ===== DARKMUX-SPAWN-HELPERS: BEGIN (#2184) ==========================
//
// The only place this file names the darkmux binary. Every child process
// this file starts goes through `darkmux_cmd()` or `darkmux_std_cmd()`,
// and both scope the child to a FRESH, empty pair of roots by setting
// `DARKMUX_HOME` AND `HOME` (see `isolated_roots` for why one is not
// enough, and why they are siblings rather than nested).
//
// Why that is load-bearing and not hygiene: these spawn a real
// subprocess, so nothing about being launched FROM a test makes the
// child read a test-shaped configuration. `DARKMUX_HOME` is the FIRST
// tier of `paths::resolve` (`crates/darkmux-types/src/paths.rs`);
// without it the child resolves `./.darkmux` and then the developer's
// actual `~/.darkmux`, and every accessor that has no test-build guard
// of its own (`crew_dir_override`, `fleet_file`, `notebook_dir`,
// `identity_path_override`, `ack_dir_override`) then reads and WRITES
// the operator's real state. Measured 2026-09-07 against this file's own
// binary: with `DARKMUX_HOME` unset, `darkmux machine add` created
// `$HOME/.darkmux/fleet.json`.
//
// The incident that named this (#2184) is the same failure one crate
// over: during an ordinary `cargo test` sweep on 2026-08-31, five flow
// records were POSTed to a live crawl-tracker on 127.0.0.1:8790, because
// `hooks.enabled` and its rules were read out of the operator's real
// `~/.darkmux/config.json`. That particular vector happens to be closed
// for THIS file by an unrelated mechanism: `cargo test` feature-unifies
// `darkmux-types/test-support` (root `[dev-dependencies]`) into the bin
// target, so `config_access::config()` is empty by construction in the
// spawned child. Measured 2026-09-07, same `config.json` both ways:
// `flow status --json` reports `hooks.enabled: false` from the `cargo
// test` binary and `true` from a plain `cargo build` one. That is a
// side effect of a feature-resolution rule nobody stated as a guarantee,
// and it does nothing for the path tiers above. Isolate at the spawn.
//
// `every_darkmux_spawn_in_this_file_goes_through_the_isolating_helpers`
// (below) is the structural half: it fails if a raw spawn is ever
// reintroduced anywhere outside this block.

/// The darkmux binary under test. `CARGO_BIN_EXE_darkmux` is cargo's own
/// compile-time path to the bin target built for this integration test:
/// exact, and one source for both helpers below.
fn darkmux_bin_path() -> &'static str {
    env!("CARGO_BIN_EXE_darkmux")
}

/// A fresh `(HOME, DARKMUX_HOME)` pair for ONE spawned command.
///
/// Per-call rather than per-file so two tests running concurrently in
/// this process can never collide on `fleet.json` / `missions/`; plain
/// `PathBuf`s rather than a `TempDir` because a `TempDir` returned from
/// here would be dropped (and its directory deleted) at the end of the
/// caller's expression, typically before the child has even run. Both
/// live under one pid-named parent so a leftover tree is obviously this
/// test binary's.
///
/// They are SIBLINGS, not `<home>/.darkmux`, and that is load-bearing.
/// Six accessors in `config_access` (`lab_dir_default`,
/// `flows_dir_default`, `hooks_outbox_dir_default`,
/// `findings_dir_default`, `mods_dir_default`, `liveness_dir_default`)
/// carry a test-build guard that reads "if the resolved root IS
/// `dirs::home_dir()/.darkmux`, this test forgot to isolate itself" and
/// redirects to `/tmp/darkmux-test-isolated/...`. Nesting `DARKMUX_HOME`
/// under `HOME` makes a properly isolated child look exactly like an
/// un-isolated one to that check, and it fires: measured 2026-09-07,
/// `lab notebook draft` then resolved its run dir to
/// `/tmp/darkmux-test-isolated/runs` and could not see the fixture the
/// test had just written. Sibling roots keep the two distinguishable.
fn isolated_roots() -> (std::path::PathBuf, std::path::PathBuf) {
    use std::sync::atomic::{AtomicUsize, Ordering};
    static NEXT: AtomicUsize = AtomicUsize::new(0);
    let n = NEXT.fetch_add(1, Ordering::Relaxed);
    let base = std::env::temp_dir()
        .join(format!("darkmux-cli-tests-{}", std::process::id()))
        .join(format!("spawn-{n:04}"));
    let home = base.join("home");
    let darkmux_home = base.join("darkmux");
    for d in [&home, &darkmux_home] {
        fs::create_dir_all(d)
            .unwrap_or_else(|e| panic!("creating isolated root {}: {e}", d.display()));
    }
    (home, darkmux_home)
}

/// A `std::process::Command` for the darkmux binary, isolated. Use when
/// the test needs `.spawn()` / `Stdio` plumbing that
/// `assert_cmd::Command` does not offer.
///
/// TWO env vars, because `DARKMUX_HOME` alone does not actually contain
/// the child. It is the first tier of `paths::resolve`, and most darkmux
/// directories route through that — but several accessors reach for
/// `dirs::home_dir()` directly and never consult it:
/// `config_access::fleet_file`, `config_access::cache_dir`,
/// `residency_lease`'s root, `dispatch_liveness`'s root, and
/// `crew::dispatch`'s `identity.md`. Measured 2026-09-07: with
/// `DARKMUX_HOME` pointed at a tempdir, `darkmux machine add` still wrote
/// `$HOME/.darkmux/fleet.json`. `dirs::home_dir()` honors `$HOME` on
/// unix, so setting BOTH closes the whole family at once instead of
/// enumerating the leaks (and re-enumerating them every time a new one is
/// written). They point at the same tree, so a child sees one coherent
/// root either way it resolves.
///
/// (#2682 fix-pass round 3, MUST FIX 1) And one `env_remove`, because
/// `DARKMUX_CREW_DIR` OUTRANKS both of them. `crew::loader::
/// user_state_root()` — the resolver behind every `missions/` and
/// `phases/` read and write — asks `config_access::crew_dir_override()`
/// first and only falls back to the `DARKMUX_HOME` tier when that
/// override is absent, so a child spawned by this helper with
/// `DARKMUX_CREW_DIR` merely INHERITED from the ambient environment reads
/// and writes the operator's real board, whatever root the two vars above
/// name. Measured at this head with a sentinel exported: `cargo test
/// --test cli mission_status_` failed 4 of 8, because each test's fixture
/// (written under its own `DARKMUX_HOME`) was invisible to the subprocess
/// reading it back. Clearing it here restores the intended
/// `DARKMUX_HOME`-tier resolution for every spawn at once; the handful of
/// tests that genuinely exercise the override still set it explicitly
/// afterward, and a later `.env` wins over this `.env_remove`.
fn darkmux_std_cmd() -> std::process::Command {
    let (home, darkmux_home) = isolated_roots();
    let mut cmd = std::process::Command::new(darkmux_bin_path());
    cmd.env("HOME", home)
        .env("DARKMUX_HOME", darkmux_home)
        .env_remove("DARKMUX_CREW_DIR");
    cmd
}

/// The default: an `assert_cmd::Command` for the darkmux binary,
/// isolated. Same idiom as the e2e harness's `FleetNode::cmd()`
/// (`tests/e2e/harness.rs`): one constructor owns the isolation env, so
/// no call site can forget it.
///
/// A test that needs a SPECIFIC root (one it seeds, or one it later
/// asserts against) still comes through here and overrides with its own
/// `.env("DARKMUX_HOME", ...)`. A later `.env` wins, so the override is
/// explicit and the default is never a raw inherit.
fn darkmux_cmd() -> Command {
    Command::from_std(darkmux_std_cmd())
}

/// The one named override: a spawn scoped to a PROJECT-LOCAL darkmux
/// root at `<dir>/.darkmux`, with `<dir>` as the child's cwd.
///
/// The `lab fixture` / `lab doctor` / `lab notebook` / `lab run` tests
/// below seed a tempdir, create `<tempdir>/.darkmux`, and then assert
/// against that root (or run a SECOND command that has to see what the
/// first one wrote). They cannot take `darkmux_cmd()`'s per-call root:
/// each spawn would get a fresh one, so a `fixture register` in spawn 1
/// is invisible to a `fixture list` in spawn 2.
///
/// This is the override shape `darkmux_cmd()`'s doc describes — the root
/// is NAMED, not inherited — and the `HOME` half of the isolation from
/// `darkmux_std_cmd()` is untouched, so the `dirs::home_dir()` accessors
/// (`fleet_file` and friends) still cannot reach the operator.
///
/// Setting `DARKMUX_HOME` rather than relying on `paths::resolve`'s
/// project tier finding `./.darkmux` on its own is deliberate: the two
/// resolve to the identical set of paths (only `DarkmuxPaths::scope`
/// differs, which no production code reads), and an explicit value can't
/// be defeated by an inherited one.
fn darkmux_cmd_in_project(dir: &std::path::Path) -> Command {
    let mut cmd = darkmux_cmd();
    cmd.current_dir(dir).env("DARKMUX_HOME", dir.join(".darkmux"));
    cmd
}

/// (#2184) The BEHAVIORAL half. The structural guard below proves every
/// spawn is ROUTED through the helper; it cannot tell whether the helper
/// still isolates anything.
///
/// So: read the root the helper picked straight off the command it built,
/// then RUN that command and prove the operator state landed THERE and
/// nowhere near this process's own home. `machine add` is the probe on
/// purpose. Its roster path (`config_access::fleet_file`) is one of the
/// accessors that reaches `dirs::home_dir()` WITHOUT consulting
/// `DARKMUX_HOME`, so a helper that kept only the `DARKMUX_HOME` half
/// fails here rather than passing.
///
/// The child's cwd is a fresh tempdir so the `./.darkmux` tier of
/// `paths::resolve` cannot quietly absorb the write and turn a real
/// regression into a green run.
#[test]
fn darkmux_cmd_keeps_a_child_out_of_the_process_home() {
    let mut cmd = darkmux_std_cmd();
    let env: BTreeMap<std::ffi::OsString, Option<std::ffi::OsString>> = cmd
        .get_envs()
        .map(|(k, v)| (k.to_owned(), v.map(|v| v.to_owned())))
        .collect();
    let child_home = env
        .get(std::ffi::OsStr::new("HOME"))
        .cloned()
        .flatten()
        .map(std::path::PathBuf::from)
        .expect("(#2184) the spawn helper must set HOME on every child; several darkmux paths \
                 (fleet_file, cache_dir, residency_lease, dispatch_liveness, identity.md) \
                 resolve through dirs::home_dir() and never look at DARKMUX_HOME");
    let child_darkmux_home = env
        .get(std::ffi::OsStr::new("DARKMUX_HOME"))
        .cloned()
        .flatten()
        .map(std::path::PathBuf::from)
        .expect("(#2184) the spawn helper must set DARKMUX_HOME on every child");

    assert_ne!(
        child_darkmux_home,
        child_home.join(".darkmux"),
        "(#2184) DARKMUX_HOME must NOT be nested at `<HOME>/.darkmux` — see `isolated_roots`: \
         six config_access test-build guards read that exact equality as `this test forgot to \
         isolate itself` and silently redirect to /tmp/darkmux-test-isolated"
    );
    assert_ne!(
        Some(child_home.clone()),
        std::env::var_os("HOME").map(std::path::PathBuf::from),
        "(#2184) the helper handed the child THIS process's own home — in a real `cargo test` \
         run that is the operator's, and every roster/mission/notebook write is theirs"
    );

    let empty_cwd = TempDir::new().unwrap();
    let out = cmd
        .current_dir(empty_cwd.path())
        .args(["machine", "add", "spawn-isolation-probe", "--address", "127.0.0.1:1"])
        .output()
        .expect("running `darkmux machine add`");
    assert!(
        out.status.success(),
        "machine add failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    // Anti-vacuity: the negative assertions below mean nothing unless this
    // command really does write a roster somewhere.
    //
    // (#2450) Under EITHER helper root, not `HOME`'s specifically. Before
    // #2450 the roster came from `dirs::home_dir()`, so it could only land
    // under `<child_home>/.darkmux`; now `fleet_file` resolves through
    // `paths::resolve`, whose first branch is `DARKMUX_HOME` — so it
    // correctly lands under `child_darkmux_home` instead. Pinning the HOME
    // root specifically would make this guard fail on a fix that is working,
    // which is exactly what it did when the two branches first met. What the
    // guard is actually for is unchanged: the child wrote its state inside
    // the helper's own tree rather than into the operator's.
    let roster_under_home = child_home.join(".darkmux").join("fleet.json");
    let roster_under_darkmux_home = child_darkmux_home.join("fleet.json");
    assert!(
        roster_under_home.is_file() || roster_under_darkmux_home.is_file(),
        "(#2184/#2450) `machine add` wrote no roster under either of the helper's own roots \
         ({} or {}) — either the isolation is pointing somewhere unexpected, or this probe no \
         longer writes state and the assertions below are vacuous",
        roster_under_home.display(),
        roster_under_darkmux_home.display()
    );
    assert!(
        !empty_cwd.path().join(".darkmux").exists(),
        "(#2184) the child fell through to the project-local `./.darkmux` tier of \
         paths::resolve instead of the helper's root"
    );
}

/// (#2184) The structural half of the fix: a source-scanning conformance
/// test, in the shape this repo already uses elsewhere.
///
/// Before this pass, `tests/cli.rs` named the binary at 135 sites and set
/// `DARKMUX_HOME` at 55 of them. Nothing made the other 80 visible, and
/// nothing stops site 136 from being written the same way tomorrow. So
/// the invariant is asserted against this file's own source: the binary
/// may be named ONLY inside the helper block above.
///
/// The block itself is the trusted region, deliberately. It is ~90 lines
/// long, fenced by two markers, and it is where a reviewer looking for
/// "how do these tests isolate themselves" already has to look.
#[test]
fn every_darkmux_spawn_in_this_file_goes_through_the_isolating_helpers() {
    // Split with `concat!` on purpose: written as one literal, these two
    // lines would themselves contain the markers, and the scan below would
    // find the END marker HERE instead of at the real end of the block,
    // silently shrinking the trusted region to nothing.
    const BEGIN: &str = concat!("DARKMUX-SPAWN-", "HELPERS: BEGIN");
    const END: &str = concat!("DARKMUX-SPAWN-", "HELPERS: END");
    // The tokens that can only mean "this line resolves or spawns the
    // darkmux binary": `assert_cmd`'s `Command::cargo_bin` /
    // `assert_cmd::cargo::cargo_bin`, and cargo's own
    // `CARGO_BIN_EXE_darkmux` env.
    const SPAWN_TOKENS: [&str; 2] = ["cargo_bin", "CARGO_BIN_EXE_darkmux"];

    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/cli.rs");
    let src = fs::read_to_string(path).unwrap_or_else(|e| panic!("reading {path}: {e}"));
    let lines: Vec<&str> = src.lines().collect();

    let begin = lines
        .iter()
        .position(|l| l.contains(BEGIN))
        .unwrap_or_else(|| panic!("the `{BEGIN}` marker is gone from tests/cli.rs; this guard \
             cannot tell helper code from a raw spawn without it"));
    let end = lines
        .iter()
        .position(|l| l.contains(END))
        .unwrap_or_else(|| panic!("the `{END}` marker is gone from tests/cli.rs; this guard \
             cannot tell helper code from a raw spawn without it"));
    assert!(
        begin < end,
        "the spawn-helper markers are out of order (BEGIN at line {}, END at line {})",
        begin + 1,
        end + 1
    );

    let offenders: Vec<String> = lines
        .iter()
        .enumerate()
        // Inside the helper block is the one place the binary may be named.
        .filter(|(i, _)| *i < begin || *i > end)
        // A whole-line comment cannot spawn anything (this guard's own
        // prose above says `cargo_bin` several times).
        .filter(|(_, l)| !l.trim_start().starts_with("//"))
        .filter(|(_, l)| SPAWN_TOKENS.iter().any(|t| l.contains(t)))
        .map(|(i, l)| format!("  tests/cli.rs:{}: {}", i + 1, l.trim()))
        .collect();

    assert!(
        offenders.is_empty(),
        "(#2184) {} spawn site(s) in tests/cli.rs name the darkmux binary directly instead of \
         going through `darkmux_cmd()` / `darkmux_std_cmd()`. A raw spawn inherits the \
         developer's environment, so with no `DARKMUX_HOME` the child resolves the operator's \
         REAL `~/.darkmux` and reads and writes their actual roster, missions and notebook \
         (and, in a non-`test-support` build, POSTs their real hook rules). Route it through \
         the helper; override `DARKMUX_HOME` after the call if this test needs a specific \
         root:\n{}",
        offenders.len(),
        offenders.join("\n")
    );
}

// ===== DARKMUX-SPAWN-HELPERS: END (#2184) ============================

fn fixture_json() -> &'static str {
    r#"{
        "profiles": {
            "fast": {
                "description": "bounded tasks",
                "models": [
                    {"id": "model-a", "n_ctx": 32000, "role": "primary"}
                ]
            },
            "deep": {
                "description": "long tasks",
                "models": [
                    {"id": "model-a", "n_ctx": 100000, "role": "primary"},
                    {"id": "model-b", "n_ctx": 50000, "role": "compactor"}
                ]
            }
        },
        "default_profile": "fast"
    }"#
}

#[test]
fn version_outputs_semver() {
    let mut cmd = darkmux_cmd();
    cmd.arg("--version")
        .assert()
        .success()
        .stdout(predicate::str::contains("darkmux"));
}

#[test]
fn help_lists_subcommands() {
    let mut cmd = darkmux_cmd();
    cmd.arg("--help")
        .assert()
        .success()
        .stdout(predicate::str::contains("machine"))
        .stdout(predicate::str::contains("profile"))
        .stdout(predicate::str::contains("lab"));
}

// (#1426) The top-level `profiles` verb retired into `profile list`.
#[test]
fn profile_list_lists_from_explicit_config() {
    let tmp = TempDir::new().unwrap();
    let p = tmp.path().join("profiles.json");
    fs::write(&p, fixture_json()).unwrap();
    let mut cmd = darkmux_cmd();
    cmd.args(["profile", "list", "--profiles-file", p.to_str().unwrap()])
        .assert()
        .success()
        .stdout(predicate::str::contains("fast"))
        .stdout(predicate::str::contains("deep"))
        .stdout(predicate::str::contains("(default)"));
}

#[test]
fn profile_list_errors_when_config_missing() {
    let mut cmd = darkmux_cmd();
    cmd.args(["profile", "list", "--profiles-file", "/no/such/path.json"])
        .assert()
        .failure()
        .stderr(
            predicate::str::contains("registry not found")
                .or(predicate::str::contains("no profile registry")),
        );
}

// (#1426) The retired top-level spellings now fail with an unknown-subcommand
// error (no compat alias — pre-2.0 clean removal).
#[test]
fn retired_top_level_profiles_verb_is_unknown() {
    let mut cmd = darkmux_cmd();
    cmd.arg("profiles")
        .assert()
        .failure()
        .stderr(predicate::str::contains("unrecognized subcommand").or(
            predicate::str::contains("unexpected argument"),
        ));
}

#[test]
fn retired_top_level_scan_verb_is_unknown() {
    let mut cmd = darkmux_cmd();
    cmd.arg("scan")
        .assert()
        .failure()
        .stderr(predicate::str::contains("unrecognized subcommand").or(
            predicate::str::contains("unexpected argument"),
        ));
}

#[test]
fn retired_top_level_pr_review_verb_is_unknown() {
    let mut cmd = darkmux_cmd();
    cmd.arg("pr-review")
        .assert()
        .failure()
        .stderr(predicate::str::contains("unrecognized subcommand").or(
            predicate::str::contains("unexpected argument"),
        ));
}

#[test]
fn retired_top_level_notebook_verb_is_unknown() {
    let mut cmd = darkmux_cmd();
    cmd.arg("notebook")
        .assert()
        .failure()
        .stderr(predicate::str::contains("unrecognized subcommand").or(
            predicate::str::contains("unexpected argument"),
        ));
}

/// (#1426 phase 2) The `skills` top-level verb retired — `init` is the one
/// setup/refresh verb (it refreshes the bundled darkmux-* skills on re-run,
/// and `darkmux doctor` flags stale ones). The spelling has NO compat alias,
/// so clap rejects it as an unknown subcommand.
#[test]
fn retired_top_level_skills_verb_is_unknown() {
    let mut cmd = darkmux_cmd();
    cmd.arg("skills")
        .assert()
        .failure()
        .stderr(predicate::str::contains("unrecognized subcommand").or(
            predicate::str::contains("unexpected argument"),
        ));
}

/// (#1426 ship-2) The `crew` family retired ENTIRELY: phase 2 promoted
/// `dispatch` to a top-level verb, and the crew REGISTRY dissolved (a crew is
/// a derived view of a mission's resourcing), taking the registry-read verbs
/// (`crew list`/`show`/`index`) with it. Every crew spelling — the bare family
/// and each old sub-verb — is now an unknown TOP-LEVEL verb with no compat
/// alias (pre-2.0 clean removal).
#[test]
fn retired_crew_family_is_unknown_entirely() {
    for args in [
        vec!["crew"],
        vec!["crew", "dispatch", "code-reviewer"],
        vec!["crew", "list"],
        vec!["crew", "show", "review-deep"],
        vec!["crew", "index", "status"],
    ] {
        let mut cmd = darkmux_cmd();
        cmd.args(&args)
            .assert()
            .failure()
            .stderr(predicate::str::contains("unrecognized subcommand").or(
                predicate::str::contains("unexpected argument"),
            ));
    }
}

/// (#1426, decision 17) The `lessons` top-level verb retired into the `memory`
/// family — every spelling, the bare family and each old sub-verb, is now an
/// unknown TOP-LEVEL verb with no compat alias (pre-2.0 clean removal). The
/// surface moved to `memory lesson <sub>`; see the companion test below.
#[test]
fn retired_lessons_family_is_unknown_entirely() {
    for args in [
        vec!["lessons"],
        vec!["lessons", "list"],
        vec!["lessons", "add", "--title", "t", "--body", "b"],
        vec!["lessons", "recall", "--term", "x"],
        vec!["lessons", "export"],
    ] {
        let mut cmd = darkmux_cmd();
        cmd.args(&args)
            .assert()
            .failure()
            .stderr(predicate::str::contains("unrecognized subcommand").or(
                predicate::str::contains("unexpected argument"),
            ));
    }
}

/// (#1426, decision 17) The replacement surface EXISTS: `memory` carries both
/// kinds, `memory lesson` keeps all seven of the retired family's sub-verbs
/// (behavior + flags unchanged — the verb moved, nothing else), and
/// `memory correction` is read-only (a `list` and no write verb, since
/// corrections are recorded by the review path, never hand-authored).
/// The retirement test above only proves the OLD spelling is gone; this proves
/// the new one landed, so a rename that dropped a sub-verb can't pass both.
#[test]
fn memory_family_carries_both_kinds() {
    let help = |args: &[&str]| -> String {
        let out = darkmux_cmd()
            .args(args)
            .arg("--help")
            .output()
            .unwrap();
        String::from_utf8_lossy(&out.stdout).to_string()
    };

    let memory = help(&["memory"]);
    for kind in ["lesson", "correction"] {
        assert!(memory.contains(kind), "memory --help lists `{kind}`: {memory}");
    }

    let lesson = help(&["memory", "lesson"]);
    for sub in ["add", "list", "edit", "remove", "export", "import", "recall"] {
        assert!(
            lesson.contains(sub),
            "memory lesson --help keeps the retired family's `{sub}`: {lesson}"
        );
    }

    let correction = help(&["memory", "correction"]);
    assert!(correction.contains("list"), "{correction}");
    // Read-only by construction (#849): the review path records corrections as
    // flow notes, so a write verb here would be inventing a surface.
    for write_verb in ["add", "edit", "remove", "import"] {
        assert!(
            !correction.contains(&format!("  {write_verb} ")),
            "memory correction stays read-only — no `{write_verb}`: {correction}"
        );
    }
}

/// (#1465) The `lab` second-level surface regrouped: the flat plural-noun
/// leaves (`lab runs`/`lab workloads`/`lab fixtures`), the flat run leaves
/// (`lab inspect`/`lab compare`), the flat fixture-mutation leaves
/// (`lab register`/`lab unregister`), and the role-scoped snowflake
/// (`lab review-bench`) all retired into kind-families (`lab run {list,
/// inspect,compare}`, `lab workload list`, `lab fixture {list,register,
/// unregister}`) and the generalized `lab eval`. `lab` survives, so each is an
/// unknown SUB-verb within the surviving family. No compat alias (pre-2.0
/// clean removal).
#[test]
fn retired_lab_flat_subverbs_are_unknown() {
    for args in [
        vec!["lab", "runs"],
        vec!["lab", "workloads"],
        vec!["lab", "fixtures"],
        vec!["lab", "inspect", "some-run"],
        vec!["lab", "compare", "a", "b"],
        vec!["lab", "register", "/some/path"],
        vec!["lab", "unregister", "some-name"],
        vec!["lab", "review-bench"],
    ] {
        let mut cmd = darkmux_cmd();
        cmd.args(&args)
            .assert()
            .failure()
            .stderr(predicate::str::contains("unrecognized subcommand").or(
                predicate::str::contains("unexpected argument"),
            ));
    }
}

/// (#1465) The `--crew` flag on the review-eval path retired with the crew
/// family (#1426); it is now `--roster-profile`. clap rejects the old flag as
/// an unexpected argument (no compat alias).
#[test]
fn retired_crew_flag_on_lab_eval_is_unknown() {
    let mut cmd = darkmux_cmd();
    cmd.args(["lab", "eval", "--funnel", "--crew", "review-funnel"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("unexpected argument"));
}

/// (#1465) The replacement `lab` surface EXISTS: `lab --help` lists the new
/// kind-families, and each family's `--help` keeps its members. The retirement
/// test above only proves the OLD spellings are gone; this proves the new ones
/// landed, so a regroup that dropped a member can't pass both.
#[test]
fn lab_kind_families_carry_their_members() {
    let help = |args: &[&str]| -> String {
        let out = darkmux_cmd()
            .args(args)
            .arg("--help")
            .output()
            .unwrap();
        String::from_utf8_lossy(&out.stdout).to_string()
    };

    let lab = help(&["lab"]);
    for family in ["run", "workload", "fixture", "notebook", "eval"] {
        assert!(lab.contains(family), "lab --help lists `{family}`: {lab}");
    }

    // `lab run` carries the recorded-run sub-verbs AND still takes a workload.
    let run = help(&["lab", "run"]);
    for sub in ["list", "inspect", "compare"] {
        assert!(run.contains(sub), "lab run --help keeps `{sub}`: {run}");
    }
    assert!(
        run.to_lowercase().contains("workload"),
        "lab run --help still names the workload positional: {run}"
    );

    let workload = help(&["lab", "workload"]);
    assert!(workload.contains("list"), "lab workload --help has `list`: {workload}");

    let fixture = help(&["lab", "fixture"]);
    for sub in ["list", "register", "unregister"] {
        assert!(fixture.contains(sub), "lab fixture --help keeps `{sub}`: {fixture}");
    }

    // `lab eval` takes a role positional (default pr-reviewer) and the renamed
    // roster flag.
    let eval = help(&["lab", "eval"]);
    assert!(eval.to_lowercase().contains("role"), "lab eval --help names the role positional: {eval}");
    assert!(eval.contains("--roster-profile"), "lab eval --help has --roster-profile: {eval}");
    // The retired `--crew` flag must be gone. The word may still appear in the
    // `--roster-profile` doc's "renamed from `--crew`" note, so assert the
    // FLAG-DEFINITION form (`--crew <`) is absent, not the bare substring.
    assert!(!eval.contains("--crew <"), "lab eval --help must not define the retired --crew flag: {eval}");
}

/// (#1426 ship-4) `mission run` retired — the coder pipeline runs through
/// `mission launch coder-phase`. `mission` survives (launch/finalize/abort/…),
/// so the error is an unknown SUB-verb WITHIN the surviving family. No compat
/// alias (pre-2.0 clean removal).
#[test]
fn retired_mission_run_subverb_is_unknown() {
    let mut cmd = darkmux_cmd();
    cmd.args(["mission", "run", "some-mission"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("unrecognized subcommand").or(
            predicate::str::contains("unexpected argument"),
        ));
}

/// (#1463) The `phase` top-level verb family retired ENTIRELY: `estimate` +
/// `review` + the `start`/`complete`/`abandon` lifecycle trio. Every spelling —
/// the bare family and each old sub-verb — is now an unknown TOP-LEVEL verb with
/// no compat alias (pre-2.0 clean removal). (`mission add-phase` is a DIFFERENT,
/// surviving verb — it is NOT `darkmux phase`; see the mission-surface test.)
#[test]
fn retired_phase_family_is_unknown_entirely() {
    for args in [
        vec!["phase"],
        vec!["phase", "estimate", "spec.json"],
        vec!["phase", "review"],
        vec!["phase", "start", "s1"],
        vec!["phase", "complete", "s1"],
        vec!["phase", "abandon", "s1"],
    ] {
        let mut cmd = darkmux_cmd();
        cmd.args(&args)
            .assert()
            .failure()
            .stderr(predicate::str::contains("unrecognized subcommand").or(
                predicate::str::contains("unexpected argument"),
            ));
    }
}

/// (#1463) `mission ship` retired (the frontier does git/gh by hand, then
/// `mission finalize`) and `mission close` renamed to `mission finalize`. Both
/// old spellings are now unknown SUB-verbs within the surviving `mission`
/// family. No compat alias (pre-2.0 clean removal).
#[test]
fn retired_mission_ship_and_close_subverbs_are_unknown() {
    for args in [
        vec!["mission", "ship", "some-mission"],
        vec!["mission", "close", "some-mission"],
    ] {
        let mut cmd = darkmux_cmd();
        cmd.args(&args)
            .assert()
            .failure()
            .stderr(predicate::str::contains("unrecognized subcommand").or(
                predicate::str::contains("unexpected argument"),
            ));
    }
}

/// (#1463) The replacement surface EXISTS: the `mission` family lists `finalize`
/// and `abort` (the two whole-mission terminals) and keeps `add-phase`, while
/// `ship`/`close` are gone. Proves the rename landed — a change that dropped
/// `finalize` or re-added `ship`/`close` can't pass both this and the
/// retirement test above.
#[test]
fn mission_family_has_finalize_abort_addphase_but_not_ship_close() {
    let out = darkmux_cmd()
        .args(["mission", "--help"])
        .output()
        .expect("mission --help runs");
    let help = String::from_utf8_lossy(&out.stdout);
    let mut verbs: Vec<String> = Vec::new();
    let mut in_commands = false;
    for line in help.lines() {
        if line.trim_start().starts_with("Commands:") {
            in_commands = true;
            continue;
        }
        if !in_commands {
            continue;
        }
        if line.trim().is_empty() || line.starts_with("Options:") {
            break;
        }
        let indent = line.len() - line.trim_start().len();
        if indent == 0 || indent > 3 {
            continue; // section header or a wrapped description line
        }
        if let Some(tok) = line.split_whitespace().next() {
            if tok.chars().all(|c| c.is_ascii_lowercase() || c == '-') {
                verbs.push(tok.to_string());
            }
        }
    }
    for present in ["finalize", "abort", "add-phase"] {
        assert!(
            verbs.iter().any(|v| v == present),
            "mission help must list `{present}` (#1463); parsed verbs: {verbs:?}"
        );
    }
    for gone in ["ship", "close"] {
        assert!(
            !verbs.iter().any(|v| v == gone),
            "the `mission {gone}` verb must stay retired (#1463); parsed verbs: {verbs:?}"
        );
    }
}

/// (#1426 ship-4) VERBPAT drift guard: the `mission` family exposes NO `run`
/// subcommand after the collapse, but DOES keep `launch`. Anchored on the exact
/// command-column token so it never false-matches `mission launch` (the
/// two-word `mission run` anchor the ship-4 coverage directive names). Re-adding
/// `MissionCmd::Run` would list `run` in the help and fail this.
#[test]
fn mission_run_verb_absent_from_help_but_launch_present() {
    let out = darkmux_cmd()
        .args(["mission", "--help"])
        .output()
        .expect("mission --help runs");
    let help = String::from_utf8_lossy(&out.stdout);
    // Collect the command-name column under the "Commands:" section — the
    // verb token sits at a shallow (<=3 space) indent; wrapped description
    // lines sit deeper and are skipped, so we never read a description word.
    let mut in_commands = false;
    let mut verbs: Vec<String> = Vec::new();
    for line in help.lines() {
        if line.trim_start().starts_with("Commands:") {
            in_commands = true;
            continue;
        }
        if !in_commands {
            continue;
        }
        if line.trim().is_empty() || line.starts_with("Options:") {
            break;
        }
        let indent = line.len() - line.trim_start().len();
        if indent == 0 || indent > 3 {
            continue; // section header or a wrapped description line
        }
        if let Some(tok) = line.split_whitespace().next() {
            if tok.chars().all(|c| c.is_ascii_lowercase() || c == '-') {
                verbs.push(tok.to_string());
            }
        }
    }
    assert!(
        verbs.iter().any(|v| v == "launch"),
        "mission help must list `launch`; parsed verbs: {verbs:?}"
    );
    assert!(
        !verbs.iter().any(|v| v == "run"),
        "the `mission run` verb must stay retired (#1426 ship-4); parsed verbs: {verbs:?}"
    );
}

// (#1860) `mission config list`/`show` wiring — help-level presence plus one
// real end-to-end invocation of each, isolated via `DARKMUX_CREW_DIR` so the
// user tier is empty and deterministic (the on-disk `templates/builtin/`
// tier still resolves from cwd, and the two embedded built-ins always
// resolve regardless of either).

#[test]
fn mission_help_lists_config() {
    let out = darkmux_cmd()
        .args(["mission", "--help"])
        .output()
        .expect("mission --help runs");
    let help = String::from_utf8_lossy(&out.stdout);
    assert!(
        help.lines().any(|l| l.trim_start().starts_with("config")),
        "mission help must list `config` (#1860); got:\n{help}"
    );
}

#[test]
fn mission_config_help_lists_list_and_show() {
    let out = darkmux_cmd()
        .args(["mission", "config", "--help"])
        .output()
        .expect("mission config --help runs");
    let help = String::from_utf8_lossy(&out.stdout);
    assert!(help.contains("list"), "got:\n{help}");
    assert!(help.contains("show"), "got:\n{help}");
}

#[test]
fn mission_config_list_json_includes_the_two_embedded_builtins() {
    let tmp = TempDir::new().unwrap();
    let out = darkmux_cmd()
        // (merge-gate CONSIDER 5) `DARKMUX_HOME` isolates config.json +
        // profiles.json + mission-configs all at once (never the
        // operator's real `~/.darkmux`); `DARKMUX_LMS_BIN=/usr/bin/true`
        // means `lms ps --json` never shells to a real LMStudio.
        .env("DARKMUX_HOME", tmp.path())
        .env("DARKMUX_LMS_BIN", "/usr/bin/true")
        .args(["mission", "config", "list", "--json"])
        .output()
        .expect("mission config list --json runs");
    assert!(out.status.success(), "stderr: {}", String::from_utf8_lossy(&out.stderr));
    let stdout = String::from_utf8_lossy(&out.stdout);
    let v: serde_json::Value = serde_json::from_str(&stdout).expect("valid JSON");
    let configs = v["configs"].as_array().expect("configs is an array");
    let ids: Vec<&str> = configs.iter().map(|c| c["id"].as_str().unwrap()).collect();
    assert!(ids.contains(&"review"), "got ids: {ids:?}");
    assert!(ids.contains(&"coder-phase"), "got ids: {ids:?}");
    for c in configs {
        assert!(c.get("error").is_some(), "every row must carry an `error` key even when null");
    }
}

#[test]
fn mission_config_show_review_names_every_phase_and_flags_unconstructible_kinds() {
    let tmp = TempDir::new().unwrap();
    let out = darkmux_cmd()
        .env("DARKMUX_HOME", tmp.path())
        .env("DARKMUX_LMS_BIN", "/usr/bin/true")
        .args(["mission", "config", "show", "review", "--json"])
        .output()
        .expect("mission config show review --json runs");
    assert!(out.status.success(), "stderr: {}", String::from_utf8_lossy(&out.stderr));
    let stdout = String::from_utf8_lossy(&out.stdout);
    let v: serde_json::Value = serde_json::from_str(&stdout).expect("valid JSON");
    assert_eq!(v["id"], "review");
    let phase_ids: Vec<&str> =
        v["phases"].as_array().unwrap().iter().map(|p| p["id"].as_str().unwrap()).collect();
    assert_eq!(phase_ids, vec!["plan", "review", "summarize", "create-mods", "deliver"]);
    // Every step review.json declares is a real registered kind (Tier 1 +
    // the review Tier 3 kinds) — none should be flagged unconstructible.
    for phase in v["phases"].as_array().unwrap() {
        for task in phase["tasks"].as_array().unwrap() {
            for step in task["steps"].as_array().unwrap() {
                assert_eq!(
                    step["constructible"], true,
                    "step {:?} in the built-in review config must be constructible",
                    step
                );
            }
        }
    }
}

/// (#2310 P4c-2 review item 3 — proven; #2404 P4d round 3: `bundler`, the
/// shipped `review.json` input this test originally used, was removed
/// outright, so the test was retargeted to review's `mode` input; the
/// post-#2431 fix loop then deleted `mode` too — see review.json's own
/// `inputs` doc — once nothing turned out to pass it. Retargeting a THIRD
/// time to some other shipped config's ignored input would just repeat the
/// same fragility, so this test now plants its OWN synthetic config with
/// one `ignored: true` input, same as `an_ignored_input_no_step_references_
/// is_clean` does at the unit level in `mission_config/inputs.rs` — the
/// rendering this proves has nothing to do with WHICH config declares the
/// input.) `mission config show` must render an ignored input's
/// `ignored`/`ignored_reason` — both in `--json` (typed fields, always
/// present) and in the text form (`(optional, ignored: <reason>)`), so an
/// operator sees the same signal launch-time gives without having to
/// launch first.
#[test]
fn mission_config_show_renders_an_ignored_input() {
    let tmp = TempDir::new().unwrap();
    let config_dir = tmp.path().join("mission-configs");
    fs::create_dir_all(&config_dir).unwrap();
    let config_json = r#"{
        "id": "ignored-input-test",
        "name": "Ignored Input Test",
        "schema_version": "3.1",
        "inputs": [
            {
                "name": "legacy_flag",
                "description": "kept for CLI-surface parity; nothing in this document reads it",
                "required": false,
                "ignored": true,
                "ignored_reason": "no step in this document consumes it"
            },
            {"name": "message", "description": "the noop step's own config value", "required": true}
        ],
        "phases": [{
            "id": "p1",
            "tasks": [{
                "id": "t1",
                "steps": [{"id": "s1", "kind": "procedural.noop", "config": {"text": "{{message}}"}}]
            }]
        }]
    }"#;
    fs::write(config_dir.join("ignored-input-test.json"), config_json).unwrap();

    let json_out = darkmux_cmd()
        .env("DARKMUX_HOME", tmp.path())
        .args(["mission", "config", "show", "ignored-input-test", "--json"])
        .output()
        .expect("mission config show ignored-input-test --json runs");
    assert!(json_out.status.success(), "stderr: {}", String::from_utf8_lossy(&json_out.stderr));
    let v: serde_json::Value = serde_json::from_str(&String::from_utf8_lossy(&json_out.stdout)).expect("valid JSON");
    let legacy = v["inputs"]
        .as_array()
        .unwrap()
        .iter()
        .find(|i| i["name"] == serde_json::json!("legacy_flag"))
        .expect("the legacy_flag input is listed");
    assert_eq!(legacy["ignored"], serde_json::json!(true), "{legacy}");
    let reason = legacy["ignored_reason"].as_str().expect("a reason string");
    assert!(!reason.is_empty(), "{legacy}");

    let text_out = darkmux_cmd()
        .env("DARKMUX_HOME", tmp.path())
        .args(["mission", "config", "show", "ignored-input-test"])
        .output()
        .expect("mission config show ignored-input-test runs");
    assert!(text_out.status.success(), "stderr: {}", String::from_utf8_lossy(&text_out.stderr));
    let text = String::from_utf8_lossy(&text_out.stdout);
    assert!(
        text.contains(&format!("legacy_flag (optional, ignored: {reason})")),
        "text output must render the ignored form:\n{text}"
    );

    // A LIVE (non-ignored) input on the same config must NOT get the
    // ignored suffix.
    assert!(
        text.contains("message (required)\n") || text.contains("message (required,"),
        "a live input keeps the plain form:\n{text}"
    );
    assert!(
        !text.contains("message (required, ignored"),
        "a live input must never render an ignored suffix:\n{text}"
    );
}

#[test]
fn mission_config_show_unknown_id_exits_nonzero_with_hint() {
    let tmp = TempDir::new().unwrap();
    darkmux_cmd()
        .env("DARKMUX_HOME", tmp.path())
        .env("DARKMUX_LMS_BIN", "/usr/bin/true")
        .args(["mission", "config", "show", "totally-not-a-real-config"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("not found"));
}

/// (merge-gate MUST-FIX 3; #2310 P4d) `--param <role>=<profile>` no longer
/// applies as a launch binding on ANY route — the review route was the only
/// launcher that did, and it retired with the bespoke funnel. `show` must
/// mirror that: the override is reported as ignored, with a warning, and the
/// role still resolves through the registry's own binding.
#[test]
fn mission_config_show_review_param_override_is_reported_ignored_with_a_warning() {
    let tmp = TempDir::new().unwrap();
    let profiles_path = tmp.path().join("profiles.json");
    fs::write(
        &profiles_path,
        r#"{"profiles":{"deep":{"models":[{"id":"m-deep","n_ctx":8000}]}},"default_profile":"deep"}"#,
    )
    .unwrap();
    let out = darkmux_cmd()
        .env("DARKMUX_HOME", tmp.path())
        .env("DARKMUX_LMS_BIN", "/usr/bin/true")
        .args([
            "mission",
            "config",
            "show",
            "review",
            "--json",
            "--profiles-file",
            profiles_path.to_str().unwrap(),
            "--param",
            "reviewer=deep",
        ])
        .output()
        .expect("runs");
    assert!(out.status.success(), "stderr: {}", String::from_utf8_lossy(&out.stderr));
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    let warnings = v["warnings"].as_array().unwrap();
    assert!(
        warnings.iter().any(|w| w.as_str().unwrap_or("").contains("ignored")),
        "an override no launcher applies must be reported, not silently honored: {warnings:?}"
    );
    let reviewer_role = v["phases"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|p| p["tasks"].as_array().unwrap())
        .find_map(|t| {
            let r = &t["role"];
            (r["role_id"] == "reviewer").then(|| r.clone())
        })
        .expect("a reviewer task must be present");
    assert_ne!(
        reviewer_role["provenance"], "launch override (--param)",
        "no launcher applies --param role overrides any more: {reviewer_role}"
    );
}

/// (merge-gate MUST-FIX 3, end to end) `mission launch` never converts
/// `--param <role>=<profile>` into a binding for a NON-review-route config
/// (coder-phase's `--param role=<id>` is a different knob entirely). `show`
/// must neuter the override and warn, not silently apply it.
#[test]
fn mission_config_show_coder_phase_param_is_neutered_with_warning() {
    let tmp = TempDir::new().unwrap();
    let out = darkmux_cmd()
        .env("DARKMUX_HOME", tmp.path())
        .env("DARKMUX_LMS_BIN", "/usr/bin/true")
        .args([
            "mission",
            "config",
            "show",
            "coder-phase",
            "--json",
            "--param",
            "coder=deep",
        ])
        .output()
        .expect("runs");
    assert!(out.status.success(), "stderr: {}", String::from_utf8_lossy(&out.stderr));
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    let warnings = v["warnings"].as_array().unwrap();
    assert!(
        warnings.iter().any(|w| w.as_str().unwrap().contains("ignored")),
        "got warnings: {warnings:?}"
    );
    let coder_role = v["phases"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|p| p["tasks"].as_array().unwrap())
        .find_map(|t| {
            let r = &t["role"];
            (r["role_id"] == "coder").then(|| r.clone())
        })
        .expect("coder task must be present");
    assert_ne!(
        coder_role["provenance"], "launch override (--param)",
        "a non-review-route config must never claim the launch-override provenance from --param"
    );
}

/// (merge-gate CONSIDER 7) Mirrors `machine_status_explicit_bad_profiles_file_errors_loudly`:
/// an EXPLICIT `--profiles-file` that fails to load errors loudly. Only
/// the no-arg default degrades.
#[test]
fn mission_config_show_explicit_bad_profiles_file_errors_loudly() {
    let tmp = TempDir::new().unwrap();
    darkmux_cmd()
        .env("DARKMUX_HOME", tmp.path())
        .env("DARKMUX_LMS_BIN", "/usr/bin/true")
        .args([
            "mission",
            "config",
            "show",
            "review",
            "--profiles-file",
            "/no/such/path.json",
        ])
        .assert()
        .failure()
        .stderr(
            predicate::str::contains("registry not found")
                .or(predicate::str::contains("no profile registry"))
                .or(predicate::str::contains("profiles-file")),
        );
}

// (#1426 phase 3) `swap`, `status`, `model`, `fleet`, and `recommendations`
// all retired as top-level verbs with NO compat alias (pre-2.0 clean removal).
// `swap` (the second residency writer) is gone entirely; `status`/`model`/
// `fleet` folded into the `machine` family.
#[test]
fn retired_top_level_swap_verb_is_unknown() {
    let mut cmd = darkmux_cmd();
    cmd.arg("swap").assert().failure().stderr(
        predicate::str::contains("unrecognized subcommand")
            .or(predicate::str::contains("unexpected argument")),
    );
}

#[test]
fn retired_top_level_status_verb_is_unknown() {
    let mut cmd = darkmux_cmd();
    cmd.arg("status").assert().failure().stderr(
        predicate::str::contains("unrecognized subcommand")
            .or(predicate::str::contains("unexpected argument")),
    );
}

#[test]
fn retired_top_level_model_verb_is_unknown() {
    let mut cmd = darkmux_cmd();
    cmd.arg("model").assert().failure().stderr(
        predicate::str::contains("unrecognized subcommand")
            .or(predicate::str::contains("unexpected argument")),
    );
}

#[test]
fn retired_top_level_fleet_verb_is_unknown() {
    let mut cmd = darkmux_cmd();
    cmd.arg("fleet").assert().failure().stderr(
        predicate::str::contains("unrecognized subcommand")
            .or(predicate::str::contains("unexpected argument")),
    );
}

#[test]
fn retired_top_level_recommendations_verb_is_unknown() {
    let mut cmd = darkmux_cmd();
    cmd.arg("recommendations").assert().failure().stderr(
        predicate::str::contains("unrecognized subcommand")
            .or(predicate::str::contains("unexpected argument")),
    );
}

// (#1426) The `machine` family is present. `machine status` (absorbs the
// retired `status`) shows the matching-profile line; bare `machine` routes to
// `machine status` (one code path, no separate overview render).
#[test]
fn machine_status_runs_with_explicit_profiles() {
    let tmp = TempDir::new().unwrap();
    let p = tmp.path().join("profiles.json");
    fs::write(&p, fixture_json()).unwrap();
    let mut cmd = darkmux_cmd();
    cmd.env("DARKMUX_LMS_BIN", "/usr/bin/true");
    cmd.args(["machine", "status", "--profiles-file", p.to_str().unwrap()])
        .assert()
        .success()
        .stdout(predicate::str::contains("darkmux-managed"))
        .stdout(predicate::str::contains("matches"));
}

#[test]
fn bare_machine_routes_to_status() {
    let mut cmd = darkmux_cmd();
    cmd.env("DARKMUX_LMS_BIN", "/usr/bin/true");
    cmd.arg("machine")
        .assert()
        .success()
        .stdout(predicate::str::contains("darkmux-managed"));
}

/// (#1426) `machine status --json` emits the machine-readable shape the
/// frontier orchestrator parses: ownership groups plus the absorbed `status`
/// verb's `matching_profiles` + `registry` provenance keys.
#[test]
fn machine_status_json_carries_matching_profiles_and_registry_keys() {
    let tmp = TempDir::new().unwrap();
    let p = tmp.path().join("profiles.json");
    fs::write(&p, fixture_json()).unwrap();
    let out = darkmux_cmd()
        .env("DARKMUX_LMS_BIN", "/usr/bin/true")
        .args([
            "machine",
            "status",
            "--json",
            "--profiles-file",
            p.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(out.status.success());
    let json: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    for key in ["managed", "user_state", "matching_profiles", "registry"] {
        assert!(json.get(key).is_some(), "missing `{key}` in: {json}");
    }
    assert!(json["matching_profiles"].is_array());
}

/// (#1426 gate fix) An EXPLICIT `--profiles-file` that doesn't load errors
/// loudly — the retired `status` verb's behavior. Only the no-arg default
/// degrades to residents-without-match.
#[test]
fn machine_status_explicit_bad_profiles_file_errors_loudly() {
    let mut cmd = darkmux_cmd();
    cmd.env("DARKMUX_LMS_BIN", "/usr/bin/true");
    cmd.args(["machine", "status", "--profiles-file", "/no/such/path.json"])
        .assert()
        .failure()
        .stderr(
            predicate::str::contains("registry not found")
                .or(predicate::str::contains("no profile registry"))
                .or(predicate::str::contains("profiles-file")),
        );
}

/// Serve `count` canned HTTP responses on an ephemeral loopback port,
/// then stop. Returns the bound `host:port`.
fn canned_http_peer(status_line: &'static str, body: &'static str, count: usize) -> String {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap().to_string();
    std::thread::spawn(move || {
        for _ in 0..count {
            let Ok((mut stream, _)) = listener.accept() else { break };
            use std::io::{Read, Write};
            let mut buf = [0u8; 4096];
            let _ = stream.read(&mut buf);
            let resp = format!(
                "HTTP/1.1 {status_line}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = stream.write_all(resp.as_bytes());
        }
    });
    addr
}

/// Register `id` at `addr` in a roster under a per-test DARKMUX_FLEET_FILE,
/// via the real `machine add` verb. Returns the roster file path.
fn roster_with_peer(tmp: &TempDir, id: &str, addr: &str) -> std::path::PathBuf {
    let fleet_file = tmp.path().join("fleet.json");
    darkmux_cmd()
        .env("DARKMUX_FLEET_FILE", &fleet_file)
        .args(["machine", "add", id, "--address", addr])
        .assert()
        .success();
    fleet_file
}

/// (#1426 gate fix) A peer whose daemon answers but can't reach LMStudio
/// (`lms_unreachable: true`) must NOT render as a healthy-empty machine —
/// residents are UNKNOWN, not zero. Loud message, exit 2.
#[test]
fn machine_status_remote_degraded_peer_is_not_healthy_empty() {
    let addr = canned_http_peer("200 OK", r#"{"models":[],"lms_unreachable":true,"generated_at_ms":1}"#, 1);
    let tmp = TempDir::new().unwrap();
    let fleet_file = roster_with_peer(&tmp, "peer1", &addr);
    let out = darkmux_cmd()
        .env("DARKMUX_FLEET_FILE", &fleet_file)
        .args(["machine", "status", "peer1"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2), "degraded peer must exit 2");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("UNKNOWN"), "must say residents unknown: {stderr}");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        !stdout.contains("exclusively darkmux's"),
        "must NOT render the healthy-empty view: {stdout}"
    );
}

/// (#1426) A healthy peer read renders its residents partitioned by
/// ownership; `--json` carries the `machine_id` provenance key.
#[test]
fn machine_status_remote_happy_path_json_carries_machine_id() {
    let body = r#"{"models":[{"identifier":"darkmux:qwen-x","model":"qwen-x","status":"loaded","size":"4 GB","context":32000}],"lms_unreachable":false,"generated_at_ms":1}"#;
    let addr = canned_http_peer("200 OK", body, 1);
    let tmp = TempDir::new().unwrap();
    let fleet_file = roster_with_peer(&tmp, "peer1", &addr);
    let out = darkmux_cmd()
        .env("DARKMUX_FLEET_FILE", &fleet_file)
        .args(["machine", "status", "peer1", "--json"])
        .output()
        .unwrap();
    assert!(out.status.success(), "stderr: {}", String::from_utf8_lossy(&out.stderr));
    let json: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(json["machine_id"], "peer1");
    assert_eq!(json["managed"][0]["identifier"], "darkmux:qwen-x");
}

/// (#1426) A peer payload whose `models` doesn't parse (older/newer daemon
/// shape) falls back to a raw JSON print — never a fabricated-empty render.
#[test]
fn machine_status_remote_shape_mismatch_prints_raw_json() {
    let addr = canned_http_peer("200 OK", r#"{"future_shape":{"models_v2":[]}}"#, 1);
    let tmp = TempDir::new().unwrap();
    let fleet_file = roster_with_peer(&tmp, "peer1", &addr);
    let out = darkmux_cmd()
        .env("DARKMUX_FLEET_FILE", &fleet_file)
        .args(["machine", "status", "peer1"])
        .output()
        .unwrap();
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("future_shape"), "raw payload passthrough: {stdout}");
    assert!(!stdout.contains("darkmux-managed"), "no fabricated render: {stdout}");
}

#[test]
fn unknown_command_exits_nonzero() {
    let mut cmd = darkmux_cmd();
    cmd.arg("nonexistent-command").assert().failure();
}

#[test]
fn lab_with_no_subcommand_reports() {
    let mut cmd = darkmux_cmd();
    cmd.args(["lab"])
        .assert()
        .stderr(predicate::str::contains("not yet wired").or(predicate::str::contains("lab")));
}

/// End-to-end: `darkmux lab run quick-q` works from a non-source CWD using the
/// embedded built-in workload. This is the headline guarantee of the embedded
/// approach — `cargo install --path .` produces a binary that doesn't need
/// the source tree at runtime.
///
/// **Requires Docker** (#1405 removed the legacy openclaw shell-out runtime,
/// which this test previously mocked via `--runtime-cmd /usr/bin/true` to
/// avoid needing a real backend in CI). The internal runtime is now the only
/// dispatch path and it always spawns a real container, so this test needs
/// `darkmux-runtime:latest` built locally — matches the
/// `mock_dispatch_proof` test's "Docker required → `#[ignore]`d by default"
/// convention so `cargo test --workspace` never requires Docker. Run
/// explicitly with:
///
/// ```sh
/// cargo test --test cli lab_run_quick_q_from_clean_cwd_uses_embedded_workload -- --ignored
/// ```
#[test]
#[ignore]
fn lab_run_quick_q_from_clean_cwd_uses_embedded_workload() {
    let tmp = TempDir::new().unwrap();
    // Profile registry with `deep` as default.
    let cfg = tmp.path().join("profiles.json");
    fs::write(
        &cfg,
        r#"{
            "profiles": {
                "deep": {
                    "description": "test deep stack",
                    "models": [
                        {"id": "model-a", "n_ctx": 100000, "role": "primary"}
                    ]
                }
            },
            "default_profile": "deep"
        }"#,
    )
    .unwrap();

    // Force project-scope path resolution: paths::resolve(Auto) falls back to
    // ~/.darkmux/ when `./.darkmux/` is absent. Pre-create the project dir so
    // the test writes to the tempdir, not the user's home.
    fs::create_dir_all(tmp.path().join(".darkmux")).unwrap();

    let mut cmd = darkmux_cmd_in_project(tmp.path());
    // Force an empty templates dir so on-disk lookup doesn't accidentally
    // resolve before the embedded fallback. This proves the embedded path.
    cmd.env(
        "DARKMUX_TEMPLATES_DIR",
        tmp.path().join("nope").to_str().unwrap(),
    );
    cmd.args([
        "lab",
        "run",
        "quick-q",
        "--profiles-file",
        cfg.to_str().unwrap(),
        "--quiet",
    ])
    .assert()
    .success();

    // The run dir should exist under .darkmux/runs/<id>/ in the tempdir,
    // and contain a v2 manifest with the right run_id.
    let runs_dir = tmp.path().join(".darkmux").join("runs");
    assert!(
        runs_dir.is_dir(),
        "expected {} to exist",
        runs_dir.display()
    );
    let entries: Vec<_> = fs::read_dir(&runs_dir)
        .unwrap()
        .filter_map(|e| e.ok())
        .filter(|e| e.path().is_dir())
        .collect();
    assert_eq!(entries.len(), 1, "expected exactly one run dir");
    let run_dir = entries[0].path();
    let run_id = run_dir.file_name().unwrap().to_str().unwrap().to_string();
    assert!(
        run_id.starts_with("quick-q-deep-"),
        "expected run_id to start with workload-profile-, got: {run_id}"
    );

    let manifest_raw = fs::read_to_string(run_dir.join("manifest.json")).unwrap();
    let manifest: serde_json::Value = serde_json::from_str(&manifest_raw).unwrap();
    // (#487, #489) Phase 2 of the lab cluster: lab/run.rs's
    // enrich_manifest_with_fixture_info adds the `fixture` section
    // post-provider and bumps schema_version to 4. Pre-Phase-1 was v2;
    // Phase 1 (coding-task only) was v3; Phase 2 brings v4 to ALL
    // providers via the enrich step.
    assert_eq!(manifest["schema_version"].as_u64(), Some(4));
    assert_eq!(manifest["workload"].as_str(), Some("quick-q"));
    assert_eq!(manifest["provider"].as_str(), Some("prompt"));
    assert_eq!(manifest["profile"].as_str(), Some("deep"));
    assert_eq!(manifest["run_id"].as_str(), Some(run_id.as_str()));
    assert_eq!(manifest["ok"].as_bool(), Some(true));
    // Phase 2 fixture section: for a self-contained workload (quick-q
    // has no source sandbox) BOTH baseline_hash and source_path are
    // null — the #496 resolution records an explicit "no source" signal
    // rather than a non-canonical raw-path fallback that would
    // spuriously mismatch a canonicalized run under `dm lab compare`.
    assert!(
        manifest["fixture"].is_object(),
        "expected fixture section, got: {}",
        manifest["fixture"]
    );
    assert!(
        manifest["fixture"]["baseline_hash"].is_null(),
        "expected null baseline_hash for self-contained workload, got: {}",
        manifest["fixture"]["baseline_hash"]
    );
    assert!(
        manifest["fixture"]["source_path"].is_null(),
        "expected null source_path for self-contained workload, got: {}",
        manifest["fixture"]["source_path"]
    );
}

/// `notebook list` enumerates .md files and prints aligned columns.
#[serial_test::serial]
#[test]
fn notebook_list_shows_entries() {
    let tmp = TempDir::new().unwrap();
    let nb_dir = tmp.path().join("notebook");
    fs::create_dir_all(&nb_dir).unwrap();

    // Create a few notebook entries.
    fs::write(
        nb_dir.join("2026-05-10-run-a.md"),
        "<!-- darkmux:notebook-entry: run=abc123 machine=m5-home date=2026-05-10 -->\n\nContent A.",
    )
    .unwrap();
    fs::write(
        nb_dir.join("2026-05-11-run-b.md"),
        "<!-- darkmux:notebook-entry: run=def456 machine=m3-laptop date=2026-05-11 -->\n\nContent B.",
    )
    .unwrap();

    let mut cmd = darkmux_cmd();
    // Set notebook dir via env var.
    cmd.env("DARKMUX_NOTEBOOK_DIR", nb_dir.to_str().unwrap())
        .arg("lab")
        .arg("notebook")
        .arg("list")
        .assert()
        .success()
        .stdout(predicate::str::contains("2026-05-11"))
        .stdout(predicate::str::contains("2026-05-10"))
        .stdout(predicate::str::contains("def456"))
        .stdout(predicate::str::contains("abc123"));
}

/// `notebook list --machine` filters entries.
#[serial_test::serial]
#[test]
fn notebook_list_machine_filter() {
    let tmp = TempDir::new().unwrap();
    let nb_dir = tmp.path().join("notebook");
    fs::create_dir_all(&nb_dir).unwrap();

    fs::write(
        nb_dir.join("e1.md"),
        "<!-- darkmux:notebook-entry: run=r1 machine=m5-home date=2026-05-10 -->\n",
    )
    .unwrap();
    fs::write(
        nb_dir.join("e2.md"),
        "<!-- darkmux:notebook-entry: run=r2 machine=m3-laptop date=2026-05-11 -->\n",
    )
    .unwrap();

    // Filter to m5-home.
    let mut cmd = darkmux_cmd();
    cmd.env("DARKMUX_NOTEBOOK_DIR", nb_dir.to_str().unwrap())
        .arg("lab")
        .arg("notebook")
        .arg("list")
        .arg("--machine")
        .arg("m5-home")
        .assert()
        .success()
        // Assert on the machine name, NOT the 2-char run id: `notebook list`
        // prints each entry's full file path (under a random TempDir), so a
        // `contains("r2")` predicate spuriously fails whenever the tmp path
        // happens to contain "r2". Machine names don't collide with paths.
        .stdout(predicate::str::contains("m5-home"))
        .stdout(predicate::str::contains("m3-laptop").not());

    // Filter to nonexistent machine → no output.
    let mut cmd2 = darkmux_cmd();
    cmd2.env("DARKMUX_NOTEBOOK_DIR", nb_dir.to_str().unwrap())
        .arg("lab")
        .arg("notebook")
        .arg("list")
        .arg("--machine")
        .arg("nonexistent")
        .assert()
        .success()
        .stdout(predicate::str::contains("no notebook entries found"));
}

/// (#895) `lab notebook list` with an absent notebook dir exits 0 — "nothing
/// to list" is success (fresh user / chaining), not an error. (#1426 — the
/// notebook family folded into `lab`.)
#[test]
fn notebook_list_no_dir() {
    let mut cmd = darkmux_cmd();
    cmd.arg("lab")
        .arg("notebook")
        .arg("list")
        .env("DARKMUX_NOTEBOOK_DIR", "/no/such/path/xyz")
        .assert()
        .success()
        .stdout(predicate::str::contains("no notebook directory yet"));
}

/// (#1426) `external` retired entirely — the pipe is the interface (any text
/// on stdin into `mission propose`). The old top-level verb now fails with an
/// unknown-subcommand error (no compat alias — pre-2.0 clean removal).
#[test]
fn retired_top_level_external_verb_is_unknown() {
    let mut cmd = darkmux_cmd();
    cmd.arg("external")
        .arg("pull")
        .arg("--stdin")
        .assert()
        .failure()
        .stderr(predicate::str::contains("unrecognized subcommand").or(
            predicate::str::contains("unexpected argument"),
        ));
}

// ── mission migrate integration tests (#148 Task 8) ───────────────────────

fn write_flat_mission_file(root: &std::path::Path, id: &str) {
    let dir = root.join("missions");
    fs::create_dir_all(&dir).unwrap();
    let body = serde_json::json!({
        "id": id,
        "description": "test",
        "phase_ids": [],
        "created_ts": 1,
    });
    fs::write(
        dir.join(format!("{id}.json")),
        serde_json::to_string_pretty(&body).unwrap(),
    )
    .unwrap();
}

fn write_flat_phase_file(root: &std::path::Path, id: &str, mission_id: &str) {
    let dir = root.join("phases");
    fs::create_dir_all(&dir).unwrap();
    let body = serde_json::json!({
        "id": id,
        "mission_id": mission_id,
        "description": "test",
        "depends_on": [],
        "created_ts": 1,
    });
    fs::write(
        dir.join(format!("{id}.json")),
        serde_json::to_string_pretty(&body).unwrap(),
    )
    .unwrap();
}

/// Dry-run lists proposed moves but does NOT move files.
#[test]
fn mission_migrate_dry_run_shows_moves_without_moving() {
    let tmp = TempDir::new().unwrap();
    write_flat_mission_file(tmp.path(), "alpha");
    write_flat_phase_file(tmp.path(), "s1", "alpha");

    darkmux_cmd()
        .env("DARKMUX_CREW_DIR", tmp.path())
        .args(["mission", "migrate"])
        .assert()
        .success()
        .stdout(predicate::str::contains("alpha"))
        .stdout(predicate::str::contains("s1"))
        .stdout(predicate::str::contains("Re-run with --apply"));

    // Files must NOT have been moved.
    assert!(
        tmp.path().join("missions/alpha.json").is_file(),
        "dry-run must not move the flat mission file"
    );
    assert!(
        tmp.path().join("phases/s1.json").is_file(),
        "dry-run must not move the flat phase file"
    );
}

/// `--apply` actually moves files to the per-mission nested layout.
#[test]
fn mission_migrate_apply_moves_files() {
    let tmp = TempDir::new().unwrap();
    write_flat_mission_file(tmp.path(), "alpha");
    write_flat_phase_file(tmp.path(), "s1", "alpha");

    darkmux_cmd()
        .env("DARKMUX_CREW_DIR", tmp.path())
        .args(["mission", "migrate", "--apply"])
        .assert()
        .success()
        .stdout(predicate::str::contains("applied"));

    // New nested paths must exist.
    assert!(
        tmp.path().join("missions/alpha/mission.json").is_file(),
        "mission.json should be at nested path after --apply"
    );
    assert!(
        tmp.path().join("missions/alpha/phases/s1.json").is_file(),
        "phase json should be at nested path after --apply"
    );
    // Old flat paths must be gone.
    assert!(
        !tmp.path().join("missions/alpha.json").exists(),
        "flat mission file should be gone after --apply"
    );
    assert!(
        !tmp.path().join("phases/s1.json").exists(),
        "flat phase file should be gone after --apply"
    );
}

/// Re-running `--apply` after a successful migration is a no-op (idempotent).
#[test]
fn mission_migrate_apply_is_idempotent() {
    let tmp = TempDir::new().unwrap();
    write_flat_mission_file(tmp.path(), "alpha");
    write_flat_phase_file(tmp.path(), "s1", "alpha");

    // First apply.
    darkmux_cmd()
        .env("DARKMUX_CREW_DIR", tmp.path())
        .args(["mission", "migrate", "--apply"])
        .assert()
        .success();

    // Second apply: must succeed and report nothing to do.
    darkmux_cmd()
        .env("DARKMUX_CREW_DIR", tmp.path())
        .args(["mission", "migrate", "--apply"])
        .assert()
        .success()
        .stdout(predicate::str::contains("nothing to do"));
}

/// Phase-H: `notebook draft --role <id>` is the new flag (renamed
/// from `--agent` per Beat 36). The old `--agent` flag must NOT be
/// accepted — clap should reject it as an unknown argument so
/// operators with stale scripts get a loud failure instead of a
/// silent mis-dispatch.
#[test]
fn notebook_draft_rejects_old_agent_flag() {
    let tmp = TempDir::new().unwrap();
    let mut cmd = darkmux_cmd_in_project(tmp.path());
    let output = cmd
        .args([
            "lab",
            "notebook",
            "draft",
            "nonexistent",
            "--agent",
            "main",
            "--dry-run",
        ])
        .output()
        .unwrap();
    assert!(
        !output.status.success(),
        "expected --agent to be rejected by clap; got success: stdout={:?} stderr={:?}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    // QA NIT 3: tighten to "unexpected argument" specifically — `agent`
    // alone could appear in clap's help suggestion text and false-pass.
    assert!(
        stderr.contains("unexpected argument"),
        "expected clap to flag `--agent` as unexpected argument; got: {stderr}"
    );
}

/// Phase-H: `notebook draft --role <id>` accepts the new flag and
/// proceeds. Uses --dry-run + an absolute manifest path so we don't
/// need a real dispatch.
#[test]
fn notebook_draft_accepts_role_flag_under_dry_run() {
    let tmp = TempDir::new().unwrap();
    let darkmux = tmp.path().join(".darkmux");
    let runs_dir = darkmux.join("runs/test-run-h");
    fs::create_dir_all(&runs_dir).unwrap();
    fs::write(
        runs_dir.join("manifest.json"),
        r#"{"workload":"quick-q","provider":"prompt","profile":"scribe","session_id":"s","duration_ms":5000,"ok":true}"#,
    )
    .unwrap();

    let mut cmd = darkmux_cmd_in_project(tmp.path());
    cmd.env("DARKMUX_NOTEBOOK_DIR", darkmux.join("notebook").to_str().unwrap());
    cmd.args([
        "lab",
        "notebook",
        "draft",
        "test-run-h",
        "--role",
        "scribe",
        "--dry-run",
        "--slug",
        "phase-h-test",
    ])
    .assert()
    .success()
    .stdout(predicate::str::contains("phase-h-test"));
}

// ─── (#491) Phase 4 lab CLI verbs: register / unregister / fixtures / doctor ──

/// Operator runs `dm lab fixture register <path>` against a fixture dir with
/// a valid `.fixture.json`. Registry file is created at
/// `{paths.root}/lab-registry.json` with one entry.
#[test]
fn lab_register_creates_registry_entry() {
    let tmp = TempDir::new().unwrap();
    // Create the fixture dir + manifest.
    let fixture_dir = tmp.path().join("my-fixture");
    fs::create_dir_all(&fixture_dir).unwrap();
    fs::write(
        fixture_dir.join(".fixture.json"),
        r#"{"name": "demo", "satisfies": "tiny@1.0"}"#,
    )
    .unwrap();
    fs::write(fixture_dir.join("a.txt"), "alpha").unwrap();

    // Force project-scope so registry lands in tmp.
    fs::create_dir_all(tmp.path().join(".darkmux")).unwrap();

    let mut cmd = darkmux_cmd_in_project(tmp.path());
    cmd.args(["lab", "fixture", "register", fixture_dir.to_str().unwrap()])
        .assert()
        .success()
        .stdout(predicate::str::contains("Registered fixture `demo`"));

    let reg_path = tmp.path().join(".darkmux/lab-registry.json");
    assert!(reg_path.exists(), "registry should exist at {}", reg_path.display());
    let raw = fs::read_to_string(&reg_path).unwrap();
    assert!(raw.contains("\"demo\""));
    assert!(raw.contains("\"tiny@1.0\""));
    assert!(raw.contains("\"content_hash\""));
}

/// `dm lab fixtures` shows the registered entry after a register.
#[test]
fn lab_fixtures_shows_registered_entries() {
    let tmp = TempDir::new().unwrap();
    let fixture_dir = tmp.path().join("my-fixture");
    fs::create_dir_all(&fixture_dir).unwrap();
    fs::write(fixture_dir.join(".fixture.json"), r#"{"name": "demo"}"#).unwrap();
    fs::write(fixture_dir.join("a.txt"), "x").unwrap();
    fs::create_dir_all(tmp.path().join(".darkmux")).unwrap();

    // Register first.
    darkmux_cmd_in_project(tmp.path())
        .args(["lab", "fixture", "register", fixture_dir.to_str().unwrap()])
        .assert()
        .success();

    // Now list.
    darkmux_cmd_in_project(tmp.path())
        .args(["lab", "fixture", "list"])
        .assert()
        .success()
        .stdout(predicate::str::contains("demo"))
        .stdout(predicate::str::contains("1 fixture"));
}

/// `dm lab unregister` removes the entry without touching the dir.
#[test]
fn lab_unregister_removes_entry_but_not_dir() {
    let tmp = TempDir::new().unwrap();
    let fixture_dir = tmp.path().join("my-fixture");
    fs::create_dir_all(&fixture_dir).unwrap();
    fs::write(fixture_dir.join(".fixture.json"), r#"{"name": "demo"}"#).unwrap();
    fs::write(fixture_dir.join("a.txt"), "x").unwrap();
    fs::create_dir_all(tmp.path().join(".darkmux")).unwrap();

    darkmux_cmd_in_project(tmp.path())
        .args(["lab", "fixture", "register", fixture_dir.to_str().unwrap()])
        .assert()
        .success();

    darkmux_cmd_in_project(tmp.path())
        .args(["lab", "fixture", "unregister", "demo"])
        .assert()
        .success()
        .stdout(predicate::str::contains("Unregistered"));

    // Dir still on disk (operator-sovereignty).
    assert!(fixture_dir.join(".fixture.json").exists());
    // Registry no longer has the entry.
    let raw = fs::read_to_string(tmp.path().join(".darkmux/lab-registry.json")).unwrap();
    assert!(!raw.contains("\"demo\""));
}

/// (#2613) A fixture registered from one directory must be visible from
/// ANOTHER directory — including one that has its own project-local
/// `.darkmux/`, which is exactly the state that made them diverge. Unlike
/// every other fixture test in this file, this one deliberately does NOT
/// go through `darkmux_cmd_in_project` (which pins the root via an
/// explicit `DARKMUX_HOME` override — the override wins identically
/// whether the registry resolves `Auto` or `ForceUser`, so it can't
/// exercise this divergence at all). Instead: `DARKMUX_HOME` is removed
/// entirely and only `HOME` is isolated, so `paths::resolve` has to make
/// its own Auto-vs-ForceUser decision from the real cwd.
#[test]
fn fixture_registered_from_one_cwd_is_visible_from_a_project_local_cwd() {
    let fake_home = TempDir::new().unwrap();
    let plain_dir = TempDir::new().unwrap();
    let project_dir = TempDir::new().unwrap();
    // A GENUINE project-local `.darkmux/` — pre-existing, not created by
    // this test's own commands — is what makes `Auto` resolution diverge
    // from the home tier.
    fs::create_dir_all(project_dir.path().join(".darkmux")).unwrap();

    let fixture_dir = plain_dir.path().join("my-fixture");
    fs::create_dir_all(&fixture_dir).unwrap();
    fs::write(
        fixture_dir.join(".fixture.json"),
        r#"{"name": "demo", "satisfies": "tiny@1.0"}"#,
    )
    .unwrap();
    fs::write(fixture_dir.join("a.txt"), "x").unwrap();

    let home_registry = fake_home.path().join(".darkmux").join("lab-registry.json");

    // Register from a PLAIN directory (no project-local `.darkmux/`
    // anywhere in cwd) — lands at the home tier.
    darkmux_cmd()
        .env_remove("DARKMUX_HOME")
        .env("HOME", fake_home.path())
        .current_dir(plain_dir.path())
        .args(["lab", "fixture", "register", fixture_dir.to_str().unwrap()])
        .assert()
        .success()
        .stdout(predicate::str::contains("Registered fixture `demo`"))
        .stdout(predicate::str::contains(home_registry.display().to_string()));
    assert!(home_registry.exists(), "registry should exist at {}", home_registry.display());

    // `lab fixture list` from the PROJECT directory — its own `.darkmux/`
    // exists — must still show the fixture, from the SAME home-rooted
    // registry, not a fresh empty one.
    darkmux_cmd()
        .env_remove("DARKMUX_HOME")
        .env("HOME", fake_home.path())
        .current_dir(project_dir.path())
        .args(["lab", "fixture", "list"])
        .assert()
        .success()
        .stdout(predicate::str::contains("demo"))
        .stdout(predicate::str::contains("1 fixture"))
        .stdout(predicate::str::contains(home_registry.display().to_string()));

    // The project-local root never received a registry of its own.
    assert!(
        !project_dir.path().join(".darkmux").join("lab-registry.json").exists(),
        "the project-local .darkmux/ must NOT get its own registry — the fixture registry is \
         always home-tier"
    );

    // `lab doctor` from the project directory agrees: it's looking at the
    // SAME populated registry, not reporting an empty/missing one.
    darkmux_cmd()
        .env_remove("DARKMUX_HOME")
        .env("HOME", fake_home.path())
        .current_dir(project_dir.path())
        .args(["lab", "doctor"])
        .assert()
        .success()
        .stdout(predicate::str::contains("1 fixture"));

    // (#2613 MUST FIX 1) Register a SECOND fixture with cwd ITSELF set to
    // the project directory (its own `.darkmux/` exists) — this is the
    // leg the read-side assertions above cannot cover. `plain_dir` has no
    // project-local `.darkmux/` anywhere in cwd, so for the FIRST register
    // above `Auto` and `ForceUser` resolve identically and a `cmd_register`
    // reverted back to `Auto` would still pass every assertion in this
    // test up to this point. Only a register issued FROM a directory whose
    // own `.darkmux/` exists can catch that regression.
    let fixture_dir_2 = plain_dir.path().join("my-fixture-2");
    fs::create_dir_all(&fixture_dir_2).unwrap();
    fs::write(
        fixture_dir_2.join(".fixture.json"),
        r#"{"name": "demo2", "satisfies": "tiny@2.0"}"#,
    )
    .unwrap();
    fs::write(fixture_dir_2.join("a.txt"), "x").unwrap();

    darkmux_cmd()
        .env_remove("DARKMUX_HOME")
        .env("HOME", fake_home.path())
        .current_dir(project_dir.path())
        .args(["lab", "fixture", "register", fixture_dir_2.to_str().unwrap()])
        .assert()
        .success()
        .stdout(predicate::str::contains("Registered fixture `demo2`"))
        .stdout(predicate::str::contains(home_registry.display().to_string()));

    assert!(
        !project_dir.path().join(".darkmux").join("lab-registry.json").exists(),
        "registering FROM a project-local cwd must NOT create a project-local registry — \
         cmd_register itself must resolve to the home tier, at {}",
        project_dir.path().join(".darkmux").join("lab-registry.json").display()
    );

    // And it actually RESOLVES from elsewhere — not just fails to create
    // a stray project-local file.
    darkmux_cmd()
        .env_remove("DARKMUX_HOME")
        .env("HOME", fake_home.path())
        .current_dir(plain_dir.path())
        .args(["lab", "fixture", "list"])
        .assert()
        .success()
        .stdout(predicate::str::contains("demo2"))
        .stdout(predicate::str::contains("2 fixtures"));

    // And unregistering from the project directory removes it from the
    // SAME home-tier registry a later `list` from the plain directory
    // would also read.
    darkmux_cmd()
        .env_remove("DARKMUX_HOME")
        .env("HOME", fake_home.path())
        .current_dir(project_dir.path())
        .args(["lab", "fixture", "unregister", "demo"])
        .assert()
        .success()
        .stdout(predicate::str::contains("Unregistered"))
        .stdout(predicate::str::contains(home_registry.display().to_string()));
    let raw = fs::read_to_string(&home_registry).unwrap();
    assert!(!raw.contains("\"demo\""), "unregister from the project cwd must remove the entry \
        from the home-tier registry: {raw}");
}

/// (#2613 CONSIDER 3) `lab fixture list` and `lab doctor`, run from a
/// directory whose OWN `.darkmux/lab-registry.json` still holds a
/// populated registry (left behind by a pre-#2613 build, or hand-crafted),
/// must say so — not just report "no registry" / empty at the home tier
/// and go silent about the populated file sitting right there in cwd. The
/// file itself is never touched (operator-sovereignty); this only tests
/// the DISCLOSURE half.
#[test]
fn lab_list_and_doctor_signpost_an_orphaned_project_local_registry() {
    let fake_home = TempDir::new().unwrap();
    let project_dir = TempDir::new().unwrap();
    let project_darkmux = project_dir.path().join(".darkmux");
    fs::create_dir_all(&project_darkmux).unwrap();

    let fixture_dir = project_dir.path().join("orphaned-fixture");
    fs::create_dir_all(&fixture_dir).unwrap();
    fs::write(
        fixture_dir.join(".fixture.json"),
        r#"{"name": "orphan", "satisfies": "tiny@1.0"}"#,
    )
    .unwrap();
    fs::write(fixture_dir.join("a.txt"), "x").unwrap();

    // Populate `project_dir/.darkmux/lab-registry.json` DIRECTLY — pointing
    // `DARKMUX_HOME` straight at it — mimicking a registry that predates
    // (or was hand-written outside) this fix, sitting in a directory's own
    // `.darkmux/` rather than the home tier.
    darkmux_cmd()
        .env("DARKMUX_HOME", &project_darkmux)
        .current_dir(project_dir.path())
        .args(["lab", "fixture", "register", fixture_dir.to_str().unwrap()])
        .assert()
        .success();
    let project_registry = project_darkmux.join("lab-registry.json");
    assert!(project_registry.exists());

    let expected_home_registry = fake_home.path().join(".darkmux").join("lab-registry.json");

    // `lab fixture list` from that SAME directory, but with the real home
    // tier empty (no registry there at all) — must name the orphaned
    // project-local file, not just report "no registry" and stop.
    darkmux_cmd()
        .env_remove("DARKMUX_HOME")
        .env("HOME", fake_home.path())
        .current_dir(project_dir.path())
        .args(["lab", "fixture", "list"])
        .assert()
        .success()
        .stdout(predicate::str::contains(format!(
            "No registry at {}",
            expected_home_registry.display()
        )))
        .stdout(predicate::str::contains(project_registry.display().to_string()))
        .stdout(predicate::str::contains("never consulted"));

    // `lab doctor` from the same directory agrees.
    darkmux_cmd()
        .env_remove("DARKMUX_HOME")
        .env("HOME", fake_home.path())
        .current_dir(project_dir.path())
        .args(["lab", "doctor"])
        .assert()
        .stdout(predicate::str::contains(project_registry.display().to_string()))
        .stdout(predicate::str::contains("never consulted"));

    // The orphaned file itself was never touched by either read.
    let raw = fs::read_to_string(&project_registry).unwrap();
    assert!(raw.contains("\"orphan\""), "orphaned registry content must survive untouched: {raw}");
}

/// `dm lab doctor` with no registry exits non-zero + emits a warning
/// with the three options for the operator.
#[test]
fn lab_doctor_warns_when_no_registry() {
    let tmp = TempDir::new().unwrap();
    fs::create_dir_all(tmp.path().join(".darkmux")).unwrap();

    let output = darkmux_cmd_in_project(tmp.path())
        .args(["lab", "doctor"])
        .output()
        .unwrap();
    assert!(!output.status.success(), "doctor should exit non-zero on warnings");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("no registry found"), "got: {stdout}");
    assert!(stdout.contains("lab-init.sh") || stdout.contains("dm lab fixture register"), "got: {stdout}");
}

/// `dm lab doctor` passes when a registered fixture is unchanged.
#[test]
fn lab_doctor_passes_for_clean_fixture() {
    let tmp = TempDir::new().unwrap();
    let fixture_dir = tmp.path().join("my-fixture");
    fs::create_dir_all(&fixture_dir).unwrap();
    fs::write(fixture_dir.join(".fixture.json"), r#"{"name": "demo"}"#).unwrap();
    fs::write(fixture_dir.join("source.txt"), "baseline").unwrap();
    fs::create_dir_all(tmp.path().join(".darkmux")).unwrap();

    darkmux_cmd_in_project(tmp.path())
        .args(["lab", "fixture", "register", fixture_dir.to_str().unwrap()])
        .assert()
        .success();

    darkmux_cmd_in_project(tmp.path())
        .args(["lab", "doctor"])
        .assert()
        .success()
        .stdout(predicate::str::contains("[ok]"))
        .stdout(predicate::str::contains("demo"));
}

/// `dm lab doctor` warns + exits non-zero when a registered fixture's
/// content has drifted (hash mismatch).
#[test]
fn lab_doctor_warns_on_hash_drift() {
    let tmp = TempDir::new().unwrap();
    let fixture_dir = tmp.path().join("my-fixture");
    fs::create_dir_all(&fixture_dir).unwrap();
    fs::write(fixture_dir.join(".fixture.json"), r#"{"name": "demo"}"#).unwrap();
    fs::write(fixture_dir.join("source.txt"), "baseline").unwrap();
    fs::create_dir_all(tmp.path().join(".darkmux")).unwrap();

    darkmux_cmd_in_project(tmp.path())
        .args(["lab", "fixture", "register", fixture_dir.to_str().unwrap()])
        .assert()
        .success();

    // Mutate the fixture → drift.
    fs::write(fixture_dir.join("source.txt"), "MUTATED").unwrap();

    let output = darkmux_cmd_in_project(tmp.path())
        .args(["lab", "doctor"])
        .output()
        .unwrap();
    assert!(!output.status.success(), "drift should exit non-zero");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("content drift"), "got: {stdout}");
    assert!(
        stdout.contains("dm lab fixture register --force"),
        "expected recovery hint: {stdout}"
    );
}

// ─── (#492) Phase 5: built-in fixture + lab-init.sh + demo-quickstart workload ──

/// The built-in `demo-tiny-py` fixture ships with a valid
/// `.fixture.json` that registers successfully.
#[test]
fn lab_register_builtin_demo_tiny_py_succeeds() {
    let tmp = TempDir::new().unwrap();
    fs::create_dir_all(tmp.path().join(".darkmux")).unwrap();

    // Resolve the in-tree built-in fixture path from CARGO_MANIFEST_DIR.
    let repo_root = std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR set by cargo");
    let fixture_path = format!(
        "{}/templates/builtin/lab-fixtures/demo-tiny-py",
        repo_root
    );

    darkmux_cmd_in_project(tmp.path())
        .args(["lab", "fixture", "register", &fixture_path])
        .assert()
        .success()
        .stdout(predicate::str::contains("Registered fixture `demo-tiny-py`"))
        .stdout(predicate::str::contains("tiny-python-suite@1.0"));
}

/// `darkmux lab doctor` passes against the freshly-registered
/// Recursively copy `src` → `dst`, skipping run-artifact dirs (the
/// `crates/darkmux-lab` `RUN_ARTIFACT_DIRS` set). (#613) A dev machine that has
/// run a dispatch against the in-repo builtin fixture leaves `__pycache__/` /
/// `coverage/` / `.darkmux-runtime/` under it; registering that raw source
/// would trip `lab doctor`'s cleanliness check (warn → exit 1) and fail the
/// test below locally, though CI (fresh checkout) stays green. Registering a
/// pruned copy gives the test the same isolation the real lab flow gets from
/// its COW clone (#609), so the result no longer depends on dev-machine cruft.
fn copy_pruned(src: &std::path::Path, dst: &std::path::Path) {
    const PRUNE: &[&str] = &[
        ".darkmux-runtime",
        ".darkmux-agent",
        "coverage",
        ".coverage",
        "target",
        "__pycache__",
        ".git",
    ];
    fs::create_dir_all(dst).unwrap();
    for entry in fs::read_dir(src).unwrap() {
        let entry = entry.unwrap();
        let name = entry.file_name();
        if entry.file_type().unwrap().is_dir() {
            if PRUNE.contains(&name.to_string_lossy().as_ref()) {
                continue;
            }
            copy_pruned(&entry.path(), &dst.join(&name));
        } else {
            fs::copy(entry.path(), dst.join(&name)).unwrap();
        }
    }
}

/// `demo-tiny-py` built-in — schema check, required_files present,
/// hash matches. Registers a pruned copy (not the raw in-repo source) so
/// dev-machine artifact cruft can't trip the cleanliness check (#613).
#[test]
fn lab_doctor_passes_for_builtin_demo_tiny_py() {
    let tmp = TempDir::new().unwrap();
    fs::create_dir_all(tmp.path().join(".darkmux")).unwrap();
    let repo_root = std::env::var("CARGO_MANIFEST_DIR").unwrap();
    let fixture_src = format!(
        "{}/templates/builtin/lab-fixtures/demo-tiny-py",
        repo_root
    );
    // Copy to an isolated, artifact-pruned location and register THAT, so the
    // test is hermetic regardless of cruft under the in-repo fixture (#613).
    let fixture_dir = tmp.path().join("demo-tiny-py");
    copy_pruned(std::path::Path::new(&fixture_src), &fixture_dir);
    let fixture_path = fixture_dir.to_string_lossy().to_string();
    darkmux_cmd_in_project(tmp.path())
        .args(["lab", "fixture", "register", &fixture_path])
        .assert()
        .success();

    darkmux_cmd_in_project(tmp.path())
        .args(["lab", "doctor"])
        .assert()
        .success()
        .stdout(predicate::str::contains("demo-tiny-py"))
        .stdout(predicate::str::contains("1 pass"));
}

/// (#1426 / #386) `darkmux dispatch <role> [MESSAGE]` sources the message
/// from a positional argument, `--message-from-file`, or stdin. The positional
/// and the file flag are mutually exclusive, and the file is resolved at the
/// top of the handler — all before any container work, so these need no
/// docker / model.
#[test]
fn dispatch_message_source_contract() {
    // Mutual exclusion: positional MESSAGE AND --message-from-file → clap
    // rejects. (Proves the positional exists and conflicts with the file flag.)
    darkmux_cmd()
        .args([
            "dispatch",
            "code-reviewer",
            "inline",
            "--message-from-file",
            "/tmp/whatever",
        ])
        .assert()
        .failure()
        .stderr(predicate::str::contains("cannot be used with"));

    // Missing --message-from-file → resolved early, fails loud BEFORE any
    // dispatch setup (the message is resolved at the top of the handler, ahead
    // of out-dir creation / container spawn).
    darkmux_cmd()
        .args([
            "dispatch",
            "code-reviewer",
            "--message-from-file",
            "/nonexistent/darkmux-1426/brief.md",
        ])
        .assert()
        .failure()
        .stderr(
            predicate::str::contains("message-from-file")
                .and(predicate::str::contains("out-dir").not())
                .and(predicate::str::contains("spawning").not()),
        );
}

/// (#2265) `dispatch --finding <key>` refuses a key that addresses no stored
/// finding, BEFORE any dispatch setup — a silently missing brief would send
/// the role to work on an observation it never saw. The refusal names the
/// second producer that can fill the store.
#[test]
fn dispatch_finding_refuses_a_key_with_no_stored_record() {
    let store = TempDir::new().unwrap(); // empty store
    darkmux_cmd()
        .env("DARKMUX_FINDINGS_DIR", store.path())
        .args(["dispatch", "health-research", "--finding", "sess-x/9", "smoke"])
        .assert()
        .failure()
        .stderr(
            predicate::str::contains("no finding sess-x/9")
                .and(predicate::str::contains("darkmux finding sync"))
                // Refused ahead of the dispatch: the ACK gate never ran, and
                // nothing reached docker.
                .and(predicate::str::contains("requires operator acknowledgment").not())
                .and(predicate::str::contains("docker").not()),
        );

    // A key of the wrong SHAPE is refused with the form it should have.
    darkmux_cmd()
        .env("DARKMUX_FINDINGS_DIR", store.path())
        .args(["dispatch", "health-research", "--finding", "not-a-key", "smoke"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("<dispatch>/<seq>"));
}

/// (#2295) The same refusal rule for the second record kind: `dispatch --mod
/// <key>` with a key that addresses no stored mod refuses BEFORE any dispatch
/// setup. Proven by ABSENCE — the ACK gate this role would otherwise hit, and
/// any docker work, must both be unreached.
#[test]
fn dispatch_mod_refuses_a_key_with_no_stored_record() {
    let store = TempDir::new().unwrap(); // empty store
    darkmux_cmd()
        .env("DARKMUX_MODS_DIR", store.path())
        .args(["dispatch", "health-research", "--mod", "mod-1-nope", "smoke"])
        .assert()
        .failure()
        .stderr(
            predicate::str::contains("no mod mod-1-nope")
                .and(predicate::str::contains("darkmux mod list"))
                .and(predicate::str::contains("requires operator acknowledgment").not())
                .and(predicate::str::contains("docker").not()),
        );

    // A key that could escape the store is refused as a key, never read.
    darkmux_cmd()
        .env("DARKMUX_MODS_DIR", store.path())
        .args(["dispatch", "health-research", "--mod", "../etc", "smoke"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("is not a mod key"));
}

/// (#2265) A key that DOES address a stored finding is loaded and the dispatch
/// proceeds — proven at the ACK gate, which bails before any Docker work.
/// `--finding` is repeatable, and both keys resolve.
#[test]
fn dispatch_finding_loads_a_stored_record_and_proceeds() {
    let store = TempDir::new().unwrap();
    for (dispatch, seq) in [("sess-x", 1u64), ("sess-y", 2)] {
        let dir = store.path().join(dispatch).join(seq.to_string());
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("finding.json"),
            serde_json::json!({
                "key": format!("{dispatch}/{seq}"),
                "dispatch": dispatch,
                "seq": seq,
                "ts": "2026-09-03T00:00:00Z",
                "tool_name": "create_finding",
                "proposer": {"handle": "crawler", "model": "m"},
                "context": {"unit": "u7"},
                "emitted": {"file": "src/x.ts", "line": 82, "why": "three unnamed operands"},
                "schema_version": "1"
            })
            .to_string(),
        )
        .unwrap();
    }
    let ack_dir = TempDir::new().unwrap();
    darkmux_cmd()
        .env("DARKMUX_FINDINGS_DIR", store.path())
        .env("DARKMUX_ACK_DIR", ack_dir.path())
        .args([
            "dispatch", "health-research", "--finding", "sess-x/1", "--finding", "sess-y/2",
            "smoke",
        ])
        .assert()
        .failure()
        .stderr(
            predicate::str::contains("requires operator acknowledgment")
                .and(predicate::str::contains("no finding").not()),
        );
}

/// (#2265 review, IMPORTANT 4 + CRITICAL 8) The END-TO-END pin for
/// `--finding`, through the path a real `darkmux dispatch` takes: the CLI ->
/// the crew-of-one graph (`dispatch_as_crew_of_one::build_graph`) -> the step
/// kind -> `dispatch_internal`, with the assertions made on the `dispatch
/// start` FLOW RECORD that dispatch actually wrote.
///
/// Both halves matter and neither was covered before. The earlier CLI tests
/// asserted only that a good key reaches the ACK gate and a bad one is refused
/// — nothing about the brief or the record — so the append could be deleted
/// and they stayed green, and the crew-of-one graph could drop the keys (it
/// did) with nothing to catch it. Here: `prompt_chars` must exceed the
/// operator's own message, proving the finding block was appended, and
/// `brief_refs` must name the records, proving the hand-off survived every
/// hop.
///
/// (#2295) Extended to BOTH record kinds in one dispatch: a finding and a mod,
/// in the order given, with the mod's attachment named by the container path
/// it is mounted at.
#[test]
fn dispatch_finding_reaches_the_flow_record_with_the_brief_and_the_keys() {
    let stub = RespondingStubServer::start();
    let home = TempDir::new().unwrap();
    let flows = TempDir::new().unwrap();
    let findings = TempDir::new().unwrap();
    let mods = TempDir::new().unwrap();

    let profiles_path = home.path().join("profiles.json");
    fs::write(&profiles_path, responding_endpoint_profiles_json(stub.port)).unwrap();

    // A finding in the store, with a distinctive marker the brief must carry.
    let dir = findings.path().join("sess-pin").join("4");
    fs::create_dir_all(&dir).unwrap();
    fs::write(
        dir.join("finding.json"),
        serde_json::json!({
            "key": "sess-pin/4", "dispatch": "sess-pin", "seq": 4,
            "ts": "2026-09-03T00:00:00Z", "tool_name": "create_finding",
            "proposer": {"handle": "crawler", "model": "m"},
            "context": {"unit": "u7"},
            "emitted": {"file": "src/x.ts", "line": 82, "why": "MARKER-three-unnamed-operands"},
            "schema_version": "1"
        })
        .to_string(),
    )
    .unwrap();

    // A mod in the store, with its own distinctive kit and one attachment.
    let mod_dir = mods.path().join("mod-9-pin");
    fs::create_dir_all(mod_dir.join("attachments")).unwrap();
    fs::write(mod_dir.join("attachments").join("fix.patch"), b"body").unwrap();
    fs::write(
        mod_dir.join("mod.json"),
        serde_json::json!({
            "key": "mod-9-pin", "ts": "2026-09-04T00:00:00Z", "by": "sonnet",
            "for": ["sess-pin/4"],
            "kit": "MARKER-name-the-three-operands",
            "kit_looks_json": false,
            "attachments": ["fix.patch"],
            "context": {"findings": []},
            "schema_version": "1"
        })
        .to_string(),
    )
    .unwrap();

    let message = "fix it";
    // `dialectic-judge` is TOOL-LESS, so this takes the light single-shot
    // hosted path (a host `curl` to the stub) rather than a
    // `darkmux-runtime` container — no Docker, no image, no model.
    darkmux_cmd()
        .env("DARKMUX_PROFILES", &profiles_path)
        .env("DARKMUX_FLOWS_DIR", flows.path())
        .env("DARKMUX_FINDINGS_DIR", findings.path())
        .env("DARKMUX_MODS_DIR", mods.path())
        .env("DARKMUX_REDIS_URL", "")
        .args([
            "dispatch", "dialectic-judge", "--finding", "sess-pin/4", "--mod", "mod-9-pin",
            "--skip-preflight", message,
        ])
        .assert()
        .success();

    let mut start: Option<serde_json::Value> = None;
    for entry in fs::read_dir(flows.path()).unwrap() {
        let path = entry.unwrap().path();
        if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
            continue;
        }
        for line in fs::read_to_string(&path).unwrap().lines() {
            let Ok(rec) = serde_json::from_str::<serde_json::Value>(line) else { continue };
            if rec["action"] == "dispatch start" {
                start = Some(rec);
            }
        }
    }
    let start = start.expect("a `dispatch start` flow record");
    let payload = &start["payload"];
    assert_eq!(
        payload["brief_refs"],
        serde_json::json!([
            {"kind": "finding", "key": "sess-pin/4"},
            {"kind": "mod", "key": "mod-9-pin"},
        ]),
        "both refs must survive the CLI -> crew-of-one -> step-kind hand-off, \
         in the order given: {start}"
    );
    let prompt_chars = payload["prompt_chars"].as_u64().expect("prompt_chars");
    assert!(
        prompt_chars > message.chars().count() as u64 + 200,
        "the finding block must be IN the brief (prompt_chars {prompt_chars} is barely \
         longer than the operator's own {} chars): {start}",
        message.chars().count()
    );
    let prompt = payload["prompt"].as_str().unwrap_or_default();
    assert!(
        prompt.contains("MARKER-three-unnamed-operands"),
        "the record's own prompt carries the finding's emission verbatim: {start}"
    );
    assert!(
        prompt.contains("MARKER-name-the-three-operands"),
        "and the mod's kit, byte-exact: {start}"
    );
    assert!(
        prompt.find("MARKER-three-unnamed-operands") < prompt.find("MARKER-name-the-three-operands"),
        "the blocks follow the order the refs were given: {start}"
    );
    assert!(
        prompt.contains("/darkmux-mods/mod-9-pin/attachments/fix.patch"),
        "the mod block names its attachment by the container path it is mounted at: {start}"
    );
    // (#2295 review, CRITICAL 1) EXACTLY once. Resolution moved from the CLI
    // down to the step kind so a mission graph gets its blocks too; if the CLI
    // kept appending as well, a `darkmux dispatch --finding` would send the
    // model the same record twice and nothing above would notice.
    assert_eq!(
        prompt.matches("MARKER-three-unnamed-operands").count(),
        1,
        "the finding block is appended exactly once: {start}"
    );
    assert_eq!(
        prompt.matches("MARKER-name-the-three-operands").count(),
        1,
        "and the mod block exactly once: {start}"
    );
    assert_eq!(
        prompt.matches("<mod key=\"mod-9-pin\">").count(),
        1,
        "one mod block, not two: {start}"
    );
}

/// (#2295 review, CRITICAL 1) The refs cannot ride the fleet work queue — its
/// job shape has no field for them and the peer's stores are its own — so a
/// remote `--machine` dispatch that names one is refused rather than routed
/// with its blocks silently missing. Refused BEFORE the ack gate, like every
/// other brief-ref refusal.
#[test]
fn dispatch_refuses_a_record_ref_routed_to_another_machine() {
    let store = TempDir::new().unwrap();
    let dir = store.path().join("sess-r").join("1");
    fs::create_dir_all(&dir).unwrap();
    fs::write(
        dir.join("finding.json"),
        serde_json::json!({
            "key": "sess-r/1", "dispatch": "sess-r", "seq": 1,
            "ts": "2026-09-04T00:00:00Z", "tool_name": "create_finding",
            "proposer": {"handle": "h", "model": "m"},
            "context": {}, "emitted": {"why": "x"}, "schema_version": "1"
        })
        .to_string(),
    )
    .unwrap();

    darkmux_cmd()
        .env("DARKMUX_FINDINGS_DIR", store.path())
        .env("DARKMUX_MACHINE_ID", "this-one")
        .args([
            "dispatch", "health-research", "--finding", "sess-r/1", "--machine", "some-other-mac",
            "smoke",
        ])
        .assert()
        .failure()
        .stderr(
            predicate::str::contains("cannot be routed to another machine")
                .and(predicate::str::contains("requires operator acknowledgment").not()),
        );
}

/// (#1426) The POSITIONAL message reaches the dispatch path. `health-research`
/// is licensed-adjacent, so its ACK gate bails BEFORE any Docker work — a
/// CI-safe way to prove the positional message was accepted and routed without
/// a real model.
#[test]
fn dispatch_positional_message_reaches_ack_gate() {
    let ack_dir = TempDir::new().unwrap(); // empty — no prior ack on file
    darkmux_cmd()
        .env("DARKMUX_ACK_DIR", ack_dir.path())
        .args(["dispatch", "health-research", "smoke"])
        .assert()
        .failure()
        .stderr(
            predicate::str::contains("requires operator acknowledgment")
                // The positional was consumed — no "no message given" guard,
                // no docker.
                .and(predicate::str::contains("no message given").not())
                .and(predicate::str::contains("runtime=internal").not())
                .and(predicate::str::contains("docker").not()),
        );
}

/// (#1426) When the positional MESSAGE is omitted, the message is read from
/// stdin (pipe composition: `git diff | darkmux dispatch pr-reviewer`). Piping
/// a message to `health-research` proves the stdin channel drives the message
/// (no TTY-absent error fires) and the dispatch reaches the ACK gate, which
/// bails before Docker. CI-safe: `write_stdin` makes stdin a non-TTY pipe, the
/// path the byte-faithful `read_to_string` consumes.
#[test]
fn dispatch_stdin_message_reaches_ack_gate() {
    let ack_dir = TempDir::new().unwrap();
    darkmux_cmd()
        .env("DARKMUX_ACK_DIR", ack_dir.path())
        .args(["dispatch", "health-research"])
        .write_stdin("smoke from stdin")
        .assert()
        .failure()
        .stderr(
            predicate::str::contains("requires operator acknowledgment")
                // The TTY-absent guard did NOT fire (stdin was a pipe with
                // content), and no docker work happened.
                .and(predicate::str::contains("no message given").not())
                .and(predicate::str::contains("runtime=internal").not())
                .and(predicate::str::contains("docker").not()),
        );
}

/// (#1426) Empty piped stdin bails LOUDLY with its own error — distinct from
/// the terminal-guard's "no message given" text — instead of dispatching a
/// blank brief (an empty `git diff |` is the most common accident). The
/// dispatch never starts: no ACK-gate text, no docker.
#[test]
fn dispatch_empty_stdin_bails_loudly() {
    let ack_dir = TempDir::new().unwrap();
    darkmux_cmd()
        .env("DARKMUX_ACK_DIR", ack_dir.path())
        .args(["dispatch", "health-research"])
        .write_stdin("") // empty pipe → loud bail, not a blank dispatch
        .assert()
        .failure()
        .stderr(
            predicate::str::contains("stdin was empty")
                // Distinct from the TTY-absent guard's error.
                .and(predicate::str::contains("no message given").not())
                // Bailed before any dispatch machinery.
                .and(predicate::str::contains("requires operator acknowledgment").not())
                .and(predicate::str::contains("docker").not()),
        );
}

/// (#1426) A whitespace-only pipe (`echo |` produces a lone "\n" — the second
/// most common accident) gets the same loud empty-stdin bail. The emptiness
/// check trims for the CHECK only; a message with real content is still
/// delivered byte-faithfully (covered by dispatch_stdin_message_reaches_ack_gate).
#[test]
fn dispatch_whitespace_only_stdin_bails_loudly() {
    let ack_dir = TempDir::new().unwrap();
    darkmux_cmd()
        .env("DARKMUX_ACK_DIR", ack_dir.path())
        .args(["dispatch", "health-research"])
        .write_stdin("\n")
        .assert()
        .failure()
        .stderr(
            predicate::str::contains("stdin was empty")
                .and(predicate::str::contains("requires operator acknowledgment").not()),
        );
}

/// (#1426) An empty (or whitespace-only) --message-from-file gets the same
/// trim-empty bail for consistency, with a distinct error naming the file
/// path — resolved at the top of the handler, before any dispatch setup.
#[test]
fn dispatch_empty_message_file_bails_loudly() {
    let tmp = TempDir::new().unwrap();
    let brief = tmp.path().join("blank-brief.md");
    fs::write(&brief, "  \n\n").unwrap();
    darkmux_cmd()
        .args([
            "dispatch",
            "code-reviewer",
            "--message-from-file",
            brief.to_str().unwrap(),
        ])
        .assert()
        .failure()
        .stderr(
            predicate::str::contains("is empty")
                .and(predicate::str::contains("blank-brief.md"))
                .and(predicate::str::contains("docker").not()),
        );
}

/// (#1405 gate remediation, relocated to top-level `dispatch` in #1426) The
/// licensed-adjacent ACK gate fires on the internal dispatch path BEFORE any
/// Docker work. This pins the moved-but-unwired regression class structurally:
/// a non-TTY dispatch of `health-research` with no prior ack must bail at the
/// gate — no Docker preflight, no container spawn — so the test needs no
/// Docker and is CI-safe.
#[test]
fn dispatch_licensed_adjacent_role_bails_at_ack_gate_before_docker() {
    let ack_dir = TempDir::new().unwrap(); // empty — no prior ack on file
    darkmux_cmd()
        .env("DARKMUX_ACK_DIR", ack_dir.path())
        .args(["dispatch", "health-research", "smoke"])
        // assert_cmd pipes stdin (not a TTY), so the gate's non-interactive
        // arm bails rather than prompting for ACKNOWLEDGE.
        .assert()
        .failure()
        .stderr(
            predicate::str::contains("requires operator acknowledgment")
                // Bailed BEFORE the Docker preflight / container spawn: the
                // "runtime=internal — image:" line prints only after the
                // gate, and no docker error can have surfaced.
                .and(predicate::str::contains("runtime=internal").not())
                .and(predicate::str::contains("docker").not()),
        );
}


/// (Second review round, MUST FIX 1 follow-up: "neither disclosure call
/// site is test-covered") Pins the HOST-side half of the unset-compactor
/// disclosure (`CompactionDispatchArgs::unset_compactor_warning`, wired at
/// its call site in `dispatch_internal.rs`'s compactor-residency block) at
/// the real CLI boundary — the runtime half already gets an equivalent
/// integration assertion (`runtime/tests/`, spawning `darkmux-runtime`
/// directly). Before this, `dispatch_internal.rs`'s own comment on that call
/// site said plainly: "Still NOT covered ... deleting this whole call
/// compiles and stays green ... covering it needs a dispatch()-level
/// integration test this crate does not have" — `darkmux-crew` cannot spawn
/// `CARGO_BIN_EXE_darkmux` (that binary belongs to a different workspace
/// member, so Cargo never sets the env var there); this file's own package
/// IS that binary's package, so it can.
///
/// Fully hermetic, no `#[ignore]` needed: `DARKMUX_LMS_BIN` and `PATH` point
/// at fake `lms`/`docker` stand-ins (`install_fake_docker`'s idiom in
/// `crates/darkmux-crew/src/dispatch_internal_tests.rs`, reproduced here
/// since a cross-crate `dev-dependency` on that test-only helper would be
/// backwards). The fake `lms ps --json` reports `model-a` already resident
/// under its namespaced identifier (`darkmux:model-a`) at a context >= the
/// profile's declared `n_ctx`, so `ensure_model_resident` takes its early
/// `return Ok(())` and never calls `lms load` — no real model dispatch, no
/// real LMStudio contact, anywhere on this path (per the standing "do not
/// dispatch local models" guardrail). The fixture profile registry declares
/// no `internal.utility` binding and a real `n_ctx`, so the host reaches its
/// compactor-residency block with `compactor_model: None` and
/// `context_window: Some(32000)` — exactly `unset_compactor_warning`'s
/// firing condition. `--skip-preflight` is the CLI's own existing debug
/// escape hatch (already used this way in `darkmux-crew`'s own
/// `dispatch_preflight_probe_opts`) and only skips the SEPARATE
/// `check_docker_preflight` reachability/pull check; the fake `docker` on
/// `PATH` stands in for the `docker run` the dispatch still performs right
/// after — this test only cares what printed BEFORE that point, so the fake
/// docker just exits 0 immediately.
#[test]
fn dispatch_host_side_unset_compactor_disclosure_fires_on_the_local_path() {
    let tmp = TempDir::new().unwrap();

    let profiles_path = tmp.path().join("profiles.json");
    fs::write(
        &profiles_path,
        r#"{
            "profiles": {
                "fast": {
                    "description": "no compactor bound, a real context window",
                    "models": [
                        {"id": "model-a", "n_ctx": 32000, "role": "primary"}
                    ]
                }
            },
            "default_profile": "fast"
        }"#,
    )
    .unwrap();

    let fake_bin = tmp.path().join("fake-bin");
    fs::create_dir_all(&fake_bin).unwrap();
    let fake_lms = fake_bin.join("lms");
    fs::write(
        &fake_lms,
        "#!/bin/sh\n\
         if [ \"$1\" = \"ps\" ]; then\n\
         echo '[{\"identifier\":\"darkmux:model-a\",\"modelKey\":\"model-a\",\"status\":\"loaded\",\"sizeBytes\":1000000000,\"contextLength\":32000}]'\n\
         exit 0\n\
         fi\n\
         exit 0\n",
    )
    .unwrap();
    let fake_docker = fake_bin.join("docker");
    fs::write(&fake_docker, "#!/bin/sh\nexit 0\n").unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        for p in [&fake_lms, &fake_docker] {
            let mut perms = fs::metadata(p).unwrap().permissions();
            perms.set_mode(0o755);
            fs::set_permissions(p, perms).unwrap();
        }
    }

    let ack_dir = TempDir::new().unwrap();
    let real_path = std::env::var("PATH").unwrap_or_default();

    darkmux_cmd()
        .env("DARKMUX_ACK_DIR", ack_dir.path())
        .env("DARKMUX_PROFILES", &profiles_path)
        .env("DARKMUX_LMS_BIN", &fake_lms)
        .env("PATH", format!("{}:{real_path}", fake_bin.display()))
        .args(["dispatch", "coder", "--skip-preflight", "smoke"])
        .assert()
        .stderr(
            predicate::str::contains("no compactor is bound for this dispatch")
                .and(predicate::str::contains("compaction is OFF"))
                .and(predicate::str::contains("32000")),
        );
}

// ─── #2124: SIGTERM mid-probe leaves a terminal record + no orphaned curl ──

/// A tiny local server that ACCEPTS every connection and never responds —
/// makes a review probe's remote `curl` call (`darkmux-crew`'s
/// `remote_chat_attempt`) hang exactly the way a live LLM endpoint that
/// stopped answering would. Each accepted connection gets its own thread
/// that DRAINS whatever the peer sends (curl's request line, headers, and
/// body all arrive before curl blocks waiting for a response) and keeps
/// the socket open across every `Ok(n > 0)` read — the connection is only
/// released, and the stream only dropped, once a read returns `Ok(0)`
/// (peer sent FIN) or `Err` (peer reset). That is the instant `curl`
/// itself actually dies, which is what [`Self::wait_for_a_connection_to_close`]
/// below waits on.
///
/// (#2461) An earlier version of this handler read exactly ONE byte and
/// then let `stream` fall out of scope — which itself closed the
/// server's end of the socket right after curl's very first byte
/// (`'P'` of `POST`) landed. Two bugs from that one line: the stub never
/// actually held a connection open (curl got an immediate empty reply and
/// exited in milliseconds, so there was never a long-lived process for
/// `assert_no_surviving_remote_curl` to find), and the "closed" signal
/// fired on the request ARRIVING rather than on a real peer close. The
/// loop below fixes both: nothing closes this end until the peer does.
///
/// This is a more precise proof of reaping than polling `ps`/`pgrep` for a
/// process that might not exist yet: it observes the OS actually tearing
/// the connection down, not just a name disappearing from a process list.
struct HangingStubServer {
    port: u16,
    accepted_rx: std::sync::mpsc::Receiver<()>,
    closed_rx: std::sync::mpsc::Receiver<()>,
}

impl HangingStubServer {
    fn start() -> Self {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("binding the stub listener");
        let port = listener.local_addr().unwrap().port();
        let (accepted_tx, accepted_rx) = std::sync::mpsc::channel::<()>();
        let (closed_tx, closed_rx) = std::sync::mpsc::channel::<()>();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { continue };
                let _ = accepted_tx.send(());
                let closed_tx = closed_tx.clone();
                // (#2461) This reader thread's own lifetime is bounded by
                // the PEER, never by this process: it only returns once
                // curl's socket actually closes (curl exits, whether from
                // the production SIGKILL reap, its own `-m` bound, or the
                // whole test binary exiting and tearing down every fd on
                // the way out). Every caller that blocks on
                // `closed_rx`/`accepted_rx` already bounds ITS OWN wait
                // with `recv_timeout`, so a peer that never closes fails
                // that caller's assertion instead of hanging the suite.
                std::thread::spawn(move || {
                    use std::io::Read;
                    let mut buf = [0u8; 8192];
                    loop {
                        match stream.read(&mut buf) {
                            // The peer is still sending (or handed back a
                            // short read) — keep draining without closing
                            // our end. This server never writes a
                            // response, so curl blocks waiting for one
                            // until it is killed or its own `-m` bound
                            // fires.
                            Ok(n) if n > 0 => continue,
                            // (#2461 review) EINTR is NOT a peer close --
                            // it is a signal landing on THIS process while
                            // the read was parked. `std`'s bare
                            // `Read::read` does not retry it (only
                            // `read_exact`/`read_to_end` do), so without
                            // this arm a stray signal would fire the
                            // "closed" signal while curl is still very much
                            // alive -- re-opening exactly the false-`closed`
                            // hole this loop exists to close.
                            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                            // Ok(0): peer sent FIN. Err: peer reset.
                            // Either way this is a REAL close, not the
                            // request merely arriving.
                            _ => break,
                        }
                    }
                    let _ = closed_tx.send(());
                    // `stream` drops here, AFTER the real peer close —
                    // never before it.
                });
            }
        });
        Self { port, accepted_rx, closed_rx }
    }

    /// Block until curl has actually connected — the precise "now it's
    /// mid-probe" signal this test sends SIGTERM against, in place of a
    /// fixed sleep that would either race a slow CI runner or waste time
    /// on a fast one.
    fn wait_for_a_connection(&self, timeout: std::time::Duration) -> bool {
        self.accepted_rx.recv_timeout(timeout).is_ok()
    }

    /// Block until SOME accepted connection's read returned — i.e. some
    /// `curl` this test's review dispatch spawned has been torn down.
    fn wait_for_a_connection_to_close(&self, timeout: std::time::Duration) -> bool {
        self.closed_rx.recv_timeout(timeout).is_ok()
    }
}

/// (#2265 review) A stub endpoint that ANSWERS — the counterpart to
/// `HangingStubServer`, for tests that need a dispatch to run to completion
/// and leave its flow records behind rather than to hang mid-probe. Replies
/// to every request with one minimal chat completion. No model, no Docker: the
/// role it serves is tool-less, so `dispatch_internal` takes the light
/// single-shot hosted path (a plain host `curl`).
struct RespondingStubServer {
    port: u16,
}

impl RespondingStubServer {
    fn start() -> Self {
        let listener =
            std::net::TcpListener::bind("127.0.0.1:0").expect("binding the stub listener");
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { continue };
                std::thread::spawn(move || {
                    use std::io::{Read, Write};
                    // Read the request head (and whatever body arrives with
                    // it); curl waits for the response, so a bounded read is
                    // enough — this is a stub, not an HTTP server.
                    let mut buf = [0u8; 8192];
                    let _ = stream.read(&mut buf);
                    let body = serde_json::json!({
                        "choices": [{ "message": { "content": "ack" } }],
                        "usage": { "prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2 }
                    })
                    .to_string();
                    let _ = stream.write_all(
                        format!(
                            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n\
                             Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
                            body.len()
                        )
                        .as_bytes(),
                    );
                    let _ = stream.flush();
                });
            }
        });
        Self { port }
    }
}

fn responding_endpoint_profiles_json(port: u16) -> String {
    format!(
        r#"{{
            "profiles": {{
                "stub": {{
                    "models": [
                        {{"id": "stub-model", "n_ctx": 8000, "endpoint": {{"url": "http://127.0.0.1:{port}"}}}}
                    ]
                }}
            }},
            "default_profile": "stub"
        }}"#
    )
}

fn hanging_endpoint_profiles_json(port: u16) -> String {
    format!(
        r#"{{
            "profiles": {{
                "hang": {{
                    "models": [
                        {{"id": "stub-model", "n_ctx": 8000, "endpoint": {{"url": "http://127.0.0.1:{port}"}}}}
                    ]
                }}
            }},
            "default_profile": "hang"
        }}"#
    )
}

/// Assert that no `curl` spawned by the darkmux child `pid` outlives it.
/// Polls the process table for that child's own `darkmux-remote-<pid>-`
/// config-file marker (see `remote_chat_attempt`) for up to 2s: the OS
/// tears the table down asynchronously after SIGKILL, and instrumented
/// (coverage) builds are slower than a fixed settle delay allows for.
/// Scoped to `pid` so a sibling test's live curl is never mistaken for a
/// survivor of this one.
fn assert_no_surviving_remote_curl(pid: u32, label: &str) {
    let marker = format!("darkmux-remote-{pid}-");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
    let survivors = loop {
        let Ok(out) = std::process::Command::new("pgrep").args(["-f", &marker]).output() else {
            return; // no `pgrep` on this image — the socket-close proof already covers it
        };
        let survivors = String::from_utf8_lossy(&out.stdout).trim().to_string();
        if survivors.is_empty() || std::time::Instant::now() >= deadline {
            break survivors;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    };
    assert!(
        survivors.is_empty(),
        "a darkmux {label} curl process is still running after the parent exited: {survivors}"
    );
}

/// (#2476 review round 2, MUST FIX 4) `darkmux acp`'s own OS-signal
/// wiring (`acp::run()`'s `tokio::spawn(host_shutdown_reap_loop(
/// reap_on_host_shutdown))`, installed unconditionally before the ACP
/// stdio loop starts serving) has NEVER been constructed by any test —
/// every existing `acp.rs` signal test proves a piece of the mechanism
/// in-process (`wait_for_host_shutdown_signal_ready` reacts to a real
/// SIGTERM, `host_shutdown_reap_loop` calls its `on_signal` callback,
/// `child_registry::kill_all` kills a real registered pid), but nothing
/// spawns the REAL `darkmux acp` binary and signals it. `src/acp.rs`'s
/// own production closure (`reap_on_host_shutdown`, wired into `run()`)
/// was consequently silently deletable — deleting the `tokio::spawn(...)`
/// call in `run()` leaves every existing unit test green, since none of
/// them construct `acp::run()` itself.
///
/// This spawns the real binary in `acp` mode with piped stdio (stdin
/// held open by this test so the ACP stdio loop never sees an EOF that
/// would exit the process on its own, independent of signal handling —
/// this test needs the process to still be alive when SIGTERM lands, not
/// racing an unrelated early exit), waits for it to be observably
/// running, sends a real SIGTERM, and asserts it exits within a bound at
/// the documented exit code (130 — `reap_on_host_shutdown`'s own
/// `std::process::exit(130)`, matching the mission-launch SIGTERM
/// precedent's exit code elsewhere in this file).
///
/// RED-PROVED by hand: commenting out the `tokio::spawn(host_shutdown_
/// reap_loop(reap_on_host_shutdown));` line in `acp::run()` makes this
/// test fail — the process then has no signal handler installed at all,
/// SIGTERM takes default disposition (process-terminated-by-signal, no
/// exit code), and `exit_status.code()` reports `None` instead of
/// `Some(130)`, so the process also never got the chance to reap
/// anything it might have had in flight.
///
/// **Retries, growing the pre-signal wait, rather than one fixed sleep.**
/// There is no in-process readiness latch reachable from OUTSIDE a spawned
/// BINARY the way `acp.rs`'s own `wait_for_host_shutdown_signal_ready`
/// tests use one — a fixed sleep is the only pre-signal readiness signal
/// available here, and measured directly on this machine under real
/// concurrent load (multiple live `darkmux` processes + other cargo
/// activity, `uptime` load average in the 20s), even 2.5s was
/// insufficient often enough to make a single fixed-sleep attempt
/// unreliable — a genuinely loaded box can push a debug binary's tokio
/// runtime startup + first task scheduling round past that. Retrying
/// with a longer wait on each attempt (rather than one long wait
/// up-front) keeps the common case fast while still tolerating a
/// once-in-a-while slow scheduling round without flaking outright.
#[test]
fn acp_sigterm_reaps_children_and_exits_130() {
    const WAITS_MS: [u64; 4] = [1_000, 3_000, 6_000, 10_000];
    let mut last_debug = String::new();

    for (attempt, wait_ms) in WAITS_MS.iter().enumerate() {
        let mut child = darkmux_std_cmd()
            .args(["acp"])
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .expect("spawning darkmux acp");
        let pid = child.id();
        // Held so the child's stdin never sees EOF — an ACP agent seeing
        // EOF on stdin is a legitimate "the client disconnected" exit on
        // its own, which would race this test's SIGTERM instead of
        // proving anything about signal handling. Never written to; the
        // ACP protocol handshake is irrelevant here —
        // `host_shutdown_reap_loop` is spawned independently of (and
        // before) the stdio-driven `serve()` loop.
        let _stdin_holder = child.stdin.take();

        std::thread::sleep(std::time::Duration::from_millis(*wait_ms));

        assert!(
            child.try_wait().expect("polling darkmux acp before SIGTERM").is_none(),
            "darkmux acp must still be running before SIGTERM — an early exit here would prove \
             nothing about signal handling"
        );

        let kill_status = std::process::Command::new("kill")
            .args(["-TERM", &pid.to_string()])
            .status()
            .expect("running kill -TERM");
        assert!(kill_status.success(), "kill -TERM itself must succeed");

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let exit_status = loop {
            if let Some(status) = child.try_wait().expect("polling darkmux acp after SIGTERM") {
                break status;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "darkmux acp did not exit within 5s of SIGTERM (#2476 review round 2 regression)"
            );
            std::thread::sleep(std::time::Duration::from_millis(50));
        };

        if exit_status.code() == Some(130) {
            return; // PASS — the documented signal-handling contract held.
        }

        // A signal-terminated exit (no code at all) with a SHORT wait
        // behind it is exactly the "listener wasn't installed yet on a
        // loaded box" outcome this retry loop exists to tolerate — but
        // ONLY on the shortest attempt; a longer wait still landing here
        // stops looking like scheduling noise. Any OTHER exit code (a
        // real crash / clean-but-wrong exit) is never worth retrying —
        // that is a genuine finding, reported immediately.
        use std::io::Read;
        let mut out = String::new();
        let mut err = String::new();
        let _ = child.stdout.take().unwrap().read_to_string(&mut out);
        let _ = child.stderr.take().unwrap().read_to_string(&mut err);
        last_debug = format!(
            "attempt {} (wait={wait_ms}ms): exit_status={exit_status:?} stdout={out:?} stderr={err:?}",
            attempt + 1
        );
        if exit_status.code().is_some() {
            panic!(
                "darkmux acp exited with an unexpected but well-formed code (not a signal-\
                 disposition race — not worth retrying): {last_debug}"
            );
        }
    }

    panic!(
        "darkmux acp never reached its documented SIGTERM exit code (130) across {} attempts \
         with growing pre-signal waits (up to {}ms) — this is no longer plausibly scheduling \
         noise. Last attempt: {last_debug}",
        WAITS_MS.len(),
        WAITS_MS.last().unwrap()
    );
}

/// True if `redis-server` is on PATH — `darkmux serve`'s fleet runner
/// thread (the thing that actually gets a dispatch child registered for
/// `serve_sigterm_reaps_the_fleet_runners_curl_child` below to reap) only
/// activates with a real Redis to point it at. Mirrors
/// `tests/e2e/harness.rs`'s own `redis_available` gate (that file's own
/// doc explains why a missing dependency must not silently read as
/// "passed" — this test opts into the SAME discipline, but stays inside
/// `tests/cli.rs` rather than pulling in the full `FleetHarness` (a
/// release-binary-building, multi-node harness built for a different
/// scenario shape) for what only needs one daemon + one ephemeral redis.
fn redis_available_for_serve_test() -> bool {
    std::process::Command::new("redis-server")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Spawn a throwaway `redis-server` on an ephemeral port, isolated under
/// `workdir` (no persistence — `--save ""`, `--appendonly no`). Returns
/// the child (kill it when done — this test owns it, never signals it by
/// pattern) and its `redis://` URL. Same shape as `tests/e2e/harness.rs`'s
/// `spawn_redis`, kept local here rather than shared: this test needs
/// only single-instance ephemeral Redis, not that harness's multi-node
/// bookkeeping.
fn spawn_ephemeral_redis(workdir: &std::path::Path) -> (std::process::Child, String) {
    fs::create_dir_all(workdir).expect("creating redis workdir");
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("binding an ephemeral redis port");
    let port = listener.local_addr().unwrap().port();
    drop(listener); // release for redis-server to bind

    let child = std::process::Command::new("redis-server")
        .arg("--port")
        .arg(port.to_string())
        .arg("--save")
        .arg("")
        .arg("--appendonly")
        .arg("no")
        .arg("--dir")
        .arg(workdir)
        .arg("--bind")
        .arg("127.0.0.1")
        .arg("--protected-mode")
        .arg("no")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("spawning redis-server (is it on PATH? `brew install redis`)");

    let url = format!("redis://127.0.0.1:{port}");
    let client = redis::Client::open(url.as_str()).expect("redis::Client::open");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        if let Ok(mut conn) = client.get_connection() {
            let ping: redis::RedisResult<String> = redis::cmd("PING").query(&mut conn);
            if ping.as_deref() == Ok("PONG") {
                break;
            }
        }
        assert!(std::time::Instant::now() < deadline, "redis-server on {url} never became ready");
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    (child, url)
}

/// Poll `GET /health` on `port` until it answers or `timeout` elapses —
/// a real observable readiness signal for `darkmux serve`, not a fixed
/// sleep.
fn wait_for_serve_health(port: u16, timeout: std::time::Duration) {
    let addr: std::net::SocketAddr = format!("127.0.0.1:{port}").parse().unwrap();
    let deadline = std::time::Instant::now() + timeout;
    loop {
        if std::net::TcpStream::connect_timeout(&addr, std::time::Duration::from_millis(200)).is_ok() {
            return;
        }
        assert!(std::time::Instant::now() < deadline, "darkmux serve on :{port} never became reachable");
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
}

/// (#2476 review round 2, MUST FIX 4) `darkmux_serve::run()`'s own
/// shutdown wiring — specifically the `reap_dispatch_children_on_
/// shutdown()` call inside `run()`'s shutdown task — has NEVER been
/// constructed by any test. `lib_tests.rs`'s own
/// `reap_dispatch_children_on_shutdown_kills_a_real_registered_child`
/// calls that function DIRECTLY (its own doc concedes "no OS signal
/// needed HERE") — proving the function's own behavior, never that
/// `run()` actually CALLS it on a real SIGTERM. Deleting the call site
/// (`crates/darkmux-serve/src/lib.rs`'s `reap_dispatch_children_on_
/// shutdown();` inside `run()`) leaves every existing `darkmux-serve`
/// unit test green.
///
/// This spawns a real `darkmux serve` daemon pointed at a throwaway
/// Redis + a `DARKMUX_PROFILES` registry naming a HANGING endpoint (the
/// same `HangingStubServer` + `hanging_endpoint_profiles_json` fixture
/// the SIGTERM-mid-dispatch tests above use — no model dispatch
/// required), publishes one `WorkJob` for a tool-less role onto the
/// fleet queue so the daemon's OWN fleet-runner thread claims it and
/// blocks on a real `curl` to the hanging stub — a REAL in-flight
/// dispatch child registered in `child_registry`, exactly the shape
/// `reap_dispatch_children_on_shutdown`'s own doc describes — then sends
/// a real SIGTERM and asserts: the daemon exits within a bound, and the
/// `curl` child is torn down rather than orphaned past the parent's
/// exit.
///
/// **Scope, stated honestly.** The hosted/curl path (a tool-less role,
/// which this test uses) needs only `kill_all`'s SIGKILL to reap
/// cleanly — `dispatch_internal.rs`'s hosted-call path has no extra
/// post-wait cleanup step the way the DOCKER container path does (see
/// `docker_kill_by_name`'s own call sites). So this test proves the
/// production wiring is exercised and that a real curl child does not
/// orphan past `darkmux serve`'s exit — it does NOT reach the
/// container-specific race MUST FIX 3 also fixed (the `docker kill
/// <container>` window), which needs Docker + the runtime image,
/// unavailable in this test environment — the same documented
/// limitation `mission_launch_generic_sigterm_mid_dispatch_finalizes_
/// and_reaps_curl`'s own module comment names for crawl's container
/// path, above.
///
/// RED-PROVED by hand: commenting out the `reap_dispatch_children_on_
/// shutdown();` call inside `darkmux-serve/src/lib.rs`'s `run()` makes
/// this test fail — the fleet-runner thread's `curl` child is never
/// signaled, so it keeps running (holding the stub connection open)
/// past the daemon's own exit, and `assert_no_surviving_remote_curl`
/// catches the orphan.
#[test]
fn serve_sigterm_reaps_the_fleet_runners_curl_child() {
    if !redis_available_for_serve_test() {
        eprintln!("skipping: redis-server not on PATH");
        return;
    }

    let stub = HangingStubServer::start();
    let (home, darkmux_home) = isolated_roots();
    let redis_workdir = home.parent().unwrap().join("redis");
    let (redis_child_raw, redis_url) = spawn_ephemeral_redis(&redis_workdir);
    // (#2476 review round 2 — cleanup gap caught during development) RAII,
    // not a plain `redis_child.kill()` at the bottom of this function: an
    // assertion panicking anywhere ABOVE that point (any of `wait_for_
    // serve_health`, the stub-connection wait, the post-SIGTERM exit wait,
    // or the final close/no-orphan checks) unwinds past that bare call —
    // Rust does not run ordinary statements during an unwind, only `Drop`
    // impls — and leaks a real `redis-server` process. Measured live: a
    // red-prove run against this exact test left one running for hours.
    // `DirectChildGuard` fires on every exit path, panic included.
    let _redis_child = DirectChildGuard(redis_child_raw);

    let profiles_path = darkmux_home.join("profiles.json");
    fs::write(&profiles_path, hanging_endpoint_profiles_json(stub.port)).unwrap();
    let flows_dir = darkmux_home.join("flows");
    fs::create_dir_all(&flows_dir).unwrap();

    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("binding an ephemeral serve port");
    let serve_port = listener.local_addr().unwrap().port();
    drop(listener);

    let serve_child_raw = darkmux_std_cmd()
        .env("HOME", &home)
        .env("DARKMUX_HOME", &darkmux_home)
        .env("DARKMUX_MACHINE_ID", "cli-test-serve-node")
        .env("DARKMUX_REDIS_URL", &redis_url)
        .env("DARKMUX_PROFILES", &profiles_path)
        .env("DARKMUX_FLOWS_DIR", &flows_dir)
        .args(["serve", "--bind", "127.0.0.1", "--port", &serve_port.to_string()])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("spawning darkmux serve");
    // Same reasoning as `redis_child` above — a panicked assertion before
    // this test's own explicit SIGTERM+wait would otherwise leak a live
    // `darkmux serve` daemon too.
    let mut serve_child = DirectChildGuard(serve_child_raw);
    let serve_pid = serve_child.id();

    wait_for_serve_health(serve_port, std::time::Duration::from_secs(15));

    // Publish directly onto the same Redis the daemon's fleet-runner
    // thread is polling — the runner claims it, converts it via
    // `into_dispatch_opts`, and calls the SAME synchronous
    // `crew::dispatch::dispatch` the CLI's own `dispatch` verb uses; a
    // tool-less role routes that to the light single-shot HOSTED path (a
    // plain host `curl`), which is what actually reaches the stub.
    let redis_client = redis::Client::open(redis_url.as_str()).expect("redis::Client::open for publish");
    let job = darkmux_fleet::WorkJob {
        target_machine: None,
        role_id: "dialectic-judge".to_string(),
        message: "hang please".to_string(),
        session_id: "cli-test-serve-sigterm-session".to_string(),
        workdir: None,
        phase_id: None,
        image: None,
        timeout_seconds: 60,
        published_at_unix_ms: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64,
        published_by_machine: None,
        published_by_orchestrator: None,
        attempt: 1,
    };
    darkmux_fleet::publish_job(&redis_client, &job).expect("publishing the WorkJob onto the fleet queue");

    // A REAL observable readiness signal, not a fixed sleep: the runner
    // thread has claimed the job and its dispatch's `curl` has actually
    // reached the hanging stub.
    assert!(
        stub.wait_for_a_connection(std::time::Duration::from_secs(20)),
        "the fleet runner never reached a dispatch call to the stub server within 20s — either \
         it never claimed the published job, or dispatch never got as far as the curl call"
    );

    let kill_status = std::process::Command::new("kill")
        .args(["-TERM", &serve_pid.to_string()])
        .status()
        .expect("running kill -TERM");
    assert!(kill_status.success(), "kill -TERM itself must succeed");

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
    loop {
        if let Some(status) = serve_child.try_wait().expect("polling darkmux serve after SIGTERM") {
            eprintln!("darkmux serve exited with {status:?}");
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "darkmux serve did not exit within 15s of SIGTERM (#2476 review round 2 regression)"
        );
        std::thread::sleep(std::time::Duration::from_millis(100));
    }

    assert!(
        stub.wait_for_a_connection_to_close(std::time::Duration::from_secs(5)),
        "no curl connection to the stub server was ever torn down — the fleet runner's dispatch \
         child survived the daemon's own exit (#2476 review round 2 regression)"
    );
    assert_no_surviving_remote_curl(serve_pid, "serve");

    // Both children are cleaned up by `DirectChildGuard`'s `Drop`, at the
    // end of this function's scope — see that struct's own doc.
}

/// Best-effort cleanup for a `Child` this test spawned and holds
/// DIRECTLY — always kill-then-wait on drop, regardless of which exit
/// path (normal return, an early `assert!` panic mid-test) got there.
/// Safe unconditionally, unlike the pid-remembering `KillOnDrop` above:
/// this guard always holds the ORIGINAL `Child` handle rather than a
/// bare pid, so there is no window in which the underlying pid could
/// have been reaped and recycled onto an unrelated process before this
/// fires — killing (or re-killing an already-exited) `Child` through its
/// own handle is always safe (`std::process::Child::kill`'s own doc: an
/// already-exited child is simply a no-op-ish `Err`, ignored here).
///
/// (#2476 review round 2 — cleanup gap caught during development, see
/// `serve_sigterm_reaps_the_fleet_runners_curl_child`'s own comment)
/// `Deref`/`DerefMut` to `Child` so call sites read exactly like they
/// would against a bare `Child` (`.id()`, `.try_wait()`).
struct DirectChildGuard(std::process::Child);

impl std::ops::Deref for DirectChildGuard {
    type Target = std::process::Child;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl std::ops::DerefMut for DirectChildGuard {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

impl Drop for DirectChildGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

// ─── #2131: the shared LaunchFinalizeGuard, ported to crawl + generic ─────
//
// (#2131, historical) When this proof was written, `src/crawl_launch.rs` was a
// separate literal-routed launcher sharing the `LaunchFinalizeGuard`; it and
// `crawl_launch_tests.rs` were DELETED in #2301 — crawl now runs through the
// generic launcher this file proves, and its finalize/interrupt coverage lives
// in `crates/darkmux-lab/src/crawl/unit_step_tests.rs` (the scheduler-level
// probe that runs plan → unit → summary through `run_step_graph` with an
// injected dispatch). The paragraph that follows describes the situation as
// it was, kept because it explains why the live test below exercises the
// tool-less hosted path rather than a container:
// (then) `crawl_launch.rs` gained the same `LaunchFinalizeGuard` + SIGTERM/SIGHUP
// this file adds a live binary-level proof for below (the generic-graph/
// coder-phase launcher), but does NOT get an equivalent live-dispatch
// integration test here: crawl's role_id is hardcoded to `"crawler"`
// (tool-granting), so its dispatch always goes through the agentic
// `darkmux-runtime` CONTAINER path (`dispatch_internal.rs`'s docker spawn,
// #2114's own concurrent surface) rather than the tool-less light
// single-shot HOSTED path (a plain host `curl`) the test below exercises —
// and that container path's child pid isn't registered into
// `darkmux_types::child_registry` yet, so a real "no lingering container"
// proof isn't reachable without either Docker + the runtime image
// (unavailable in this environment; the release-gate doctrine reserves
// that kind of real-container run for dogfood, not `cargo test`) or wiring
// `child_registry` into the docker spawn site — deliberately left
// untouched here per this task's own boundary. Crawl's coverage instead
// rests on: `crawl_launch_tests.rs`'s
// `a_panic_mid_loop_still_finalizes_via_the_raii_guard` (proves the
// Drop-abort-writer shape survived the guard extraction) and
// `interrupted_at_readback_reports_interrupted_not_error` (proves the
// interrupt-flag read path), both passing unchanged against the shared
// guard, plus `crate::launch_guard::arm()` replacing the old SIGINT-only
// `darkmux_types::interrupt::install()` call (verified by `cargo check` +
// the module's own doc). Follow-up: wire `child_registry` into the docker
// spawn path, then add crawl's own live SIGTERM+reap test the same shape
// as the one below.

/// (#2131) `kill <pid>` (SIGTERM) on `mission launch <generic-graph-config>`
/// blocked mid-dispatch (a real `curl` call to an endpoint that never
/// answers) must: exit within 5s, leave the mission `finalized` with the
/// phase `abandoned` (never stuck `active`), and leave no `curl` process
/// still holding the stub connection open. This is the launcher #2131's own
/// issue named as having NO guard at all before this PR — a minimal
/// user-tier config with a single `dispatch.internal` step exercises the
/// SAME generic-graph path `coder-phase` and every `mission propose`-built
/// config also run through.
#[test]
fn mission_launch_generic_sigterm_mid_dispatch_finalizes_and_reaps_curl() {
    let stub = HangingStubServer::start();

    let home = TempDir::new().unwrap();
    let flows = TempDir::new().unwrap();

    let profiles_path = home.path().join("profiles.json");
    fs::write(&profiles_path, hanging_endpoint_profiles_json(stub.port)).unwrap();

    let config_dir = home.path().join("mission-configs");
    fs::create_dir_all(&config_dir).unwrap();
    // (#2131) `dialectic-judge` is deliberately TOOL-LESS
    // (`tool_palette.allow: []`) — `dispatch_internal.rs` routes a
    // tool-less role's remote dispatch through the light single-shot
    // HOSTED path (a plain host-side `curl`, already `child_registry`-
    // wired) rather than spinning up a `darkmux-runtime` container, which
    // this test environment has neither Docker nor the image for. A
    // tool-granting role (e.g. `crawler`) would instead need a real
    // container.
    let config_json = r#"{
        "id": "sigterm-generic-test",
        "name": "SIGTERM Generic Test",
        "schema_version": "2.3",
        "phases": [{
            "id": "p1",
            "tasks": [{
                "id": "t1",
                "steps": [{
                    "id": "s1",
                    "kind": "dispatch.internal",
                    "config": { "role_id": "dialectic-judge", "message": "hang please" }
                }]
            }]
        }]
    }"#;
    fs::write(config_dir.join("sigterm-generic-test.json"), config_json).unwrap();

    let mut child = darkmux_std_cmd()
        .env("DARKMUX_HOME", home.path())
        .env("DARKMUX_FLOWS_DIR", flows.path())
        .env("DARKMUX_PROFILES", &profiles_path)
        .args(["mission", "launch", "sigterm-generic-test", "--timeout", "60"])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("spawning darkmux mission launch sigterm-generic-test");
    let pid = child.id();

    assert!(
        stub.wait_for_a_connection(std::time::Duration::from_secs(20)),
        "the generic-graph dispatch never reached a dispatch call to the stub server within 20s"
    );
    assert!(
        child.try_wait().unwrap().is_none(),
        "the generic launcher must still be running (blocked on the hanging dispatch) before SIGTERM"
    );

    let kill_status =
        std::process::Command::new("kill").args(["-TERM", &pid.to_string()]).status().expect("running kill -TERM");
    assert!(kill_status.success(), "kill -TERM itself must succeed");

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    let exit_status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "mission launch (generic) did not exit within 5s of SIGTERM (#2131 regression)"
        );
        std::thread::sleep(std::time::Duration::from_millis(50));
    };
    assert!(!exit_status.success(), "a signal-interrupted generic-graph run must not exit 0");

    assert!(
        stub.wait_for_a_connection_to_close(std::time::Duration::from_secs(3)),
        "no `curl` connection to the stub server was ever torn down — a child process survived \
         the parent (#2131 regression)"
    );

    assert_no_surviving_remote_curl(child.id(), "generic-graph");

    let missions_dir = home.path().join("missions");
    let mission_id = fs::read_dir(&missions_dir)
        .unwrap_or_else(|e| panic!("reading {}: {e}", missions_dir.display()))
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().to_string())
        .next()
        .expect("exactly one mission must have been minted");

    let mission_json: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(missions_dir.join(&mission_id).join("mission.json")).unwrap())
            .unwrap();
    assert_eq!(
        mission_json["status"], "finalized",
        "an interrupted generic-graph run must reach a terminal mission status, never stay active: {mission_json}"
    );

    let phases_dir = missions_dir.join(&mission_id).join("phases");
    let mut saw_a_phase = false;
    for entry in fs::read_dir(&phases_dir).unwrap().filter_map(|e| e.ok()) {
        let phase_json: serde_json::Value = serde_json::from_str(&fs::read_to_string(entry.path()).unwrap()).unwrap();
        assert_eq!(
            phase_json["status"], "abandoned",
            "a signal-interrupted generic-graph run must abandon its phase, never complete one: {phase_json}"
        );
        saw_a_phase = true;
    }
    assert!(saw_a_phase, "the mint must have produced at least one phase to check");
}

/// (#2262) `kill <pid>` (SIGTERM) on a plain `darkmux dispatch <role>`
/// blocked mid-dispatch (a real `curl` call to an endpoint that never
/// answers) must: exit within 5s, leave a terminal `dispatch.error`
/// bookend behind (surfaced here via the crew-of-one mission this dispatch
/// mints — `dispatch_as_crew_of_one.rs`'s `finalize`/`reconcile_on_error`
/// reach the SAME `finalize_mission` a `mission launch` run does), and
/// leave no `curl` process still holding the stub connection open. Before
/// #2262's fix, `dispatch` installed no signal handling at all — the
/// same gap `mission launch` had before #2131, just never closed here —
/// so a SIGTERM killed the process outright (default disposition), no
/// `Drop` ran, and the container/curl child was orphaned.
#[test]
fn dispatch_sigterm_mid_dispatch_finalizes_and_reaps_curl() {
    let stub = HangingStubServer::start();

    let home = TempDir::new().unwrap();
    let flows = TempDir::new().unwrap();
    // (#2262 review) The spawned binary gets an isolated OS `HOME` as well as
    // an isolated `DARKMUX_HOME` — a SIBLING temp root, never a parent of it.
    // `DARKMUX_HOME` only covers darkmux's OWN root (`paths::resolve` returns
    // early on it); anything the dispatch shells out to still resolves the
    // real `$HOME`. Measured on the `lab run` twin below, which writes
    // `$HOME/.lmstudio-home-pointer` on every run; done here too so the two
    // proofs isolate identically rather than one of them relying on the
    // dispatch path happening not to reach an `lms` shell-out today.
    let os_home = TempDir::new().unwrap();

    let profiles_path = home.path().join("profiles.json");
    fs::write(&profiles_path, hanging_endpoint_profiles_json(stub.port)).unwrap();

    // (#2262, matching #2131's own test) `dialectic-judge` is a built-in,
    // deliberately TOOL-LESS role (`tool_palette.allow: []`) — so
    // `dispatch_internal.rs` routes its remote dispatch through the light
    // single-shot HOSTED path (a plain host-side `curl`, already
    // `child_registry`-wired) rather than a `darkmux-runtime` container,
    // which this test environment has neither Docker nor the image for.
    // (#2184) Through the isolating helper, not a raw spawn — the structural
    // guard in this file refuses the latter. Both overrides below are the
    // helper's own documented escape: a later `.env` wins, so this test still
    // gets the specific roots it seeds and asserts against, while the DEFAULT
    // is isolation rather than a raw inherit.

    // (#2462 review) stderr goes to a FILE, not `/dev/null` — the operator's
    // whole diagnosis lives there, and sending it to the void is exactly why
    // the first cut of this fix shipped a force-exit that discarded the
    // message the fix exists to produce (see `launch_guard::
    // report_reap_and_exit_on_signal`). A file rather than a pipe on
    // purpose: this test polls `try_wait` instead of reading the child, so a
    // pipe could fill and deadlock the very process it is timing.
    let stderr_path = home.path().join("dispatch-stderr.log");
    let stderr_file = fs::File::create(&stderr_path).unwrap();
    let mut child = darkmux_std_cmd()
        .env("HOME", os_home.path())
        .env("DARKMUX_HOME", home.path())
        .env("DARKMUX_FLOWS_DIR", flows.path())
        .env("DARKMUX_PROFILES", &profiles_path)
        .args(["dispatch", "dialectic-judge", "hang please", "--timeout", "60"])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::from(stderr_file))
        .spawn()
        .expect("spawning darkmux dispatch dialectic-judge");
    let pid = child.id();

    assert!(
        stub.wait_for_a_connection(std::time::Duration::from_secs(20)),
        "the dispatch never reached a dispatch call to the stub server within 20s"
    );
    assert!(
        child.try_wait().unwrap().is_none(),
        "the dispatch must still be running (blocked on the hanging dispatch) before SIGTERM"
    );

    let kill_status =
        std::process::Command::new("kill").args(["-TERM", &pid.to_string()]).status().expect("running kill -TERM");
    assert!(kill_status.success(), "kill -TERM itself must succeed");

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    let exit_status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "darkmux dispatch did not exit within 5s of SIGTERM (#2262 regression)"
        );
        std::thread::sleep(std::time::Duration::from_millis(50));
    };
    assert!(!exit_status.success(), "a signal-interrupted dispatch must not exit 0");
    // (#2462) The terminal mission record is already durable by the time
    // `dispatch_as_crew_of_one::dispatch` returns its `Err` (asserted via
    // `mission_json["status"]` below) — `main.rs`'s `cmd_dispatch` then
    // calls `launch_guard::reap_and_exit_on_signal()` on that `Err`, which
    // force-exits 130 (128 + SIGTERM's conventional 2), the SAME code
    // `mission launch` already exits with on a caught signal. Before this
    // fix, the `Err` just propagated up to the default error handler,
    // which exits 1 — indistinguishable from a real endpoint failure to
    // any wrapper script reading the exit code alone.
    assert_eq!(
        exit_status.code(),
        Some(130),
        "a signal-interrupted dispatch must exit 130 (like `mission launch`), not the generic \
         error code 1 — a wrapper script can't otherwise tell an operator's Ctrl-C from a real \
         failure: {exit_status:?}"
    );

    assert!(
        stub.wait_for_a_connection_to_close(std::time::Duration::from_secs(3)),
        "no `curl` connection to the stub server was ever torn down — a child process survived \
         the parent (#2262 regression)"
    );

    assert_no_surviving_remote_curl(child.id(), "dispatch");

    // `darkmux dispatch` routes through `dispatch_as_crew_of_one`, which
    // mints a real (cardinality-one) mission for every dispatch — its
    // `finalize`/`reconcile_on_error` reach the same `finalize_mission`
    // a `mission launch` run does, so the terminal-record shape is
    // directly comparable to the #2131 test above.
    let missions_dir = home.path().join("missions");
    let mission_id = fs::read_dir(&missions_dir)
        .unwrap_or_else(|e| panic!("reading {}: {e}", missions_dir.display()))
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().to_string())
        .next()
        .expect("exactly one mission must have been minted");

    let mission_json: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(missions_dir.join(&mission_id).join("mission.json")).unwrap())
            .unwrap();
    assert_eq!(
        mission_json["status"], "finalized",
        "an interrupted dispatch must reach a terminal mission status, never stay active: {mission_json}"
    );

    // (#2462) `envelope.json`'s `reason` is what an operator actually reads
    // to find out WHY a dispatch didn't finish. Before this fix it came
    // straight from `describe_curl_failure`'s bare "chat request to ...
    // failed (curl exit -1): " — no stderr (SIGKILL leaves none), reading
    // exactly like the endpoint broke. It didn't; darkmux killed its own
    // curl because the operator sent a signal.
    let envelope_json: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(missions_dir.join(&mission_id).join("envelope.json")).unwrap())
            .unwrap();
    let reason = envelope_json["reason"].as_str().unwrap_or_default();
    assert!(
        reason.contains("interrupted by an operator signal"),
        "an interrupted dispatch's envelope reason must name the signal as the cause, not read \
         as an endpoint failure: {reason:?}"
    );

    // (#2462 review) The record on disk being right is not enough — the
    // OPERATOR'S SCREEN is the surface this fix is about, and the exit-130
    // path force-exits before `main`'s own `Error: ...` printing ever runs.
    // Measured on the real binary: with the bare `reap_and_exit_on_signal()`
    // call, this file held the interrupt banner and nothing else. Both
    // halves are asserted — that stderr names the signal, and that it does
    // NOT read as an endpoint failure — because the pre-fix message
    // (`describe_curl_failure`'s "chat request to ... failed (curl exit
    // -1): ") would satisfy a looser "something was printed" check.
    let stderr_text = fs::read_to_string(&stderr_path).unwrap();
    assert!(
        stderr_text.contains("interrupted by an operator signal"),
        "a signal-interrupted dispatch must PRINT why it stopped, not exit 130 in silence — the \
         force-exit runs before `main`'s error printing, so the call site has to print first: \
         {stderr_text:?}"
    );
    assert!(
        !stderr_text.contains("curl exit -1"),
        "stderr must not blame the endpoint for darkmux killing its own child: {stderr_text:?}"
    );
}

/// (#2262) `kill <pid>` (SIGTERM) on `darkmux lab run <workload>` blocked
/// mid-dispatch (a real `curl` call to an endpoint that never answers)
/// must: exit within 5s, leave the run's `lifecycle.json` in a TERMINAL
/// status (never stuck `running`), and leave no `curl` process still
/// holding the stub connection open. Mirrors the `dispatch` proof above —
/// `lab run`'s dispatch goes through the same `crew::dispatch::dispatch`
/// primitive, one layer further from the mission machinery (no mission is
/// minted for a lab run at all — see `crates/darkmux-lab/src/lab/
/// lifecycle.rs`'s own `RunLifecycle`), so the terminal record this test
/// checks is the lab run's own lifecycle bookend, not a mission envelope.
#[test]
fn lab_run_sigterm_mid_dispatch_finalizes_lifecycle_and_reaps_curl() {
    let stub = HangingStubServer::start();

    let home = TempDir::new().unwrap();
    let flows = TempDir::new().unwrap();
    // (#2262 review) See the `dispatch` twin above. Load-bearing HERE
    // specifically: measured, this run reaches an `lms` shell-out, which
    // writes `$HOME/.lmstudio-home-pointer` — so without this the test wrote
    // into the developer's (and CI's) REAL home directory on every run.
    let os_home = TempDir::new().unwrap();

    let profiles_path = home.path().join("profiles.json");
    fs::write(&profiles_path, hanging_endpoint_profiles_json(stub.port)).unwrap();

    // A user-tier workload (the `prompt` provider — no sandbox, no Docker
    // seed) bound to the same tool-less `dialectic-judge` role the
    // `dispatch` proof above uses, so this run ALSO takes the light
    // single-shot HOSTED `curl` path rather than a container.
    let workloads_dir = home.path().join("workloads");
    fs::create_dir_all(&workloads_dir).unwrap();
    let workload_json = r#"{
        "workload": {
            "id": "sigterm-lab-hang-test",
            "provider": "prompt",
            "description": "SIGTERM lab-run regression fixture (#2262)",
            "role": "dialectic-judge",
            "prompt": "hang please"
        }
    }"#;
    fs::write(workloads_dir.join("sigterm-lab-hang-test.json"), workload_json).unwrap();

    // (#2184) Through the isolating helper, not a raw spawn — the structural
    // guard in this file refuses the latter. Both overrides below are the
    // helper's own documented escape: a later `.env` wins, so this test still
    // gets the specific roots it seeds and asserts against, while the DEFAULT
    // is isolation rather than a raw inherit.

    // (#2462 review) stderr to a FILE, not `/dev/null` — see the `dispatch`
    // twin above for why (the force-exit runs before `main` prints the
    // error, and `/dev/null` is what hid that) and for why a file rather
    // than a pipe (this test polls `try_wait`; a full pipe would deadlock).
    let stderr_path = home.path().join("lab-run-stderr.log");
    let stderr_file = fs::File::create(&stderr_path).unwrap();
    let mut child = darkmux_std_cmd()
        .env("HOME", os_home.path())
        .env("DARKMUX_HOME", home.path())
        .env("DARKMUX_FLOWS_DIR", flows.path())
        .env("DARKMUX_PROFILES", &profiles_path)
        .args(["lab", "run", "sigterm-lab-hang-test", "--profile", "hang", "--profiles-file"])
        .arg(&profiles_path)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::from(stderr_file))
        .spawn()
        .expect("spawning darkmux lab run sigterm-lab-hang-test");
    let pid = child.id();

    assert!(
        stub.wait_for_a_connection(std::time::Duration::from_secs(20)),
        "the lab run never reached a dispatch call to the stub server within 20s"
    );
    assert!(
        child.try_wait().unwrap().is_none(),
        "the lab run must still be running (blocked on the hanging dispatch) before SIGTERM"
    );

    let kill_status =
        std::process::Command::new("kill").args(["-TERM", &pid.to_string()]).status().expect("running kill -TERM");
    assert!(kill_status.success(), "kill -TERM itself must succeed");

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    let exit_status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "darkmux lab run did not exit within 5s of SIGTERM (#2262 regression)"
        );
        std::thread::sleep(std::time::Duration::from_millis(50));
    };
    assert!(!exit_status.success(), "a signal-interrupted lab run must not exit 0");
    // (#2462) `lab_run`'s own `lifecycle.json` terminal write is already
    // durable by the time it returns its `Err` (asserted via
    // `lifecycle_json["status"]` below) — `lab_cli.rs`'s `cmd_lab` then
    // calls `launch_guard::reap_and_exit_on_signal()` on that `Err`, which
    // force-exits 130, the SAME code `mission launch`/`dispatch` exit with
    // on a caught signal. Before this fix the `Err` just propagated to the
    // default error handler (exit 1) — indistinguishable from a real
    // failure to any wrapper script reading the exit code alone.
    assert_eq!(
        exit_status.code(),
        Some(130),
        "a signal-interrupted lab run must exit 130, not the generic error code 1 — a wrapper \
         script can't otherwise tell an operator's Ctrl-C from a real failure: {exit_status:?}"
    );

    assert!(
        stub.wait_for_a_connection_to_close(std::time::Duration::from_secs(3)),
        "no `curl` connection to the stub server was ever torn down — a child process survived \
         the parent (#2262 regression)"
    );

    assert_no_surviving_remote_curl(child.id(), "lab-run");

    let runs_dir = home.path().join("runs");
    let run_id = fs::read_dir(&runs_dir)
        .unwrap_or_else(|e| panic!("reading {}: {e}", runs_dir.display()))
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().to_string())
        .next()
        .expect("exactly one lab run dir must have been minted");

    let lifecycle_json: serde_json::Value = serde_json::from_str(
        &fs::read_to_string(runs_dir.join(&run_id).join("lifecycle.json"))
            .unwrap_or_else(|e| panic!("reading lifecycle.json for run {run_id}: {e}")),
    )
    .unwrap();
    assert_ne!(
        lifecycle_json["status"], "running",
        "an interrupted lab run must reach a terminal lifecycle status, never stay `running`: \
         {lifecycle_json}"
    );
    // (#2462) The whole point: a signal-caused failure must not be
    // archived as `error` — that is the "the endpoint broke" misattribution
    // the issue is about. It must read `interrupted`, and the `error` field
    // must say WHY in terms of the signal, not `describe_curl_failure`'s
    // bare "chat request to ... failed (curl exit -1): " (empty stderr,
    // since SIGKILL leaves none).
    assert_eq!(
        lifecycle_json["status"], "interrupted",
        "a signal-caused lab run failure must be archived as `interrupted`, not `error` — \
         recording `error` here is exactly the wrong-cause bug #2462 is about: {lifecycle_json}"
    );
    let error = lifecycle_json["error"].as_str().unwrap_or_default();
    assert!(
        error.contains("operator signal"),
        "the lifecycle record's `error` must name the operator's signal as the cause, not an \
         endpoint failure: {error:?}"
    );

    // (#2462 review) And the same message must reach the OPERATOR'S SCREEN,
    // not only the archive. The exit-130 path force-exits before `main`'s
    // own `Error: ...` printing runs, so a correct `lifecycle.json` next to
    // a blank stderr is a real, measured failure mode — it is what the first
    // cut of this fix actually shipped.
    let stderr_text = fs::read_to_string(&stderr_path).unwrap();
    assert!(
        stderr_text.contains("operator signal"),
        "a signal-interrupted lab run must PRINT why it stopped, not exit 130 in silence: \
         {stderr_text:?}"
    );
    assert!(
        !stderr_text.contains("curl exit -1"),
        "stderr must not blame the endpoint for darkmux killing its own child: {stderr_text:?}"
    );
}

/// (#2463) `kill <pid>` (SIGTERM) on `darkmux mission propose` blocked
/// mid-dispatch, BEFORE the operator ever sees a proposal to
/// approve/reject/regenerate, must exit within 5s and leave no `curl`
/// process still holding the stub connection open. This is the exact
/// window #2463 named: `--start` only reaches `mission_launch::launch`
/// (which arms ITS OWN guard) from `persist_and_maybe_start`, well AFTER
/// this compiler dispatch has already run — so before this fix, a signal
/// here (the dispatch itself, the interactive approve/reject/regenerate
/// prompt, or any regenerate pass) killed the process outright with no
/// guard installed at all, even on an invocation that would look fully
/// guarded once `--start` eventually reached `launch()`.
///
/// `mission-compiler`'s real role manifest grants `tool_palette.allow:
/// ["read"]`, which routes a remote-resolving dispatch through the
/// agentic-remote CONTAINER path (#1187) rather than the light
/// single-shot hosted `curl` path the other SIGTERM tests use — Docker +
/// the runtime image are out of scope for a `cargo test` proof (the same
/// reasoning the crawl-launcher note above gives for skipping ITS live
/// test). So this test overrides the operator-role tier
/// (`<DARKMUX_HOME>/roles/mission-compiler.json`) with a tool-LESS
/// manifest — same id, same `role_family`/`escalation_contract`, empty
/// `tool_palette.allow` — which `load_roles()`'s user-fills-first merge
/// picks up ahead of the builtin, landing `dispatch_compiler`'s
/// hardcoded `"mission-compiler"` dispatch on the SAME light single-shot
/// hosted path `dialectic-judge` exercises above. No sibling `.md` file
/// is written, so `load_role_prompt_for` falls back to the embedded
/// `mission-compiler.md` prompt unchanged.
///
/// (#2463 review) **Name the divergence honestly: the tool grant is not an
/// incidental field, it IS the path selector.** `role_wants_agentic_remote`
/// (`dispatch_internal.rs`) forks on exactly `!tool_palette.allow.
/// is_empty()`, so with the REAL manifest every production `mission
/// propose` — local model or remote endpoint — takes the CONTAINER path,
/// and this test takes the `curl` one. What this test therefore proves is
/// that `arm()` + the reap watchdog are installed and load-bearing at this
/// call site (both mutation-proven). The container half of the same call
/// site is not proven HERE; it reduces to the trajectory tailer's
/// `interrupt::is_set()` poll + `kill_all` (`dispatch_internal.rs`'s
/// tailer loop), which #2131 pins for the launcher paths. A change to that
/// tailer will not turn this test red.
#[test]
fn mission_propose_sigterm_before_the_operator_decision_reaps_curl() {
    let stub = HangingStubServer::start();

    let home = TempDir::new().unwrap();
    let flows = TempDir::new().unwrap();
    let os_home = TempDir::new().unwrap();

    let profiles_path = home.path().join("profiles.json");
    fs::write(&profiles_path, hanging_endpoint_profiles_json(stub.port)).unwrap();

    let roles_dir = home.path().join("roles");
    fs::create_dir_all(&roles_dir).unwrap();
    let role_json = r#"{
        "id": "mission-compiler",
        "description": "test override (#2463): tool-less mission-compiler for the SIGTERM proof",
        "tool_palette": { "allow": [], "deny": ["edit", "write", "exec", "process"] },
        "escalation_contract": "bail-with-explanation",
        "role_family": "utility"
    }"#;
    fs::write(roles_dir.join("mission-compiler.json"), role_json).unwrap();

    let input_path = home.path().join("intent.txt");
    fs::write(&input_path, "build a thing").unwrap();

    let mut child = darkmux_std_cmd()
        .env("HOME", os_home.path())
        .env("DARKMUX_HOME", home.path())
        .env("DARKMUX_FLOWS_DIR", flows.path())
        .env("DARKMUX_PROFILES", &profiles_path)
        .args(["mission", "propose", "--from-file"])
        .arg(&input_path)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("spawning darkmux mission propose");
    let pid = child.id();

    assert!(
        stub.wait_for_a_connection(std::time::Duration::from_secs(20)),
        "mission propose never reached a dispatch call to the stub server within 20s"
    );
    assert!(
        child.try_wait().unwrap().is_none(),
        "mission propose must still be running (blocked on the hanging compiler dispatch) before SIGTERM"
    );

    let kill_status =
        std::process::Command::new("kill").args(["-TERM", &pid.to_string()]).status().expect("running kill -TERM");
    assert!(kill_status.success(), "kill -TERM itself must succeed");

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    let exit_status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "darkmux mission propose did not exit within 5s of SIGTERM (#2463 regression)"
        );
        std::thread::sleep(std::time::Duration::from_millis(50));
    };
    assert!(!exit_status.success(), "a signal-interrupted mission propose must not exit 0");

    assert!(
        stub.wait_for_a_connection_to_close(std::time::Duration::from_secs(3)),
        "no `curl` connection to the stub server was ever torn down — a child process survived \
         the parent (#2463 regression)"
    );

    assert_no_surviving_remote_curl(child.id(), "mission-propose");
}

/// (#2463) `kill <pid>` (SIGTERM) on `darkmux radio "<text>"` blocked
/// mid-dispatch (a real `curl` call to an endpoint that never answers)
/// must: exit within 5s and leave no `curl` process still holding the
/// stub connection open. `darkmux radio` is a genuinely different
/// dispatch shape from `dispatch`/`lab run`/`mission propose` above: its
/// routing seat (`radio::dispatch_router_call`) goes through
/// `dispatch_local_single_shot` — the container-free direct-HTTP primitive
/// (#1698 Packet B), not `crew::dispatch::dispatch`'s container-or-remote
/// fork — which still falls through to the SAME `dispatch_remote` light
/// single-shot hosted `curl` call when the resolved profile targets a
/// remote endpoint (`radio-router`'s own role manifest is already
/// tool-less, `tool_palette.allow: []`, so no role override is needed the
/// way `mission_propose`'s test above needed one). Before #2463, this path
/// had no signal handling at all — a caught SIGTERM here just killed the
/// process outright (default disposition) and orphaned the `curl` child.
#[test]
fn radio_sigterm_mid_dispatch_reaps_curl() {
    let stub = HangingStubServer::start();

    let home = TempDir::new().unwrap();
    let flows = TempDir::new().unwrap();
    let os_home = TempDir::new().unwrap();

    let profiles_path = home.path().join("profiles.json");
    fs::write(&profiles_path, hanging_endpoint_profiles_json(stub.port)).unwrap();

    let mut child = darkmux_std_cmd()
        .env("HOME", os_home.path())
        .env("DARKMUX_HOME", home.path())
        .env("DARKMUX_FLOWS_DIR", flows.path())
        .env("DARKMUX_PROFILES", &profiles_path)
        .args(["radio", "reboot the router please"])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("spawning darkmux radio");
    let pid = child.id();

    assert!(
        stub.wait_for_a_connection(std::time::Duration::from_secs(20)),
        "darkmux radio never reached a dispatch call to the stub server within 20s"
    );
    assert!(
        child.try_wait().unwrap().is_none(),
        "darkmux radio must still be running (blocked on the hanging router dispatch) before SIGTERM"
    );

    let kill_status =
        std::process::Command::new("kill").args(["-TERM", &pid.to_string()]).status().expect("running kill -TERM");
    assert!(kill_status.success(), "kill -TERM itself must succeed");

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    let exit_status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "darkmux radio did not exit within 5s of SIGTERM (#2463 regression)"
        );
        std::thread::sleep(std::time::Duration::from_millis(50));
    };
    assert!(!exit_status.success(), "a signal-interrupted radio dispatch must not exit 0");

    assert!(
        stub.wait_for_a_connection_to_close(std::time::Duration::from_secs(3)),
        "no `curl` connection to the stub server was ever torn down — a child process survived \
         the parent (#2463 regression)"
    );

    assert_no_surviving_remote_curl(child.id(), "radio");
}

/// (#2477) A best-effort SIGKILL, at drop time, on a `mission launch`
/// child this test spawned only INDIRECTLY (`darkmux radio` spawns it,
/// never this test) and drives against a stub server that never answers —
/// so if the forwarding under proof here regresses, the child keeps
/// running against that hang for its own default (3600s) step timeout. A
/// live pid at drop time, whether from a genuine regression or a panicked
/// assertion mid-test, must not sit in the background for an hour.
/// Cleanup only, never part of the proof (the proof is the mission's own
/// terminal record, read before this guard ever drops).
///
/// **Why it re-checks the command line before signalling.** This test does
/// not own the pid the way `Child::id()` owns one: by drop time `darkmux
/// radio` has exited, so the launcher is an orphan reparented to `launchd`
/// — which reaps it the moment it exits, freeing the pid for reuse. A bare
/// `kill -KILL <remembered pid>` here could therefore land on an unrelated
/// process on the developer's own machine, with their privileges. Killing
/// only a pid whose CURRENT command line still names this test's unique
/// config id closes that to a window in which a recycled pid would have to
/// be running a command containing `radio-forward-signal-test`.
struct KillOnDrop {
    pid: Option<u32>,
    marker: &'static str,
}

impl Drop for KillOnDrop {
    fn drop(&mut self) {
        let Some(pid) = self.pid else { return };
        if !cmdline_of(pid).is_some_and(|c| c.contains(self.marker)) {
            return;
        }
        let _ = std::process::Command::new("kill").args(["-KILL", &pid.to_string()]).status();
    }
}

/// The current command line of `pid`, or `None` if it is gone. `ps` prints
/// nothing (and a non-zero status) for a pid that does not exist, which is
/// the "already gone" answer every caller here wants.
fn cmdline_of(pid: u32) -> Option<String> {
    let out = std::process::Command::new("ps").args(["-o", "command=", "-p", &pid.to_string()]).output().ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if s.is_empty() {
        None
    } else {
        Some(s)
    }
}

/// Poll for the direct child of `parent_pid` whose command line contains
/// `marker` — the `mission launch` process `darkmux radio` spawns, which
/// this test never gets a `Child` handle to directly (only `radio_cli.rs`'s
/// own `spawn_mission_launch` does).
///
/// **Parentage first, marker second, and the order matters.** An earlier
/// cut searched by `pgrep -f <marker>` alone. That finds a matching process
/// anywhere on the machine — including one belonging to a SIBLING checkout
/// of this repo running this same test concurrently, which is an ordinary
/// state on this developer's machine. It would then have been that other
/// run's launcher this test measured and (via [`KillOnDrop`]) killed.
/// `pgrep -P` restricts the search to processes this test's own `darkmux
/// radio` actually forked, so the answer can only ever be ours.
fn find_child_pid_of(parent_pid: u32, marker: &str, timeout: std::time::Duration) -> Option<u32> {
    let deadline = std::time::Instant::now() + timeout;
    loop {
        if let Ok(out) = std::process::Command::new("pgrep").args(["-P", &parent_pid.to_string()]).output() {
            let s = String::from_utf8_lossy(&out.stdout);
            let found = s
                .lines()
                .filter_map(|l| l.trim().parse::<u32>().ok())
                .find(|pid| cmdline_of(*pid).is_some_and(|c| c.contains(marker)));
            if found.is_some() {
                return found;
            }
        }
        if std::time::Instant::now() >= deadline {
            return None;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
}

/// Poll `mission.json` until its `status` reaches `want`, or `timeout`
/// elapses; returns the LAST value read either way, so a failing assertion
/// can print what was actually on disk.
///
/// **Why a poll and not a single read.** The child's finalize is
/// deliberately asynchronous to radio's own exit (`radio_cli.rs`'s
/// `forward_signal_and_wait` bounds only how long RADIO waits, and never
/// forces the child down when that bound expires — a `SIGKILL` there would
/// destroy the very finalize this test asserts). On a loaded machine the
/// child's `save_json` fsyncs can outlast radio's grace window, so reading
/// once at the instant radio exits races a finalize that is still in
/// flight. Polling asserts the CONTRACT (the child was told to stop and
/// therefore finalizes) rather than an incidental ordering between two
/// processes.
fn wait_for_mission_status(mission_json_path: &std::path::Path, want: &str, timeout: std::time::Duration) -> serde_json::Value {
    let deadline = std::time::Instant::now() + timeout;
    let mut last = serde_json::Value::Null;
    loop {
        if let Ok(text) = fs::read_to_string(mission_json_path) {
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(&text) {
                if v["status"] == want {
                    return v;
                }
                last = v;
            }
        }
        if std::time::Instant::now() >= deadline {
            return last;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
}

/// (#2477) The routing seat's own stub — distinct from `RespondingStubServer`
/// (a fixed "ack") because the shape it must produce is dictated by
/// `radio.rs`'s own parser (`validate_router_output`): a fenced ```json
/// block naming a `command` the catalog advertises. Always answers the SAME
/// canned decision, regardless of what's asked — this test only ever routes
/// one message to one command.
fn start_route_decision_stub(command: &str) -> u16 {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("binding the route-decision stub");
    let port = listener.local_addr().unwrap().port();
    let content = format!("```json\n{{\"command\": \"{command}\", \"args\": \"\"}}\n```");
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let content = content.clone();
            std::thread::spawn(move || {
                use std::io::{Read, Write};
                let mut buf = [0u8; 8192];
                let _ = stream.read(&mut buf);
                let body = serde_json::json!({
                    "choices": [{ "message": { "content": content } }],
                    "usage": { "prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2 }
                })
                .to_string();
                let _ = stream.write_all(
                    format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n\
                         Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    )
                    .as_bytes(),
                );
                let _ = stream.flush();
            });
        }
    });
    port
}

/// (#2477) `kill <pid>` (SIGTERM), TARGETED at `darkmux radio`'s OWN pid —
/// never the foreground process group — while it has already routed and
/// spawned `darkmux mission launch <config>` as a child, and that child is
/// itself blocked mid-dispatch (a real `curl` call to a stub endpoint that
/// never answers).
///
/// This is the exact gap #2463 named and left for its own test: #2463 made
/// radio itself honor a targeted kill (exit within 5s instead of hanging
/// for the launch's whole duration), but the LAUNCHED CHILD was left to its
/// own signal handling — a targeted kill on radio's pid never reaches it
/// (unlike a real Ctrl-C, which hits the whole foreground group), so the
/// mission kept running and finalized on its own schedule, long after radio
/// itself had already exited.
///
/// The behavior this proves: radio forwards its own caught signal to the
/// child BEFORE exiting, so the child's own `LaunchFinalizeGuard`
/// (`launch_guard.rs`) runs and writes a terminal record — the mission
/// reaches `finalized` (never left `active`), with its phase `abandoned`
/// (never left `active` either). Two DIFFERENT profiles keep the routing
/// call (radio-router, unmapped -> `default_profile`) and the launched
/// dispatch (`dialectic-judge`, pinned via the step's OWN `profile_name`)
/// pointed at two DIFFERENT stub servers, so the router call can answer
/// immediately (routing this test's message to the launch target) while the
/// LAUNCHED dispatch hangs (giving this test a real mid-dispatch window to
/// signal against) — without either dispatch racing the other's server.
#[test]
fn radio_sigterm_forwards_to_the_launched_child_which_finalizes() {
    let route_port = start_route_decision_stub("radio-forward-signal-test");
    let hang = HangingStubServer::start();

    let home = TempDir::new().unwrap();
    let flows = TempDir::new().unwrap();
    let os_home = TempDir::new().unwrap();

    let profiles_path = home.path().join("profiles.json");
    fs::write(
        &profiles_path,
        format!(
            r#"{{
                "profiles": {{
                    "route-stub": {{
                        "models": [
                            {{"id": "stub-model", "n_ctx": 8000, "endpoint": {{"url": "http://127.0.0.1:{route_port}"}}}}
                        ]
                    }},
                    "hang-stub": {{
                        "models": [
                            {{"id": "stub-model", "n_ctx": 8000, "endpoint": {{"url": "http://127.0.0.1:{}"}}}}
                        ]
                    }}
                }},
                "default_profile": "route-stub"
            }}"#,
            hang.port
        ),
    )
    .unwrap();

    let config_dir = home.path().join("mission-configs");
    fs::create_dir_all(&config_dir).unwrap();
    // Advertised via `panel` (so radio's catalog names it) and classified
    // `Launch` (so it spawns a `mission launch` subprocess, not an
    // in-process ephemeral run) by having a `dispatch.internal` step — the
    // SAME shape `mission_launch_generic_sigterm_mid_dispatch_finalizes_and_reaps_curl`
    // above proves finalizes correctly on a direct SIGTERM. `dialectic-judge`
    // is tool-less, so it takes the light single-shot hosted `curl` path
    // (no Docker, no image). Its OWN `profile_name` pins it to the "hang"
    // stub, independent of `default_profile` (which the routing call uses).
    let config_json = r#"{
        "id": "radio-forward-signal-test",
        "name": "Radio Forward Signal Test",
        "schema_version": "3.4",
        "panel": { "description": "test-only launch target for #2477's forwarding proof" },
        "phases": [{
            "id": "p1",
            "tasks": [{
                "id": "t1",
                "steps": [{
                    "id": "s1",
                    "kind": "dispatch.internal",
                    "config": { "role_id": "dialectic-judge", "message": "hang please", "profile_name": "hang-stub" }
                }]
            }]
        }]
    }"#;
    fs::write(config_dir.join("radio-forward-signal-test.json"), config_json).unwrap();

    let mut radio_child = darkmux_std_cmd()
        .env("HOME", os_home.path())
        .env("DARKMUX_HOME", home.path())
        .env("DARKMUX_FLOWS_DIR", flows.path())
        .env("DARKMUX_PROFILES", &profiles_path)
        .args(["radio", "please help me with something"])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("spawning darkmux radio");
    let radio_pid = radio_child.id();

    assert!(
        hang.wait_for_a_connection(std::time::Duration::from_secs(20)),
        "the launched mission's dispatch never reached the hang stub within 20s — either \
         routing never resolved to a Launch, or the launched child never dispatched"
    );

    // Belt-and-suspenders cleanup from here on, regardless of how the rest
    // of this test ends (a passing assertion, a panicking one, or a
    // deliberate RED run with the forwarding fix reverted).
    let launched_pid =
        find_child_pid_of(radio_pid, "radio-forward-signal-test", std::time::Duration::from_secs(5));
    let _cleanup = KillOnDrop { pid: launched_pid, marker: "radio-forward-signal-test" };
    assert!(
        launched_pid.is_some(),
        "no `mission launch radio-forward-signal-test` child of radio (pid {radio_pid}) was found \
         \u{2014} the rest of this test would be measuring nothing"
    );

    assert!(
        radio_child.try_wait().unwrap().is_none(),
        "darkmux radio must still be running (waiting on the launched child) before SIGTERM"
    );

    let kill_status =
        std::process::Command::new("kill").args(["-TERM", &radio_pid.to_string()]).status().expect("running kill -TERM");
    assert!(kill_status.success(), "kill -TERM itself must succeed");

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    let exit_status = loop {
        if let Some(status) = radio_child.try_wait().unwrap() {
            break status;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "darkmux radio did not exit within 10s of a TARGETED SIGTERM on its own pid \
             (#2477 regression — it must forward the signal and wait a bounded grace \
             window, not hang indefinitely)"
        );
        std::thread::sleep(std::time::Duration::from_millis(50));
    };
    assert!(!exit_status.success(), "a signal-interrupted radio invocation must not exit 0");

    // The core assertion: the LAUNCHED CHILD's own terminal record exists —
    // never provable by radio's own exit code, which #2463 already made
    // well-behaved without any forwarding at all.
    let missions_dir = home.path().join("missions");
    let mission_id = fs::read_dir(&missions_dir)
        .unwrap_or_else(|e| panic!("reading {}: {e}", missions_dir.display()))
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().to_string())
        .next()
        .expect("exactly one mission must have been minted by the launched child");

    let mission_json = wait_for_mission_status(
        &missions_dir.join(&mission_id).join("mission.json"),
        "finalized",
        std::time::Duration::from_secs(30),
    );
    assert_eq!(
        mission_json["status"], "finalized",
        "a targeted SIGTERM on radio's own pid must be FORWARDED to the launched child so \
         its own LaunchFinalizeGuard runs — the mission must never be left `active` just \
         because radio itself already exited (#2477): {mission_json}"
    );

    let phases_dir = missions_dir.join(&mission_id).join("phases");
    let mut saw_a_phase = false;
    for entry in fs::read_dir(&phases_dir).unwrap().filter_map(|e| e.ok()) {
        // The same filter every production reader applies (`crew::loader`):
        // an interrupted `save_json` can leave a `<id>.json.tmp` behind,
        // which is not a phase record and must not be parsed as one.
        if entry.path().extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        let phase_json: serde_json::Value = serde_json::from_str(&fs::read_to_string(entry.path()).unwrap()).unwrap();
        assert_eq!(
            phase_json["status"], "abandoned",
            "the launched child's phase must be abandoned, never left active or completed: {phase_json}"
        );
        saw_a_phase = true;
    }
    assert!(saw_a_phase, "the mint must have produced at least one phase to check");
}

/// (#2477 review) A stub endpoint that ANSWERS with an error-shaped body —
/// `dispatch_internal.rs::parse_hosted_response`'s documented contract:
/// curl exits 0 on an HTTP body carrying `{"error": {...}}` (no `-f` flag),
/// so the endpoint's own JSON is what turns this into a failed dispatch,
/// never the HTTP status line. Used below to give the launched mission's
/// step a genuine non-zero outcome without Docker or a real model.
fn start_hosted_error_stub() -> u16 {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("binding the error stub");
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            std::thread::spawn(move || {
                use std::io::{Read, Write};
                let mut buf = [0u8; 8192];
                let _ = stream.read(&mut buf);
                let body = serde_json::json!({ "error": { "message": "boom (test, #2477)" } }).to_string();
                let _ = stream.write_all(
                    format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n\
                         Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    )
                    .as_bytes(),
                );
                let _ = stream.flush();
            });
        }
    });
    port
}

/// One `darkmux radio "<text>"` invocation that routes to `mission launch
/// <config_id>`, is NEVER signaled, and runs to natural completion against
/// `dispatch_port` (dialectic-judge's own `profile_name` pin, same shape as
/// the SIGTERM test above). Returns radio's own exit status.
fn run_radio_launch_to_completion(config_id: &str, dispatch_port: u16) -> (std::process::ExitStatus, TempDir) {
    let route_port = start_route_decision_stub(config_id);

    let home = TempDir::new().unwrap();
    let flows = TempDir::new().unwrap();
    let os_home = TempDir::new().unwrap();

    let profiles_path = home.path().join("profiles.json");
    fs::write(
        &profiles_path,
        format!(
            r#"{{
                "profiles": {{
                    "route-stub": {{
                        "models": [{{"id": "stub-model", "n_ctx": 8000, "endpoint": {{"url": "http://127.0.0.1:{route_port}"}}}}]
                    }},
                    "dispatch-stub": {{
                        "models": [{{"id": "stub-model", "n_ctx": 8000, "endpoint": {{"url": "http://127.0.0.1:{dispatch_port}"}}}}]
                    }}
                }},
                "default_profile": "route-stub"
            }}"#
        ),
    )
    .unwrap();

    let config_dir = home.path().join("mission-configs");
    fs::create_dir_all(&config_dir).unwrap();
    let config_json = format!(
        r#"{{
            "id": "{config_id}",
            "name": "Radio Forward Signal Completion Test",
            "schema_version": "3.4",
            "panel": {{ "description": "test-only launch target for #2477's inverted-direction proof" }},
            "phases": [{{
                "id": "p1",
                "tasks": [{{
                    "id": "t1",
                    "steps": [{{
                        "id": "s1",
                        "kind": "dispatch.internal",
                        "config": {{ "role_id": "dialectic-judge", "message": "hi", "profile_name": "dispatch-stub" }}
                    }}]
                }}]
            }}]
        }}"#
    );
    fs::write(config_dir.join(format!("{config_id}.json")), config_json).unwrap();

    let output = darkmux_std_cmd()
        .env("HOME", os_home.path())
        .env("DARKMUX_HOME", home.path())
        .env("DARKMUX_FLOWS_DIR", flows.path())
        .env("DARKMUX_PROFILES", &profiles_path)
        .args(["radio", "please help me with something unrelated"])
        .output()
        .expect("running darkmux radio to completion");
    (output.status, home)
}

/// (#2477 review) Inverted-direction proof for the fix above: an
/// UNSIGNALLED `darkmux radio` invocation whose launched child completes on
/// its own must still return exactly the CHILD's exit code, unchanged by
/// the new forwarding branch — which must fire ONLY when `darkmux radio`
/// itself catches a signal (`darkmux_types::interrupt::is_set()`), never on
/// the ordinary `try_wait` = `Some(status)` path a clean exit already takes.
/// Two shapes, both against the SAME light single-shot hosted `curl` path
/// the SIGTERM test above uses (no Docker, no model): a dispatch that
/// SUCCEEDS (the launched mission finalizes clean, `mission launch` itself
/// exits 0) and one that FAILS (`start_hosted_error_stub`'s error-shaped
/// body, `mission launch` itself exits 1) — proving the pass-through
/// carries the real code both ways, not just a hardcoded "0 unless
/// signaled."
#[test]
fn radio_normal_run_returns_the_launched_childs_exit_code_unchanged() {
    let respond = RespondingStubServer::start();
    let (status, home) = run_radio_launch_to_completion("radio-forward-signal-completion-ok", respond.port);
    // The headline assertion FIRST. It used to sit below the mission-record
    // reads, so a mutation that made the forwarding branch fire
    // unconditionally reddened this test by panicking on a missing
    // `missions/` directory (the child was signalled before it minted
    // anything) instead of printing the exit-code message written to
    // explain exactly that failure. A red that does not say what broke is
    // most of a test's value thrown away.
    assert!(
        status.success(),
        "an unsignalled `darkmux radio` whose launched child dispatched successfully must \
         exit 0 (#2477 — the forwarding branch must never fire on a clean try_wait): {status:?}"
    );
    let missions_dir = home.path().join("missions");
    let mission_id = fs::read_dir(&missions_dir)
        .unwrap_or_else(|e| panic!("reading {}: {e}", missions_dir.display()))
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().to_string())
        .next()
        .expect("exactly one mission must have been minted");
    let mission_json: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(missions_dir.join(&mission_id).join("mission.json")).unwrap())
            .unwrap();
    assert_eq!(mission_json["status"], "finalized", "a normal run must finalize cleanly: {mission_json}");

    let error_port = start_hosted_error_stub();
    let (status, _home) = run_radio_launch_to_completion("radio-forward-signal-completion-err", error_port);
    assert_eq!(
        status.code(),
        Some(1),
        "an unsignalled `darkmux radio` whose launched child's dispatch FAILED must exit \
         with that SAME non-zero code (`mission launch` itself exits 1 on an errored step), \
         not the signal-path's 130 and not a swallowed 0 (#2477): {status:?}"
    );
}

/// (#2345 C2) `outcome_from` names the task whose last step's output the
/// launcher promotes as the `mission close` record's payload. Before this
/// fix, a typo'd `outcome_from` was refused only AFTER the whole run — the
/// close-time check in `run_summary_payload` — so a config-authoring
/// mistake on a long-running mission surfaced only once every step had
/// already dispatched. `MissionConfig::validate` now catches it
/// statically, and `mission_launch::launch` runs `validate` (and `bail!`s
/// on any `Error` finding) BEFORE minting a mission at all — so a bad
/// `outcome_from` must fail loud with NO `missions/` entry ever created.
/// `procedural.noop` needs no model/network/Docker — purely hermetic.
#[test]
fn mission_launch_outcome_from_unknown_task_refused_before_minting() {
    let home = TempDir::new().unwrap();

    let config_dir = home.path().join("mission-configs");
    fs::create_dir_all(&config_dir).unwrap();
    let config_json = r#"{
        "id": "outcome-from-typo-test",
        "name": "Outcome From Typo Test",
        "schema_version": "3.3",
        "phases": [{
            "id": "p1",
            "tasks": [{
                "id": "t1",
                "steps": [{ "id": "s1", "kind": "procedural.noop" }]
            }]
        }],
        "outcome_from": "no-such-task"
    }"#;
    fs::write(config_dir.join("outcome-from-typo-test.json"), config_json).unwrap();

    darkmux_cmd()
        .env("DARKMUX_HOME", home.path())
        .args(["mission", "launch", "outcome-from-typo-test"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("outcome_from"))
        .stderr(predicate::str::contains("no-such-task"));

    let missions_dir = home.path().join("missions");
    assert!(
        !missions_dir.is_dir() || fs::read_dir(&missions_dir).unwrap().next().is_none(),
        "a refused outcome_from must never mint a mission — no missions/ entry may exist"
    );
}

/// (#2359) `mission launch` must never leak flow records into the *process*
/// `HOME`'s `.darkmux/flows` — the actual incident (2026-09-05): four
/// synthetic reviewer-probe missions landed in the operator's real
/// `~/.darkmux/flows` because a `DARKMUX_HOME=<tmp>` launch with no
/// `DARKMUX_FLOWS_DIR` override still resolved the flows default via
/// `dirs::home_dir()` directly (`config_access::flows_dir_default`, fixed
/// alongside this test to derive from `paths::resolve(Auto)` instead —
/// see `crates/darkmux-types/src/config_access.rs`'s
/// NOTE (#2363 review): `flows_dir_honors_darkmux_home` pins the cfg(test) twin body,
/// not the production `flows_dir_default`; the production fix has NO automated pin here.
/// Its evidence is the manual run of a non-test-support binary recorded in PR #2363;
/// the available real pin is an `#[ignore]`d test (or a CI step) that builds and runs
/// the release-cfg binary with DARKMUX_HOME set and a fake HOME.
/// that fix). `HOME` is pointed at a SEPARATE tempdir here (never the
/// operator's real home) so this test can prove the negative without ever
/// touching a real machine's state.
///
/// This test runs the SAME hermetic launch twice, split into what each half
/// can actually observe through a `cargo test`-compiled binary:
///
///   - **Half 1 (`DARKMUX_FLOWS_DIR` unset)** proves the negative this
///     incident is about: nothing lands under the fake HOME's
///     `.darkmux/flows`. It can't additionally assert records DO land
///     under `<DARKMUX_HOME>/flows`, because `LocalFileSink::
///     local_sink_dir()` has its OWN older, even more defensive test-cfg
///     guard (#1355): when `DARKMUX_FLOWS_DIR` is unset, a `cfg(any(test,
///     test-support))` build (which `cargo test`'s `CARGO_BIN_EXE_darkmux`
///     always is, via feature unification with this crate's dev-deps)
///     redirects LocalFileSink to a per-PID ephemeral temp dir and never
///     calls `flows_dir()` at all — so the production default-resolution
///     fix is provably unreachable from an integration test in this shape,
///     by DESIGN, not by gap. That's *stronger* isolation than this issue
///     needs, so it's a feature, not something to route around.
///   - **Half 2 (`DARKMUX_FLOWS_DIR` explicitly set under `DARKMUX_HOME`)**
///     forces `LocalFileSink` through the real `flows_dir()` resolution and
///     proves records land exactly where told, end to end — the closest a
///     subprocess test can get to exercising the real write path.
///
/// `procedural.noop` needs no model/network/Docker — both runs are
/// hermetic and near-instant.
#[test]
fn mission_launch_flows_never_leak_into_process_home() {
    fn write_flows_root_test_config(darkmux_home: &std::path::Path) {
        let config_dir = darkmux_home.join("mission-configs");
        fs::create_dir_all(&config_dir).unwrap();
        let config_json = r#"{
            "id": "flows-root-test",
            "name": "Flows Root Test",
            "schema_version": "3.4",
            "phases": [{
                "id": "p1",
                "tasks": [{
                    "id": "t1",
                    "steps": [{ "id": "s1", "kind": "procedural.noop" }]
                }]
            }]
        }"#;
        fs::write(config_dir.join("flows-root-test.json"), config_json).unwrap();
    }

    // ── Half 1: DARKMUX_FLOWS_DIR unset — the actual incident shape ──
    {
        let darkmux_home = TempDir::new().unwrap();
        let fake_home = TempDir::new().unwrap();

        // The shape of the leak this test guards against: if `flows_dir`'s
        // default ever reaches for `dirs::home_dir()` again (and the
        // #1355 LocalFileSink test-cfg guard is ever weakened too),
        // records land here.
        let leak_target = fake_home.path().join(".darkmux").join("flows");
        fs::create_dir_all(&leak_target).unwrap();

        write_flows_root_test_config(darkmux_home.path());

        darkmux_cmd()
            .env("DARKMUX_HOME", darkmux_home.path())
            .env("HOME", fake_home.path())
            .env("DARKMUX_LMS_BIN", "/usr/bin/true")
            .env_remove("DARKMUX_FLOWS_DIR")
            .args(["mission", "launch", "flows-root-test"])
            .assert()
            .success();

        assert!(
            fs::read_dir(&leak_target).unwrap().next().is_none(),
            "no flow day-file may land under the process HOME's .darkmux/flows \
             when DARKMUX_HOME scopes the root elsewhere"
        );
    }

    // ── Half 2: DARKMUX_FLOWS_DIR set under DARKMUX_HOME — records land
    // exactly where told, proving the write path end to end ──
    {
        let darkmux_home = TempDir::new().unwrap();
        let fake_home = TempDir::new().unwrap();
        let flows_dir = darkmux_home.path().join("flows");

        write_flows_root_test_config(darkmux_home.path());

        darkmux_cmd()
            .env("DARKMUX_HOME", darkmux_home.path())
            .env("HOME", fake_home.path())
            .env("DARKMUX_LMS_BIN", "/usr/bin/true")
            .env("DARKMUX_FLOWS_DIR", &flows_dir)
            .args(["mission", "launch", "flows-root-test"])
            .assert()
            .success();

        assert!(
            flows_dir.is_dir() && fs::read_dir(&flows_dir).unwrap().next().is_some(),
            "flow records must land under DARKMUX_FLOWS_DIR, got: {}",
            flows_dir.display()
        );
        let leak_target = fake_home.path().join(".darkmux").join("flows");
        assert!(
            !leak_target.exists(),
            "no flow day-file may land under the process HOME's .darkmux/flows"
        );
    }
}

#[test]
fn mission_launch_run_on_unknown_value_refused_before_minting() {
    // (#2310 P4a review M3) The CLI-refuse-before-mint twin of
    // `mission_launch_outcome_from_unknown_task_refused_before_minting`
    // above — a `run_on` validate Error (unknown value) must refuse the
    // SAME way, before anything is minted.
    let home = TempDir::new().unwrap();

    let config_dir = home.path().join("mission-configs");
    fs::create_dir_all(&config_dir).unwrap();
    let config_json = r#"{
        "id": "run-on-typo-test",
        "name": "Run On Typo Test",
        "schema_version": "3.4",
        "phases": [{
            "id": "p1",
            "tasks": [{
                "id": "t1",
                "run_on": ["complete", "maybe"],
                "steps": [{ "id": "s1", "kind": "procedural.noop" }]
            }]
        }]
    }"#;
    fs::write(config_dir.join("run-on-typo-test.json"), config_json).unwrap();

    darkmux_cmd()
        .env("DARKMUX_HOME", home.path())
        .args(["mission", "launch", "run-on-typo-test"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("run_on"))
        .stderr(predicate::str::contains("maybe"));

    let missions_dir = home.path().join("missions");
    assert!(
        !missions_dir.is_dir() || fs::read_dir(&missions_dir).unwrap().next().is_none(),
        "a refused run_on must never mint a mission — no missions/ entry may exist"
    );
}

// ─── review-bench --funnel flag plumbing (#1222 Phase B packet 7) ─────────
//
// The funnel condition's real dispatch path needs a live LMStudio + a real
// crew registry, so these tests stay at the clap-plumbing layer: the flag
// conflicts and `requires` relationships fail loud BEFORE any dispatch is
// attempted. A live corpus run is maintainer-executed (see the doc comment
// on `run_funnel_case` in `crates/darkmux-lab/src/lab/review_bench.rs`).

// ─── review-bench --funnel end-to-end, offline (#1222 Phase B coverage) ───
//
// A funnel run whose bundler produces ZERO bundles short-circuits to a
// degenerate envelope BEFORE any probe/judge dispatch — so a full
// `review-bench --funnel` invocation over a non-TypeScript diff corpus is
// end-to-end testable with no LMStudio and no crew models loaded. These
// tests exercise the real preflight (registry load + crew resolution +
// role-prompt resolution), the per-case funnel branch, the console line,
// and the scores.json/funnels.json artifact pair.

// ── `darkmux radio` (#1698 Packet A) ─────────────────────────────────────
//
// No assert_cmd binary-level test exists for `radio`'s live routing path,
// for a reason worth recording: the built-in `review` mission config
// (`templates/builtin/mission-configs/review.json`) declares a `panel`
// block, so it is ALWAYS merged into `radio::compile_catalog`'s output
// regardless of `DARKMUX_CREW_DIR` — built-ins are embedded at compile
// time, independent of the user-tier crew dir an isolated TempDir can
// override. There is therefore no environment override that produces a
// genuinely EMPTY catalog (the fail-closed short-circuit
// `radio::route_with_empty_catalog_refuses_without_invoking_call` covers),
// and every non-empty-catalog path — a real route OR a real refusal —
// requires an actual dispatch to the `radio-router` role through a live
// LMStudio instance, which a deterministic test must never depend on
// (verified empirically while writing these tests: a real local run here
// dispatched successfully and routed `"review this for me"` to `/review`
// end-to-end — reassuring, but not something a test suite can rely on
// being true on every machine/CI run). This mirrors the codebase's
// existing precedent: `mission propose` (`src/mission_propose.rs`), the
// other CLI verb built on the same `crate::fleet::dispatch_routed`
// mechanism, likewise has no assert_cmd-level test of its own live
// dispatch — only its pure parsing/validation functions are unit tested.
// `radio`'s full contract (catalog compilation, the frozen prompt
// assembly, all five fail-closed validation paths, the dry-run decision
// shape) is covered at the function level instead, with an injected
// canned model call — see `src/radio.rs::tests` and
// `src/radio_cli.rs::tests`.

/// (#1775) The exit-status belt and the sentence that describes it must
/// agree. The pure `integrity_exit_code` is unit tested in `darkmux-flow`;
/// what is NOT reachable from there is the human output, which is where
/// the first version of this feature printed "exit status stays 0" on a
/// run that exited 2 — a legacy file and a broken file in the same
/// directory. Spawning the binary is the only way to catch that, and the
/// belt previously carried a comment conceding it was review-only.
///
/// Deliberately covers the MIXED case, not the happy one: with only a
/// legacy file present the buggy and fixed versions behave identically,
/// so a single-file test proves nothing.
#[test]
fn integrity_check_never_claims_exit_zero_on_a_run_that_exits_nonzero() {
    let tmp = tempfile::tempdir().unwrap();
    let audit = tmp.path().join("audit");
    std::fs::create_dir_all(&audit).unwrap();

    // Build one legacy file (header marker stripped -> unverifiable) and
    // one genuinely broken file (marker intact, record bytes mutated), by
    // emitting real records and then editing them the way an attacker
    // would rather than hand-rolling the chain format.
    for (day, text) in [("2026-01-01", "alpha"), ("2026-01-02", "bravo")] {
        let staging = tmp.path().join(format!("stage-{day}"));
        std::fs::create_dir_all(&staging).unwrap();
        darkmux_cmd()
            .env("DARKMUX_AUDIT_DIR", &staging)
            .args(["flow", "note", "--text", text, "--source", "orchestrator"])
            .assert()
            .success();
        let produced = std::fs::read_dir(&staging)
            .unwrap()
            .flatten()
            .map(|e| e.path())
            .find(|p| p.extension().and_then(|e| e.to_str()) == Some("jsonl"))
            .expect("flow note must write an audit file");
        let body = std::fs::read_to_string(&produced).unwrap();
        let mut lines: Vec<String> = body.lines().map(str::to_string).collect();
        assert!(lines.len() >= 2, "need a header plus a record: {body}");

        if day == "2026-01-01" {
            // Strip the format marker -> the walk downgrades to legacy and
            // content-verifies nothing.
            lines[0] = lines[0].replace(",\"hash_format\":\"prefix-blake3-v1\"", "");
            assert!(!lines[0].contains("hash_format"), "marker must be gone: {}", lines[0]);
        } else {
            // Mutate the record bytes AFTER the hash prefix -> a real break.
            let sp = lines[1].find(' ').expect("record line is `<hash> <json>`");
            let (hash, rec) = lines[1].split_at(sp);
            lines[1] = format!("{hash}{}", rec.replace("bravo", "BRAVX"));
        }
        std::fs::write(audit.join(format!("{day}.jsonl")), lines.join("\n") + "\n").unwrap();
    }

    let out = darkmux_cmd()
        .env("DARKMUX_AUDIT_DIR", &audit)
        .args(["flow", "integrity-check"])
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);

    assert_eq!(
        out.status.code(),
        Some(2),
        "a genuine break must exit 2 even beside an unverifiable file; stdout:\n{stdout}"
    );
    assert!(
        !stdout.contains("exit status stays 0"),
        "the run exited 2 — it must not print a claim that the status stays 0; stdout:\n{stdout}"
    );
    assert!(
        stdout.contains("BROKEN"),
        "the break must still be reported; stdout:\n{stdout}"
    );

    // The SAME run under --strict. The first fix for this defect missed
    // this branch: a per-file line claiming "(exit 3)" was gated on the
    // `strict` flag rather than on the computed code, so it printed the
    // wrong status beside the tamper signal while the process exited 2.
    // Non-strict coverage alone does not reach it.
    let out = darkmux_cmd()
        .env("DARKMUX_AUDIT_DIR", &audit)
        .args(["flow", "integrity-check", "--strict"])
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);

    assert_eq!(
        out.status.code(),
        Some(2),
        "a break outranks an unverifiable file under --strict too; stdout:\n{stdout}"
    );
    assert!(
        !stdout.contains("exit 3"),
        "the run exited 2 — no line may name exit 3; stdout:\n{stdout}"
    );
    assert!(
        stdout.contains("takes precedence"),
        "the unverifiable file must still be called out, naming the real code; stdout:\n{stdout}"
    );
}

/// (#2093, folded into `flow status` by #1959's flow-hooks-family
/// retirement) `darkmux flow status` wires the hooks section end-to-end:
/// parses, dispatches, and prints valid JSON naming the resolved
/// (disabled-by-default) state — a fresh `DARKMUX_HOME` has no
/// config.json, so hooks are off and no rules are configured.
#[test]
fn flow_status_json_reports_hooks_disabled_by_default() {
    let tmp = tempfile::tempdir().unwrap();
    let out = darkmux_cmd()
        .env("DARKMUX_HOME", tmp.path())
        .args(["flow", "status", "--json"])
        .output()
        .unwrap();
    assert!(out.status.success(), "stderr: {}", String::from_utf8_lossy(&out.stderr));
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).expect("valid JSON on stdout");
    assert_eq!(v["hooks"]["enabled"], serde_json::Value::Bool(false));
    assert_eq!(v["hooks"]["rules"], serde_json::json!([]));
}

/// The human-formatted form names the disabled state too, without needing
/// `--json`.
#[test]
fn flow_status_human_reports_hooks_disabled_by_default() {
    let tmp = tempfile::tempdir().unwrap();
    darkmux_cmd()
        .env("DARKMUX_HOME", tmp.path())
        .args(["flow", "status"])
        .assert()
        .success()
        .stdout(predicate::str::contains("Hooks"))
        .stdout(predicate::str::contains("enabled:      false"));
}

/// Sets a process-global env var and restores its previous value on drop.
///
/// Same shape as `crates/darkmux-serve/src/lib_tests.rs`'s `CrewDirGuard`.
/// Process env is shared by every test in this binary AND inherited by
/// every `assert_cmd` subprocess it spawns, so a test that sets one
/// without restoring it is an order-dependent flake waiting for the next
/// test to be appended below it.
struct EnvVarGuard {
    key: &'static str,
    prev: Option<std::ffi::OsString>,
}

impl EnvVarGuard {
    fn set(key: &'static str, value: impl AsRef<std::ffi::OsStr>) -> Self {
        let prev = std::env::var_os(key);
        unsafe { std::env::set_var(key, value) };
        Self { key, prev }
    }
}

impl Drop for EnvVarGuard {
    fn drop(&mut self) {
        unsafe {
            match self.prev.take() {
                Some(v) => std::env::set_var(self.key, v),
                None => std::env::remove_var(self.key),
            }
        }
    }
}

/// (#1905) End-to-end conformance for `runs.rs`'s "Two callers, one union"
/// contract: the `darkmux run list` BINARY and a direct
/// `darkmux_serve::build_runs` call must report the same runs for the same
/// on-disk state.
///
/// This is the half `crates/darkmux-serve/src/lib_tests.rs`'s
/// `run_list_verb_and_runs_handler_agree_on_the_same_fixture` explicitly
/// cannot cover: that test proves the HTTP handler does not transform rows
/// on the way out, but it never invokes the verb, so it stays green if the
/// verb stops calling the shared union and starts computing its own. This
/// test spawns the real binary, so a verb that grew a private aggregation,
/// a private filter, or a different input triple fails HERE.
///
/// Every input is pinned by env so both sides read the SAME state: the
/// subprocess gets them as env vars, and the in-process side gets them via
/// `set_var` (hence `#[serial]` — this mutates process-global env).
/// `DARKMUX_HOME` isolates config.json, and `DARKMUX_REDIS_URL` is removed
/// on both sides so the fleet input is an empty slice for each.
#[test]
#[serial_test::serial]
fn run_list_binary_agrees_with_the_shared_union_it_calls() {
    let home = TempDir::new().unwrap();
    let flows = TempDir::new().unwrap();
    let lab = TempDir::new().unwrap();
    let crew = TempDir::new().unwrap();

    // A lab run is any directory carrying one of the three marker files
    // `scan_lab_runs` matches on (`funnels.json` / `funnel-events.jsonl` /
    // `scores.json`). One is enough to make the union non-empty, which is
    // what stops this test from passing vacuously on two empty lists.
    let run_dir = lab.path().join("case-a/run1");
    fs::create_dir_all(&run_dir).unwrap();
    fs::write(run_dir.join("scores.json"), r#"{"cases":[]}"#).unwrap();

    // A MISSION too, so the fixture spans two kinds. With a single-kind
    // fixture this comparison discriminates on only one axis: a verb-side
    // filter that dropped every mission row would leave both sides at the
    // same lone lab row and pass. `spec.config_id` is deliberately not
    // `"dispatch"` (which is how a crew-of-one dispatch is distinguished,
    // #1509), so this lands as `kind: "mission"`.
    let mission_dir = crew.path().join("missions/conformance-m1");
    fs::create_dir_all(&mission_dir).unwrap();
    fs::write(
        mission_dir.join("mission.json"),
        r#"{
            "id": "conformance-m1",
            "description": "run-list conformance fixture",
            "status": "finalized",
            "phase_ids": [],
            "created_ts": 1700000000,
            "started_ts": 1700000000,
            "finalized_ts": 1700000060,
            "spec": {"config_id": "review", "inputs_fingerprint": "conformance"}
        }"#,
    )
    .unwrap();

    let out = darkmux_cmd()
        .args(["run", "list", "--json", "--all"])
        .env("DARKMUX_HOME", home.path())
        .env("DARKMUX_FLOWS_DIR", flows.path())
        .env("DARKMUX_LAB_DIR", lab.path())
        .env("DARKMUX_CREW_DIR", crew.path())
        .env_remove("DARKMUX_REDIS_URL")
        .output()
        .unwrap();
    assert!(out.status.success(), "run list --json failed: {}", String::from_utf8_lossy(&out.stderr));

    let json: serde_json::Value = serde_json::from_slice(&out.stdout)
        .unwrap_or_else(|e| panic!("run list --json emitted non-JSON ({e}): {}", String::from_utf8_lossy(&out.stdout)));
    let mut verb_ids: Vec<String> = json["runs"]
        .as_array()
        .expect("run list --json always emits a runs array")
        .iter()
        .map(|r| format!("{}/{}/{}", r["kind"].as_str().unwrap(), r["status"].as_str().unwrap(), r["id"].as_str().unwrap()))
        .collect();
    verb_ids.sort();

    // Only ONE env var is load-bearing on the in-process side, and it is
    // restored rather than leaked. `build_runs` takes the flows and lab
    // paths as ARGUMENTS, so `DARKMUX_FLOWS_DIR`/`DARKMUX_LAB_DIR` would
    // be inert here; `darkmux-types` is compiled with `test-support` in
    // this binary, so `config()` is empty and `DARKMUX_HOME` buys nothing;
    // and the fleet slice is literally `&[]`, so Redis is never consulted.
    // That leaves `DARKMUX_CREW_DIR`, which `crew_dir_override()` feeds to
    // `load_missions()` — without it the in-process half would read the
    // developer's REAL `~/.darkmux` missions and this comparison would be
    // an accident.
    //
    // The restore is not hygiene theater. This process's env is shared by
    // every test in this binary, and `#[serial]` orders this test against
    // the file's other `#[serial]` tests only — the rest run concurrently
    // in this same process. A leaked var pointing at a `TempDir` that has
    // since been deleted turns green tests red depending on declaration
    // order.
    //
    // (#2184) The CHILD side of that same problem is now handled one layer
    // down instead of per-site: every spawn in this file goes through
    // `darkmux_cmd()`, which stamps a fresh `DARKMUX_HOME` on the child.
    // Before that, this file named the binary at 135 sites and set
    // `DARKMUX_HOME` at 55 of them, so the rest inherited whatever the
    // developer's shell had (usually nothing, which resolves their real
    // `~/.darkmux`).
    let _crew_guard = EnvVarGuard::set("DARKMUX_CREW_DIR", crew.path());
    let direct = darkmux_serve::build_runs(flows.path(), Some(lab.path()), &[]);
    let mut direct_ids: Vec<String> = direct
        .iter()
        .map(|r| {
            format!(
                "{}/{}/{}",
                serde_json::to_value(r.kind).unwrap().as_str().unwrap(),
                serde_json::to_value(r.status).unwrap().as_str().unwrap(),
                r.id
            )
        })
        .collect();
    direct_ids.sort();

    // Anti-vacuity, on the axis this test actually names: the fixture must
    // produce BOTH kinds, or a same-kind filter on either side would be
    // invisible and the comparison would pass by accident.
    assert!(
        direct_ids.iter().any(|s| s.starts_with("lab/")),
        "fixture produced no lab run — the marker file is no longer recognized, and this test \
         would now pass vacuously on the lab axis: {direct_ids:?}"
    );
    assert!(
        direct_ids.iter().any(|s| s.starts_with("mission/")),
        "fixture produced no mission run — the mission record shape or crew-dir layout changed, \
         and this test would now pass vacuously on the mission axis: {direct_ids:?}"
    );
    assert_eq!(
        verb_ids, direct_ids,
        "`darkmux run list` and darkmux_serve::build_runs disagree on the SAME fixture — the \
         \"one union\" contract (#1905) is broken: the verb is aggregating, filtering, or \
         reading different inputs instead of rendering what the shared union returned"
    );
}

// ── crawl --dry-run (#1959) ──
//
// Migrated from the retired `darkmux crawl plan` verb (deleted alongside
// the standalone CLI family — see `src/cli.rs`'s Crawl retirement commit).
// The equivalent surface today is `darkmux mission launch crawl
// --dry-run`: it resolves + plans exactly the same way, mints nothing,
// and either prints the human plan table (the default) or writes the
// full plan JSON to `--param plan_out=<path>` when the assertion needs
// structured data. `--param workspace=<spec.json>` replaces the old
// positional manifest arg; the manifest/spec JSON SHAPE is unchanged
// (`SourceSpec`/`EdgeSpec` are wire-compatible with the retired
// `CorpusManifest`'s own fields).

fn git(dir: &std::path::Path, args: &[&str]) {
    let out = std::process::Command::new("git")
        .current_dir(dir)
        .args(args)
        .output()
        .unwrap_or_else(|e| panic!("running git {args:?} in {}: {e}", dir.display()));
    assert!(
        out.status.success(),
        "git {args:?} in {} failed: {}",
        dir.display(),
        String::from_utf8_lossy(&out.stderr)
    );
}

fn init_repo(dir: &std::path::Path) {
    fs::create_dir_all(dir).unwrap();
    git(dir, &["init", "-q", "-b", "main"]);
    git(dir, &["config", "user.email", "test@example.com"]);
    git(dir, &["config", "user.name", "test"]);
}

fn commit_all(dir: &std::path::Path, message: &str) {
    git(dir, &["add", "-A"]);
    git(dir, &["commit", "-q", "-m", message]);
}

/// Build the `app` consumer repo: a `.ts` file with two well-separated
/// `catch` blocks (so their windows don't merge into one site — the
/// prefilter is mechanical and must find both), a `package.json` pinning
/// `@org/lib` to `pin_range`, and a `src/uses-lib.ts` that imports it.
fn write_app_repo(dir: &std::path::Path, pin_range: &str) {
    init_repo(dir);
    fs::write(
        dir.join("package.json"),
        serde_json::json!({"name": "app", "dependencies": {"@org/lib": pin_range}}).to_string(),
    )
    .unwrap();
    fs::create_dir_all(dir.join("src")).unwrap();

    // Two catch sites separated by far more than 2x the built-in rule's
    // window (30 lines each side), so they land as two DISTINCT sites
    // rather than merging into one span.
    let mut lines = vec!["function a() {".to_string(), "  try { risky(); }".to_string(), "  catch (e) { console.error(e); }".to_string(), "}".to_string()];
    for _ in 0..80 {
        lines.push(String::new());
    }
    lines.push("function b() {".to_string());
    lines.push("  try { risky(); }".to_string());
    lines.push("  catch (e) { }".to_string()); // the bare swallow the model would flag
    lines.push("}".to_string());
    fs::write(dir.join("src/x.ts"), lines.join("\n")).unwrap();

    fs::write(
        dir.join("src/uses-lib.ts"),
        "import { thing } from '@org/lib';\nthing();\n",
    )
    .unwrap();

    commit_all(dir, "app: initial");
}

fn write_lib_repo(dir: &std::path::Path, version: &str) {
    init_repo(dir);
    fs::write(
        dir.join("package.json"),
        serde_json::json!({"name": "@org/lib", "version": version, "types": "index.d.ts"}).to_string(),
    )
    .unwrap();
    // A resolvable entry point — #1959 second-round CONSIDER 5 stops
    // emitting an edge unit when `library_surface` is empty, so a stale
    // edge test needs the library to actually have one. `.d.ts` (not
    // `.js`/`.ts`) deliberately: every built-in rule's `exclude` already
    // drops `**/*.d.ts`, so this stays invisible to the site/read rules
    // and doesn't perturb the unit counts those tests assert.
    fs::write(dir.join("index.d.ts"), "export {};\n").unwrap();
    commit_all(dir, "lib: initial");
}

/// (#1959) `SourceSpec`/`EdgeSpec`'s wire shape is unchanged from the
/// retired `CorpusManifest`'s own fields — only the file's ROLE changed
/// (a generic `WorkspaceSpec` any mission can take, not a crawl-only
/// manifest).
fn write_workspace_spec(path: &std::path::Path, root: &std::path::Path, app: &std::path::Path, lib: &std::path::Path) {
    let spec = serde_json::json!({
        "schema_version": "1.0",
        "name": "test-workspace",
        "root": root.to_string_lossy(),
        "sources": [
            {"id": "app", "path": app.to_string_lossy(), "ref": "main"},
            {"id": "lib", "path": lib.to_string_lossy(), "ref": "main"}
        ],
        "edges": [{"consumer": "app", "library": "lib", "package": "@org/lib"}],
        "rules": ["swallowed-error", "doc-contradicts-code", "stale-consumer"]
    });
    fs::write(path, spec.to_string()).unwrap();
}

/// (#2301) `mission launch crawl --dry-run` on the REAL built-in config.
///
/// The retired launcher's own dry run planned in-process and printed a plan
/// table; the six tests that asserted on that table went with it (their
/// subject — the plan shape — is covered directly by `darkmux-lab`'s
/// `crawl::plan` unit tests, which run the same planner without a
/// subprocess). What a dry run proves NOW is the generic thing: which
/// graph this launch would mint.
fn crawl_dry_run(home: &TempDir, spec_path: &std::path::Path, extra: &[&str]) -> std::process::Output {
    let mut args: Vec<String> = vec![
        "mission".into(),
        "launch".into(),
        "crawl".into(),
        "--dry-run".into(),
        "--param".into(),
        format!("workspace={}", spec_path.display()),
    ];
    args.extend(extra.iter().map(|s| s.to_string()));
    darkmux_cmd()
        .args(&args)
        .env("DARKMUX_HOME", home.path())
        .output()
        .expect("mission launch crawl --dry-run runs")
}

#[test]
fn crawl_dry_run_prints_the_graph_the_launch_would_mint() {
    let workdir = TempDir::new().unwrap();
    let app = workdir.path().join("app");
    let lib = workdir.path().join("lib");
    write_app_repo(&app, "^1.0.0");
    write_lib_repo(&lib, "1.2.0");
    let spec_path = workdir.path().join("workspace.json");
    write_workspace_spec(&spec_path, workdir.path(), &app, &lib);

    let out = crawl_dry_run(&workdir, &spec_path, &[]);
    assert!(out.status.success(), "stderr: {}", String::from_utf8_lossy(&out.stderr));
    let stdout = String::from_utf8_lossy(&out.stdout);
    // The document IS the crawl now: its three phases and its per-rule
    // tracks are what a dry run shows.
    for want in ["Plan", "Crawl", "Summarize", "crawl.plan", "crawl.unit", "crawl.summary"] {
        assert!(stdout.contains(want), "dry run never mentioned `{want}`:\n{stdout}");
    }
    // (#2302) The create-mods phase ships OFF, so it is PRUNED at mint and
    // the default graph ends at Summarize — never drawn gray.
    assert!(
        !stdout.contains("Create mods"),
        "the create-mod task is `enabled: false`, so its phase is pruned:\n{stdout}"
    );
    assert!(
        stdout.trim_end().ends_with("[crawl.summary]"),
        "and the default graph still ENDS at the summary, whose output is the close payload:\n{stdout}"
    );
    // Nothing was minted, and nothing was planned.
    assert!(!workdir.path().join("missions").exists(), "a dry run mints nothing");
}

/// (#2302) The same document with the create-mod task turned ON — a
/// user-tier copy of `crawl.json`, which is exactly how an operator enables
/// it — shows the template in the graph it would mint.
#[test]
fn crawl_dry_run_shows_the_create_mod_template_when_a_user_tier_copy_enables_it() {
    let workdir = TempDir::new().unwrap();
    let app = workdir.path().join("app");
    let lib = workdir.path().join("lib");
    write_app_repo(&app, "^1.0.0");
    write_lib_repo(&lib, "1.2.0");
    let spec_path = workdir.path().join("workspace.json");
    write_workspace_spec(&spec_path, workdir.path(), &app, &lib);

    // The built-in document, copied to the user tier with ONE field flipped.
    let mut doc: serde_json::Value = serde_json::from_str(include_str!(
        "../templates/builtin/mission-configs/crawl.json"
    ))
    .expect("the built-in crawl config parses");
    let phases = doc["phases"].as_array_mut().unwrap();
    let create_mods = phases.last_mut().expect("the create-mods phase");
    assert_eq!(create_mods["id"], serde_json::json!("create-mods"));
    create_mods["tasks"][0]["enabled"] = serde_json::json!(true);
    let config_dir = workdir.path().join("mission-configs");
    fs::create_dir_all(&config_dir).unwrap();
    fs::write(config_dir.join("crawl.json"), doc.to_string()).unwrap();

    let out = crawl_dry_run(&workdir, &spec_path, &[]);
    assert!(out.status.success(), "stderr: {}", String::from_utf8_lossy(&out.stderr));
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("Create mods"), "the enabled phase is in the graph:\n{stdout}");
    assert!(stdout.contains("dispatch.internal"), "and its step is a coder dispatch:\n{stdout}");
}

#[test]
fn crawl_dry_run_prunes_the_rules_the_launch_did_not_select() {
    let workdir = TempDir::new().unwrap();
    let app = workdir.path().join("app");
    let lib = workdir.path().join("lib");
    write_app_repo(&app, "^1.0.0");
    write_lib_repo(&lib, "1.2.0");
    let spec_path = workdir.path().join("workspace.json");
    write_workspace_spec(&spec_path, workdir.path(), &app, &lib);

    let all = crawl_dry_run(&workdir, &spec_path, &[]);
    let one = crawl_dry_run(&workdir, &spec_path, &["--param", "rules=swallowed-error"]);
    assert!(one.status.success(), "stderr: {}", String::from_utf8_lossy(&one.stderr));
    let all_out = String::from_utf8_lossy(&all.stdout).to_string();
    let one_out = String::from_utf8_lossy(&one.stdout).to_string();

    assert!(all_out.contains("unnamed-predicate"), "the full graph has every rule:\n{all_out}");
    assert!(one_out.contains("swallowed-error"), "the selected rule survives:\n{one_out}");
    assert!(
        !one_out.contains("unnamed-predicate"),
        "a deselected rule is PRUNED, never drawn gray:\n{one_out}"
    );
    // Mutation guard: if the selection stopped pruning, these would match.
    assert_ne!(all_out, one_out, "`--param rules=` must change the minted graph");
}

/// (#2310 P4c-2 item 4 — proven structurally) A SYNTHETIC config (not
/// `review`, not any name the launcher's source recognizes) proves the
/// `"ignored": true` input-declaration flag is honored by ANY config, not
/// detected by matching `config.id`. Both legs: an ignored input supplied
/// warns naming the input and reason; a LIVE (non-ignored) input supplied
/// never warns, even on the same launch.
#[test]
fn an_ignored_input_flag_warns_on_any_config_never_by_id() {
    let home = TempDir::new().unwrap();
    let config_dir = home.path().join("mission-configs");
    fs::create_dir_all(&config_dir).unwrap();
    fs::write(
        config_dir.join("synthetic-ignored-test.json"),
        serde_json::json!({
            "id": "synthetic-ignored-test",
            "name": "Synthetic ignored-input test",
            "schema_version": "3.4",
            "inputs": [
                {"name": "legacy_flag", "required": false, "ignored": true, "ignored_reason": "kept for CLI parity only, never read"},
                {"name": "live_flag", "required": false}
            ],
            // (#2386 MF1) `live_flag` must be genuinely LIVE — referenced by
            // a step — or `check_supplied_inert_inputs` refuses a launch
            // that supplies it, which is exactly the behavior this test's
            // OTHER half (`with_ignored`) exists to prove is now wired up.
            // An empty `phases: []` predates that refusal and made this
            // test's own `live_flag` inert by construction.
            "phases": [{"id": "p", "tasks": [{
                "id": "t",
                "steps": [{"id": "t-step", "kind": "procedural.noop", "config": {"note": "{{live_flag}}"}}]
            }]}]
        })
        .to_string(),
    )
    .unwrap();

    let with_ignored = darkmux_cmd()
        .env("DARKMUX_HOME", home.path())
        .args([
            "mission",
            "launch",
            "synthetic-ignored-test",
            "--dry-run",
            "--param",
            "legacy_flag=anything",
        ])
        .output()
        .expect("mission launch synthetic-ignored-test --dry-run runs");
    assert!(with_ignored.status.success(), "stderr: {}", String::from_utf8_lossy(&with_ignored.stderr));
    let stderr = String::from_utf8_lossy(&with_ignored.stderr);
    assert!(
        stderr.contains("legacy_flag") && stderr.contains("ignored") && stderr.contains("never read"),
        "an ignored input supplied must warn, naming the input and its reason: {stderr}"
    );

    let with_live = darkmux_cmd()
        .env("DARKMUX_HOME", home.path())
        .args([
            "mission",
            "launch",
            "synthetic-ignored-test",
            "--dry-run",
            "--param",
            "live_flag=anything",
        ])
        .output()
        .expect("mission launch synthetic-ignored-test --dry-run runs");
    assert!(with_live.status.success(), "stderr: {}", String::from_utf8_lossy(&with_live.stderr));
    let live_stderr = String::from_utf8_lossy(&with_live.stderr);
    assert!(
        !live_stderr.contains("ignored"),
        "a LIVE (non-ignored) input must never warn: {live_stderr}"
    );
}

// ─── (#2310 P4c-2 review MUST FIX) placeholder typos refused before
// minting, on both --dry-run and a real (stubbed) launch ─────────────────

/// No `missions/` dir at all, OR one with zero entries — either is "nothing
/// minted".
fn assert_nothing_minted(home: &TempDir) {
    let missions = home.path().join("missions");
    if !missions.exists() {
        return;
    }
    let entries: Vec<_> = fs::read_dir(&missions).unwrap().filter_map(|e| e.ok()).collect();
    assert!(entries.is_empty(), "expected nothing minted, found: {entries:?}");
}

fn write_synthetic_static_typo_config(home: &TempDir) {
    let config_dir = home.path().join("mission-configs");
    fs::create_dir_all(&config_dir).unwrap();
    fs::write(
        config_dir.join("synthetic-static-typo.json"),
        serde_json::json!({
            "id": "synthetic-static-typo",
            "name": "Synthetic static-typo test",
            "schema_version": "3.4",
            "inputs": [{"name": "workspace", "required": true}],
            "phases": [{"id": "p", "tasks": [{
                "id": "t",
                "steps": [{"id": "t-step", "kind": "procedural.noop", "config": {"note": "{{workspac}}"}}]
            }]}]
        })
        .to_string(),
    )
    .unwrap();
}

fn write_synthetic_grow_typo_config(home: &TempDir) {
    let config_dir = home.path().join("mission-configs");
    fs::create_dir_all(&config_dir).unwrap();
    fs::write(
        config_dir.join("synthetic-grow-typo.json"),
        serde_json::json!({
            "id": "synthetic-grow-typo",
            "name": "Synthetic grow-typo test",
            "schema_version": "3.4",
            "inputs": [{"name": "workspace", "required": true}, {"name": "intent_file", "required": false}],
            "phases": [
                {"id": "produce", "tasks": [{
                    "id": "producer",
                    "steps": [{"id": "producer-step", "kind": "procedural.noop", "config": {}}]
                }]},
                {"id": "consume", "tasks": [{
                    "id": "consumer",
                    "depends_on": ["producer"],
                    "grow": {"from": "producer", "items": "units", "id": "{{item.id}}",
                             "config": {"intent_file": "{{intent_fle}}"}},
                    "steps": [{"id": "consumer-step", "kind": "procedural.noop", "config": {}}]
                }]}
            ]
        })
        .to_string(),
    )
    .unwrap();
}

/// A STATIC step's typo, `--dry-run`. Before the MUST FIX, `--dry-run`
/// never called `interpret` at all and exited 0 regardless.
#[test]
fn a_static_placeholder_typo_is_refused_on_dry_run() {
    let home = TempDir::new().unwrap();
    write_synthetic_static_typo_config(&home);
    let out = darkmux_cmd()
        .env("DARKMUX_HOME", home.path())
        .args([
            "mission",
            "launch",
            "synthetic-static-typo",
            "--dry-run",
            "--param",
            "workspace=/tmp/ws.json",
        ])
        .output()
        .expect("mission launch synthetic-static-typo --dry-run runs");
    assert!(!out.status.success(), "a typo'd placeholder must refuse the dry run");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("workspac") && stderr.contains("t-step"), "{stderr}");
    assert_nothing_minted(&home);
}

/// The SAME static typo, a REAL launch (stubbed before dispatch — the
/// config dispatches nothing regardless, `procedural.noop`). Before the
/// MUST FIX, `interpret` refused this, but only AFTER the mission
/// directory (mission.json, phase records) was already written.
#[test]
fn a_static_placeholder_typo_is_refused_before_any_mint_on_a_real_launch() {
    let home = TempDir::new().unwrap();
    write_synthetic_static_typo_config(&home);
    let out = darkmux_cmd()
        .env("DARKMUX_HOME", home.path())
        .env("DARKMUX_LMS_BIN", "/usr/bin/true")
        .args(["mission", "launch", "synthetic-static-typo", "--param", "workspace=/tmp/ws.json"])
        .output()
        .expect("mission launch synthetic-static-typo runs");
    assert!(!out.status.success(), "a typo'd placeholder must refuse the real launch");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("workspac") && stderr.contains("t-step"), "{stderr}");
    assert_nothing_minted(&home);
}

/// A `grow.config` typo, `--dry-run`.
#[test]
fn a_grow_config_placeholder_typo_is_refused_on_dry_run() {
    let home = TempDir::new().unwrap();
    write_synthetic_grow_typo_config(&home);
    let out = darkmux_cmd()
        .env("DARKMUX_HOME", home.path())
        .args([
            "mission",
            "launch",
            "synthetic-grow-typo",
            "--dry-run",
            "--param",
            "workspace=/tmp/ws.json",
        ])
        .output()
        .expect("mission launch synthetic-grow-typo --dry-run runs");
    assert!(!out.status.success(), "a typo'd grow.config placeholder must refuse the dry run");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("intent_fle") && stderr.contains("grow.config"), "{stderr}");
    assert_nothing_minted(&home);
}

/// The SAME `grow.config` typo, a REAL launch. Before the MUST FIX, this
/// typo was refused only AFTER the ENTIRE `produce` phase had already run
/// for real (a real `producer-step` dispatch), at the `consume` phase's
/// growth boundary — abandoning a mission that had already done real work.
#[test]
fn a_grow_config_placeholder_typo_is_refused_before_any_mint_on_a_real_launch() {
    let home = TempDir::new().unwrap();
    write_synthetic_grow_typo_config(&home);
    let out = darkmux_cmd()
        .env("DARKMUX_HOME", home.path())
        .env("DARKMUX_LMS_BIN", "/usr/bin/true")
        .args(["mission", "launch", "synthetic-grow-typo", "--param", "workspace=/tmp/ws.json"])
        .output()
        .expect("mission launch synthetic-grow-typo runs");
    assert!(!out.status.success(), "a typo'd grow.config placeholder must refuse the real launch");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("intent_fle") && stderr.contains("grow.config"), "{stderr}");
    assert_nothing_minted(&home);
}

/// (#2310 P4c-2 review round 2, item a) An EMBEDDED placeholder naming a
/// DECLARED but UNCOLLECTED OPTIONAL input — `"label": "run-{{tag}}"` where
/// `tag` is optional and the operator never set it. `interpret`'s own
/// `substitute_step_config` already refuses this (round-1's item 2), but
/// only from `interpret`, which runs AFTER `--dry-run`'s short-circuit and
/// AFTER minting — the exact "caught too late" shape the MUST FIX closed
/// for an undeclared name. This config declares `tag` and never sets it.
fn write_synthetic_embedded_optional_config(home: &TempDir) {
    let config_dir = home.path().join("mission-configs");
    fs::create_dir_all(&config_dir).unwrap();
    fs::write(
        config_dir.join("synthetic-embedded-optional.json"),
        serde_json::json!({
            "id": "synthetic-embedded-optional",
            "name": "Synthetic embedded-optional test",
            "schema_version": "3.4",
            "inputs": [
                {"name": "workspace", "required": true},
                {"name": "tag", "required": false}
            ],
            "phases": [{"id": "p", "tasks": [{
                "id": "t",
                "steps": [{"id": "t-step", "kind": "procedural.noop",
                           "config": {"workspace": "{{workspace}}", "label": "run-{{tag}}"}}]
            }]}]
        })
        .to_string(),
    )
    .unwrap();
}

/// `--dry-run`. Before this fix: `--dry-run` never called `interpret` at
/// all, so an embedded reference to an unset optional input exited 0,
/// silent — the same gap `--dry-run` had for an undeclared name before the
/// MUST FIX.
#[test]
fn an_embedded_placeholder_naming_an_unset_optional_input_is_refused_on_dry_run() {
    let home = TempDir::new().unwrap();
    write_synthetic_embedded_optional_config(&home);
    let out = darkmux_cmd()
        .env("DARKMUX_HOME", home.path())
        .args([
            "mission",
            "launch",
            "synthetic-embedded-optional",
            "--dry-run",
            "--param",
            "workspace=/tmp/ws.json",
        ])
        .output()
        .expect("mission launch synthetic-embedded-optional --dry-run runs");
    assert!(!out.status.success(), "an embedded reference to an unset optional input must refuse the dry run");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("tag") && stderr.contains("t-step"), "{stderr}");
    assert_nothing_minted(&home);
}

/// The SAME config, a REAL launch. Before this fix: `interpret` refused
/// this — but only AFTER the mission directory (mission.json, phase
/// records) was already minted, exactly the shape the MUST FIX closed for
/// an undeclared name.
#[test]
fn an_embedded_placeholder_naming_an_unset_optional_input_is_refused_before_any_mint_on_a_real_launch() {
    let home = TempDir::new().unwrap();
    write_synthetic_embedded_optional_config(&home);
    let out = darkmux_cmd()
        .env("DARKMUX_HOME", home.path())
        .env("DARKMUX_LMS_BIN", "/usr/bin/true")
        .args(["mission", "launch", "synthetic-embedded-optional", "--param", "workspace=/tmp/ws.json"])
        .output()
        .expect("mission launch synthetic-embedded-optional runs");
    assert!(!out.status.success(), "an embedded reference to an unset optional input must refuse the real launch");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("tag") && stderr.contains("t-step"), "{stderr}");
    assert_nothing_minted(&home);
}

/// (#2310 P4c-2 item 0 — the P4c-1 BLOCKER, proven) A REAL (non-dry-run)
/// `review` launch — stubbed before dispatch via `DARKMUX_LMS_BIN=/usr/
/// bin/true`, so this proves MINTING, not model behavior — must leave no
/// literal `{{` in ANY minted step's config, in both the statically
/// declared `plan-<rule>-step`s (`{{workspace}}`/`{{diff_file}}`) and the
/// GROWN `unit-<rule>-step`s (`{{intent_file}}`, wired into `grow.config`
/// by this same packet). Before this packet, `crawl_plan_step_overrides`
/// only ever substituted `{{workspace}}`, and only for `kind ==
/// "crawl.plan"` — `review.json`'s `plan.sites` steps got NO
/// substitution on a real launch, so this is the fix's own regression
/// test, not incidental coverage.
#[test]
fn review_real_launch_leaves_no_literal_braces_in_any_minted_step_config() {
    let workdir = TempDir::new().unwrap();
    let home = TempDir::new().unwrap();
    let app = workdir.path().join("app");
    write_app_repo(&app, "^1.0.0");
    // A real `git diff` between the empty tree and the fully-populated
    // `write_app_repo` state — the tree materializes at HEAD (the "after"
    // state), and the diff's added lines are exactly that content, so
    // `DiffSource`'s tree-agreement check passes and the bare
    // `catch (e) { }` in `src/x.ts` is a genuine `swallowed-error` hit.
    let empty_tree = std::process::Command::new("git")
        .current_dir(&app)
        .args(["hash-object", "-t", "tree", "/dev/null"])
        .output()
        .unwrap();
    let empty_tree_sha = String::from_utf8_lossy(&empty_tree.stdout).trim().to_string();
    let diff_out = std::process::Command::new("git")
        .current_dir(&app)
        .args(["diff", &empty_tree_sha, "HEAD"])
        .output()
        .unwrap();
    assert!(diff_out.status.success(), "{}", String::from_utf8_lossy(&diff_out.stderr));
    let diff_text = String::from_utf8_lossy(&diff_out.stdout).to_string();
    assert!(
        diff_text.contains("catch (e) { }"),
        "the fixture's bare (swallowed) catch must appear in the diff: {diff_text}"
    );

    let spec_path = workdir.path().join("workspace.json");
    fs::write(
        &spec_path,
        serde_json::json!({
            "name": "review-real-launch",
            "sources": [{"id": "app", "path": app.to_string_lossy(), "ref": "main"}]
        })
        .to_string(),
    )
    .unwrap();
    let diff_path = workdir.path().join("d.diff");
    fs::write(&diff_path, &diff_text).unwrap();
    let intent_path = workdir.path().join("intent.md");
    fs::write(&intent_path, "Fix the swallowed catch in src/x.ts.").unwrap();

    let out = darkmux_cmd()
        .env("DARKMUX_HOME", home.path())
        .env("DARKMUX_LMS_BIN", "/usr/bin/true")
        .args([
            "mission",
            "launch",
            "review",
            "--param",
            &format!("workspace={}", spec_path.display()),
            "--param",
            &format!("diff_file={}", diff_path.display()),
            "--param",
            "rules=swallowed-error",
            "--param",
            &format!("intent_file={}", intent_path.display()),
        ])
        .output()
        .expect("mission launch review runs");
    let combined =
        format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));

    let mission_dir = one_mission_dir(&home);
    let steps_dir = mission_dir.join("steps");
    assert!(steps_dir.exists(), "no steps/ dir was written:\n{combined}");

    let mut all_configs: Vec<(String, serde_json::Value)> = Vec::new();
    let mut saw_plan_step = false;
    let mut saw_grown_unit_step = false;
    for phase_entry in fs::read_dir(&steps_dir).unwrap() {
        let phase_dir = phase_entry.unwrap().path();
        if !phase_dir.is_dir() {
            continue;
        }
        for step_entry in fs::read_dir(&phase_dir).unwrap() {
            let path = step_entry.unwrap().path();
            let step: serde_json::Value = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
            // Keyed on `kind`, not `id` — the id carries the composed real
            // PHASE id as its prefix (`substitute_id`), not the document's
            // bare `plan-`/`unit-` prefix.
            if step["kind"] == serde_json::json!("plan.sites") {
                saw_plan_step = true;
                assert_eq!(
                    step["config"]["workspace"],
                    serde_json::json!(spec_path.to_string_lossy()),
                    "the plan step's `{{{{workspace}}}}` must resolve to the real path: {step}"
                );
                assert_eq!(
                    step["config"]["diff_file"],
                    serde_json::json!(diff_path.to_string_lossy()),
                    "the plan step's `{{{{diff_file}}}}` must resolve to the real path: {step}"
                );
                assert!(
                    step["config"].get("head_sha").is_none(),
                    "an unset optional input's placeholder key must be OMITTED, not an empty string: {step}"
                );
            }
            if step["kind"] == serde_json::json!("crawl.unit") && step["config"].get("grown_from").is_some() {
                saw_grown_unit_step = true;
                assert_eq!(
                    step["config"]["intent_file"],
                    serde_json::json!(intent_path.to_string_lossy()),
                    "a GROWN unit step's `{{{{intent_file}}}}` must resolve too, not just static tasks: {step}"
                );
            }
            all_configs.push((path.to_string_lossy().to_string(), step["config"].clone()));
        }
    }
    assert!(saw_plan_step, "no `plan-*` step was minted:\n{combined}");
    assert!(
        saw_grown_unit_step,
        "the fixture's bare catch must have grown at least one `unit-swallowed-error-*` step:\n{combined}"
    );

    let mut braces: Vec<String> = Vec::new();
    for (path, config) in &all_configs {
        darkmux_crew::mission_config::find_unsubstituted_braces(config, path, &mut braces);
    }
    assert!(braces.is_empty(), "literal `{{{{` survived minting:\n{}", braces.join("\n"));
}

/// (#2310 P4c-2b self-QA) A REAL (non-dry-run) `review` launch — stubbed
/// before dispatch via `DARKMUX_LMS_BIN=/usr/bin/true`, same discipline the
/// sibling brace test above uses — proving the `deliver` phase this packet
/// adds actually MINTS and RUNS to completion end to end through the real
/// CLI, and that the `--param emit=<path>` this packet wires reaches
/// `deliver.github_review`'s config and gets a real file written to it. A
/// stub dispatch produces no real findings AND makes the grown unit step
/// end `Error` (`/usr/bin/true`'s trivial reply isn't the JSON envelope
/// the runtime expects), so the payload's own `mode` is `"degraded"`
/// (#2310 P4c-2b PR #2357 review MUST FIX D) — this test is about the
/// WIRING (the phase existing, running, and writing its file) and about
/// the mode being HONEST about the error, not about model behavior;
/// `crates/darkmux-lab/src/crawl/plan.rs`'s own
/// `review_fixture_plans_every_rule_and_delivers_one_comment_per_form`
/// unit test is what proves the full RENDER with real (stubbed) records.
#[test]
fn review_real_launch_runs_the_deliver_phase_and_writes_the_emit_file() {
    let workdir = TempDir::new().unwrap();
    let home = TempDir::new().unwrap();
    let flows = TempDir::new().unwrap();
    let app = workdir.path().join("app");
    write_app_repo(&app, "^1.0.0");
    let empty_tree = std::process::Command::new("git")
        .current_dir(&app)
        .args(["hash-object", "-t", "tree", "/dev/null"])
        .output()
        .unwrap();
    let empty_tree_sha = String::from_utf8_lossy(&empty_tree.stdout).trim().to_string();
    let diff_out = std::process::Command::new("git")
        .current_dir(&app)
        .args(["diff", &empty_tree_sha, "HEAD"])
        .output()
        .unwrap();
    assert!(diff_out.status.success(), "{}", String::from_utf8_lossy(&diff_out.stderr));
    let diff_text = String::from_utf8_lossy(&diff_out.stdout).to_string();

    let spec_path = workdir.path().join("workspace.json");
    fs::write(
        &spec_path,
        serde_json::json!({
            "name": "review-deliver-launch",
            "sources": [{"id": "app", "path": app.to_string_lossy(), "ref": "main"}]
        })
        .to_string(),
    )
    .unwrap();
    let diff_path = workdir.path().join("d.diff");
    fs::write(&diff_path, &diff_text).unwrap();
    let emit_path = workdir.path().join("review-payload.json");

    let out = darkmux_cmd()
        .env("DARKMUX_HOME", home.path())
        .env("DARKMUX_FLOWS_DIR", flows.path())
        .env("DARKMUX_LMS_BIN", "/usr/bin/true")
        .args([
            "mission",
            "launch",
            "review",
            "--param",
            &format!("workspace={}", spec_path.display()),
            "--param",
            &format!("diff_file={}", diff_path.display()),
            "--param",
            "rules=swallowed-error",
            "--param",
            &format!("emit={}", emit_path.display()),
            "--param",
            "attribution=darkmux review self-QA proof",
        ])
        .output()
        .expect("mission launch review runs");
    let combined = format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));

    let mission_dir = one_mission_dir(&home);
    let steps_dir = mission_dir.join("steps");
    assert!(steps_dir.exists(), "no steps/ dir was written:\n{combined}");

    let mut saw_gather_step = false;
    let mut saw_deliver_step = false;
    for phase_entry in fs::read_dir(&steps_dir).unwrap() {
        let phase_dir = phase_entry.unwrap().path();
        if !phase_dir.is_dir() {
            continue;
        }
        for step_entry in fs::read_dir(&phase_dir).unwrap() {
            let path = step_entry.unwrap().path();
            let step: serde_json::Value = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
            if step["kind"] == serde_json::json!("records.gather") {
                saw_gather_step = true;
                assert_eq!(
                    step["status"], serde_json::json!("complete"),
                    "records.gather must have run to completion: {step}"
                );
            }
            if step["kind"] == serde_json::json!("deliver.github_review") {
                saw_deliver_step = true;
                assert_eq!(
                    step["status"], serde_json::json!("complete"),
                    "deliver.github_review must have run to completion: {step}"
                );
                assert_eq!(
                    step["config"]["emit"],
                    serde_json::json!(emit_path.to_string_lossy()),
                    "the deliver step's `{{{{emit}}}}` must resolve to the real path: {step}"
                );
                assert_eq!(
                    step["config"]["attribution"],
                    serde_json::json!("darkmux review self-QA proof"),
                    "the deliver step's `{{{{attribution}}}}` must resolve too: {step}"
                );
            }
        }
    }
    assert!(saw_gather_step, "no `records.gather` step was minted or run:\n{combined}");
    assert!(saw_deliver_step, "no `deliver.github_review` step was minted or run:\n{combined}");

    assert!(emit_path.exists(), "deliver.github_review must have written its emit file:\n{combined}");
    let payload: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&emit_path).unwrap()).expect("the emit file is valid DeliverOutcome JSON");
    // (#2310 P4c-2b PR #2357 review MUST FIX D, fixed here per the
    // review's own instruction: "fix the CLI test that accepts noop ||
    // review to assert the exact mode per scenario") `DARKMUX_LMS_BIN=
    // /usr/bin/true` stubs the unit's dispatch to a trivial, non-JSON
    // reply, so the grown `unit-swallowed-error` step ends `Error` — a
    // run with zero findings AND a real error is `"degraded"`, never the
    // `"noop"` a clean run gets. The scope line names the errored unit.
    assert_eq!(payload["mode"], serde_json::json!("degraded"), "{payload}");
    let body = payload["review"]["body"].as_str().unwrap();
    assert!(body.contains("Errored:") && body.contains("unit-swallowed-error"), "{payload}");

    // (#2310 fix-loop C2 / C2-4) `review.json` declares
    // `outcome_from: "deliver"`, which is what gives the
    // exit-1-on-delivery-failure rule a production subscriber (the rule is
    // scoped to an EXPLICIT declaration, never the positional guess).
    // THIS run is the other side of that pair: the units errored, so the
    // run is `Degraded` — but the DELIVERY itself succeeded, and a
    // partially-constrained run that still shipped its review exits 0.
    // (`review_real_launch_exits_non_zero_when_the_deliver_step_errors`
    // is the failing half.)
    assert_eq!(out.status.code(), Some(0), "{combined}");
    let close = flow_actions(&flows)
        .into_iter()
        .find(|r| r["action"] == serde_json::json!("mission close"))
        .expect("a mission close record");
    // (#2310 fix-loop E2) The close payload IS the delivery's verdict. The
    // deliver step's output is a `{mode, summary, emit}` object — declaring
    // `outcome_from: "deliver"` and then emitting a bare PATH promoted
    // nothing, so a run whose whole purpose is to deliver a review closed
    // with a null payload and the verdict was reachable only by opening the
    // emit file. Pinned here because this is the seam where the kind's
    // output contract meets `outcome_from`.
    assert_eq!(
        close["payload"]["mode"],
        serde_json::json!("degraded"),
        "the run's verdict is promoted into the close payload: {close}"
    );
    assert_eq!(
        close["payload"]["emit"],
        serde_json::json!(emit_path.to_string_lossy()),
        "and the emit destination is still carried, as a field: {close}"
    );
    assert!(
        close["payload"]["summary"].as_str().unwrap_or_default().contains("rules reviewed"),
        "the scope line rides it too: {close}"
    );
    let deliver_output = probe_steps(&mission_dir)
        .into_iter()
        .find(|(id, _)| id.contains("deliver") && !id.contains("gather"))
        .map(|(_, (_, output))| output)
        .expect("a deliver step");
    let deliver_output: serde_json::Value =
        serde_json::from_str(&deliver_output).expect("the deliver step's output is a promotable JSON object");
    assert_eq!(
        deliver_output["emit"],
        serde_json::json!(emit_path.to_string_lossy()),
        "the deliver step's output names its emit destination"
    );
}

/// (#2310 fix-loop C2 / C2-4) The other half of declaring `outcome_from`:
/// when the DELIVERING task itself errors, the run exits non-zero even
/// though earlier steps completed. An `emit` path under a directory that
/// does not exist makes `deliver.github_review`'s own write fail, which is
/// the narrowest way to fail exactly that step.
#[test]
fn review_real_launch_exits_non_zero_when_the_deliver_step_errors() {
    let workdir = TempDir::new().unwrap();
    let home = TempDir::new().unwrap();
    let flows = TempDir::new().unwrap();
    let missing_source = workdir.path().join("does-not-exist");
    let spec_path = workdir.path().join("workspace.json");
    fs::write(
        &spec_path,
        serde_json::json!({
            "name": "review-deliver-error",
            "sources": [{"id": "app", "path": missing_source.to_string_lossy(), "ref": "main"}]
        })
        .to_string(),
    )
    .unwrap();
    let diff_path = workdir.path().join("d.diff");
    fs::write(
        &diff_path,
        "diff --git a/src/x.ts b/src/x.ts\n--- a/src/x.ts\n+++ b/src/x.ts\n@@ -1,2 +1,3 @@\n function f() {\n+  g();\n }\n",
    )
    .unwrap();
    // Under a directory that was never created — `fs::write` fails.
    let emit_path = workdir.path().join("no-such-dir").join("review-payload.json");

    let out = darkmux_cmd()
        .env("DARKMUX_HOME", home.path())
        .env("DARKMUX_FLOWS_DIR", flows.path())
        .env("DARKMUX_LMS_BIN", "/usr/bin/true")
        .args([
            "mission",
            "launch",
            "review",
            "--param",
            &format!("workspace={}", spec_path.display()),
            "--param",
            &format!("diff_file={}", diff_path.display()),
            "--param",
            "rules=swallowed-error",
            "--param",
            &format!("emit={}", emit_path.display()),
        ])
        .output()
        .expect("mission launch review runs");
    let combined =
        format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));

    assert!(!emit_path.exists(), "precondition: the emit write must have failed\n{combined}");
    let mission_dir = one_mission_dir(&home);
    let steps = probe_steps(&mission_dir);
    let deliver = steps
        .iter()
        .find(|(id, _)| id.ends_with("deliver-step-2") || id.contains("deliver-github"))
        .map(|(id, v)| (id.clone(), v.clone()));
    assert!(
        steps.values().any(|(status, _)| status == "error"),
        "the deliver step must end Error: {steps:#?}\n{combined}"
    );
    assert_eq!(
        out.status.code(),
        Some(1),
        "a failed DELIVERY must not exit 0 — deliver step: {deliver:?}\n{combined}"
    );
}

/// (#2310 P4c-2b PR #2357 review MUST FIX C, the reviewer's own live
/// probe reproduced) A `workspace` naming a source path that does not
/// exist makes the `plan-swallowed-error` STEP itself end `Error` (the
/// materialize fails before any hunk is ever read) — before this fix,
/// `src/mission_launch.rs::grow_phase` `bail!`ed the WHOLE launch the
/// moment `unit-swallowed-error`'s grow tried to read that errored plan
/// step's output, so `summarize`/`create-mods`/`deliver` never even
/// minted and no `emit` file was ever written: "an errored run renders
/// nothing", the exact defect DESIGN.md's own retirement finding names.
/// After the fix: `unit-swallowed-error` grows ZERO copies (not an
/// abort), the phase loop continues, and `deliver` — `run_on:
/// ["complete","error"]`, no `depends_on` at all (naming a grow TEMPLATE
/// in `depends_on` is refused at validation) — runs regardless, off
/// phase-ordering alone (the same mechanism `crawl.json`'s `summarize`
/// phase already relies on). Its scope line names the failed rule via
/// `not_attempted`, and `mode` is `"degraded"` (MUST FIX D) since the
/// plan step itself errored.
#[test]
fn review_real_launch_survives_a_plan_phase_error_and_still_delivers() {
    let workdir = TempDir::new().unwrap();
    let home = TempDir::new().unwrap();
    let flows = TempDir::new().unwrap();
    // Deliberately a path that does not exist — `workspace_spec::
    // materialize` fails on it, so `plan-swallowed-error-step` (a STATIC,
    // always-minted task — not a grow template) ends `Error`.
    let missing_source = workdir.path().join("does-not-exist");
    let spec_path = workdir.path().join("workspace.json");
    fs::write(
        &spec_path,
        serde_json::json!({
            "name": "review-plan-error-launch",
            "sources": [{"id": "app", "path": missing_source.to_string_lossy(), "ref": "main"}]
        })
        .to_string(),
    )
    .unwrap();
    // A diff naming the same (nonexistent) source — `plan.sites` reads
    // `diff_file` before it ever needs the tree, but `materialize` runs
    // first and is what actually fails.
    let diff_path = workdir.path().join("d.diff");
    fs::write(
        &diff_path,
        "diff --git a/src/x.ts b/src/x.ts\n--- a/src/x.ts\n+++ b/src/x.ts\n@@ -1,2 +1,3 @@\n function f() {\n+  g();\n }\n",
    )
    .unwrap();
    let emit_path = workdir.path().join("review-payload.json");

    let out = darkmux_cmd()
        .env("DARKMUX_HOME", home.path())
        .env("DARKMUX_FLOWS_DIR", flows.path())
        .env("DARKMUX_LMS_BIN", "/usr/bin/true")
        .args([
            "mission",
            "launch",
            "review",
            "--param",
            &format!("workspace={}", spec_path.display()),
            "--param",
            &format!("diff_file={}", diff_path.display()),
            "--param",
            "rules=swallowed-error",
            "--param",
            &format!("emit={}", emit_path.display()),
        ])
        .output()
        .expect("mission launch review runs");
    let combined = format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));

    // MUST FIX C's own claim: the launch does NOT abort — `deliver` still
    // mints and runs, and the emit file still exists.
    assert!(
        emit_path.exists(),
        "the deliver phase must still run and write its emit file even though the plan step errored:\n{combined}"
    );
    let payload: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&emit_path).unwrap()).expect("the emit file is valid DeliverOutcome JSON");
    assert_eq!(payload["mode"], serde_json::json!("degraded"), "{payload}");
    let body = payload["review"]["body"].as_str().unwrap();
    assert!(
        body.contains("Not attempted:") && body.contains("swallowed-error"),
        "the scope line must name the rule whose plan step failed: {payload}"
    );

    // (#2310 P4c-2b PR #2357 round-2 review item 3) The `review` phase
    // owns `unit-swallowed-error`'s grow template, whose producer
    // (`plan-swallowed-error`) errored — the phase must close `Abandoned`,
    // never `Complete` ("nothing failed" would be a lie here), and the
    // `mission.grow` record for that template must carry the DISTINCT
    // `producer_errored` reason, never the legit-zero `grew_nothing`.
    let mission_dir = one_mission_dir(&home);
    let mission_id = mission_dir.file_name().unwrap().to_string_lossy().to_string();
    let review_phase = phase_record(&mission_dir, &format!("{mission_id}-review"));
    assert_eq!(
        review_phase["status"],
        serde_json::json!("abandoned"),
        "a phase whose grow producer errored must close Abandoned, not Complete: {review_phase}"
    );

    let grow_records: Vec<serde_json::Value> = flow_actions(&flows)
        .into_iter()
        .filter(|r| r["action"] == serde_json::json!("mission.grow") && r["payload"]["task_template"] == serde_json::json!("unit-swallowed-error"))
        .collect();
    assert!(!grow_records.is_empty(), "no mission.grow record for unit-swallowed-error's template");
    assert!(
        grow_records.iter().any(|r| r["payload"]["reason"] == serde_json::json!("producer_errored")
            && r["payload"]["producer_step"].as_str().is_some_and(|s| s.contains("plan-swallowed-error"))),
        "expected a producer_errored mission.grow record naming the plan step: {grow_records:?}"
    );
    // (#2310 swarm F / S2-2) `producer_status` is the STABLE lowercase
    // `NodeStatus` vocabulary — the same strings a persisted `Step.status`
    // carries — never a `Debug` rendering (`"Error"`), and `source` is the
    // producing STEP's id, never an absolute host path on the fleet stream.
    let errored_grow = grow_records
        .iter()
        .find(|r| r["payload"]["reason"] == serde_json::json!("producer_errored"))
        .expect("a producer_errored mission.grow record");
    let status = errored_grow["payload"]["producer_status"].as_str().unwrap_or("");
    assert!(
        matches!(status, "error" | "abandoned"),
        "producer_status must be the stable lowercase NodeStatus string, got {status:?}: {errored_grow}"
    );
    let source = errored_grow["payload"]["source"].as_str().unwrap_or("");
    assert_eq!(
        source,
        errored_grow["payload"]["producer_step"].as_str().unwrap_or(""),
        "`source` names the producing step, the same id `producer_step` carries: {errored_grow}"
    );
    assert!(
        !source.is_empty() && !source.starts_with('/'),
        "`mission.grow.source` must never be an absolute host path (lab/fleet boundary): {errored_grow}"
    );
}

#[test]
fn a_real_crawl_plan_step_grows_one_task_per_planned_unit() {
    // (#2301) The whole plan→grow seam end to end through the CLI, with a
    // REAL `crawl.plan` step over a real git fixture and a
    // `procedural.noop` standing in for the unit dispatch (no model, no
    // container — the live crawl is the operator's own proof).
    let home = TempDir::new().unwrap();
    let flows = TempDir::new().unwrap();
    let app = home.path().join("app");
    write_app_repo(&app, "^1.0.0");

    let spec_path = home.path().join("workspace.json");
    fs::write(
        &spec_path,
        serde_json::json!({
            "name": "grow-e2e",
            "root": home.path().join("ws").to_string_lossy(),
            "sources": [{"id": "app", "path": app.to_string_lossy(), "ref": "main"}]
        })
        .to_string(),
    )
    .unwrap();

    let config_dir = home.path().join("mission-configs");
    fs::create_dir_all(&config_dir).unwrap();
    fs::write(
        config_dir.join("crawl-e2e.json"),
        serde_json::json!({
            "id": "crawl-e2e",
            "name": "Crawl E2E",
            "schema_version": "3.2",
            "inputs": [
                {"name": "workspace", "required": true},
                {"name": "no_fetch", "required": false}
            ],
            "phases": [
                {"id": "plan", "tasks": [{
                    "id": "plan-swallowed-error",
                    "steps": [{"id": "plan-swallowed-error-step", "kind": "crawl.plan",
                               "config": {"rule": "swallowed-error", "workspace": "{{workspace}}", "no_fetch": "{{no_fetch}}"}}]
                }]},
                {"id": "units", "tasks": [{
                    "id": "unit",
                    "depends_on": ["plan-swallowed-error"],
                    "grow": {"from": "plan-swallowed-error", "items": "units", "id": "{{item.id}}",
                             "config": {"plan": "{{from.output}}", "unit": "{{item.id}}"}},
                    "steps": [{"id": "unit-step", "kind": "procedural.noop", "config": {}}]
                }]}
            ]
        })
        .to_string(),
    )
    .unwrap();

    let out = darkmux_cmd()
        .env("DARKMUX_HOME", home.path())
        .env("DARKMUX_FLOWS_DIR", flows.path())
        .env("DARKMUX_LMS_BIN", "/usr/bin/true")
        .args([
            "mission",
            "launch",
            "crawl-e2e",
            "--param",
            &format!("workspace={}", spec_path.display()),
            "--param",
            "no_fetch=true",
        ])
        .output()
        .expect("mission launch crawl-e2e runs");
    assert!(
        out.status.success(),
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("grew"), "the plan's units grew into tasks:\n{stdout}");

    // The plan landed under the run, WRAPPED, and the graph report says
    // what grew from it.
    let mission_dir = one_mission_dir(&home);
    let plan_path = mission_dir.join("plan").join("swallowed-error.json");
    let plan: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&plan_path).expect("the plan was written")).unwrap();
    assert_eq!(plan["kind"], serde_json::json!("crawl.plan"), "the plan is a typed output envelope");
    assert!(!plan["hash"].as_str().unwrap_or("").is_empty(), "and carries its body's digest");
    let units = plan["body"]["units"].as_array().expect("the body holds the units");
    assert!(!units.is_empty(), "the fixture's swallowed catch was planned: {plan}");

    let report: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(mission_dir.join("graph-report.json")).unwrap()).unwrap();
    let grown = report["grown"].as_array().expect("growth is recorded");
    assert_eq!(grown[0]["items"].as_u64().unwrap() as usize, units.len());
    assert_eq!(grown[0]["minted"].as_array().unwrap().len(), units.len(), "one task per unit");
    assert_eq!(grown[0]["from"], serde_json::json!("plan-swallowed-error"));

    // (#2310 P4c-2 review item 6 — proven) `--param no_fetch=true` must
    // actually REACH the step now that this config declares it and wires
    // it via `{{no_fetch}}` — before this fix the param was a silent
    // no-op (undeclared, unreferenced), a regression the old
    // `crawl_plan_step_overrides` masked by injecting it unconditionally.
    let plan_step_path = fs::read_dir(mission_dir.join("steps"))
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .find(|p| p.file_name().unwrap().to_string_lossy().ends_with("-plan"))
        .expect("a plan phase dir under steps/")
        .join(format!("{}-plan-swallowed-error-step.json", mission_dir.file_name().unwrap().to_string_lossy()));
    let plan_step: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&plan_step_path).unwrap()).unwrap();
    assert_eq!(
        plan_step["config"]["no_fetch"],
        serde_json::json!("true"),
        "the declared `{{{{no_fetch}}}}` placeholder must resolve to the real param: {plan_step}"
    );
}

/// (#2302) `mission config show crawl --json` says which tasks the mint
/// would prune. The create-mod task ships OFF; every other task in the document
/// declares no gate at all and runs.
#[test]
fn mission_config_show_names_the_create_mod_task_as_disabled() {
    let home = TempDir::new().unwrap();
    let out = darkmux_cmd()
        .env("DARKMUX_HOME", home.path())
        .env("DARKMUX_LMS_BIN", "/usr/bin/true")
        .args(["mission", "config", "show", "crawl", "--json"])
        .output()
        .expect("mission config show crawl --json runs");
    assert!(out.status.success(), "stderr: {}", String::from_utf8_lossy(&out.stderr));
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).expect("--json is JSON");
    let phases = v["phases"].as_array().expect("phases");
    let create_mods = phases.last().expect("the create-mods phase");
    assert_eq!(create_mods["id"], serde_json::json!("create-mods"));
    assert_eq!(
        create_mods["tasks"][0]["enabled"],
        serde_json::json!(false),
        "the gate is VISIBLE, not inferred from the description: {create_mods}"
    );
    for phase in &phases[..phases.len() - 1] {
        for task in phase["tasks"].as_array().unwrap() {
            assert_eq!(
                task["enabled"],
                serde_json::Value::Null,
                "a task that declares no gate reports none: {task}"
            );
        }
    }
}

/// (#2302) The FOLLOW-ON seam end to end through the CLI: a producer whose
/// wrapped output names findings, and a template that grows one
/// `dispatch.internal` step per finding carrying that finding's key in
/// `config.brief_refs`.
///
/// The finding store is left EMPTY on purpose. `brief_refs` resolution
/// refuses a key that addresses no stored record, and it refuses BEFORE any
/// model or container work, so the refusal is both the proof that the
/// substituted ref reached `dispatch.internal` and the proof of the
/// readiness guard the create-mods phase leans on — with no docker, no model
/// and no dispatch anywhere in this test.
#[test]
#[serial_test::serial]
fn a_template_grows_one_dispatch_per_finding_carrying_its_key_in_brief_refs() {
    let home = TempDir::new().unwrap();
    let flows = TempDir::new().unwrap();
    let findings = TempDir::new().unwrap();

    // The producer's output: the SAME envelope `crawl.summary` writes — a
    // wrapped body whose top-level `finding_refs` is the array to map over.
    let summary_body = serde_json::json!({
        "findings": 2,
        "finding_refs": [
            {"key": "sess-a/1", "id": "sess-a-1", "file": "src/a.ts", "line": 7,
             "rule": "unnamed-predicate", "tree_root": home.path().to_string_lossy()},
            {"key": "sess-a/2", "id": "sess-a-2", "file": "src/b.ts", "line": 9,
             "rule": "unnamed-predicate", "tree_root": home.path().to_string_lossy()},
        ],
    });
    let summary_output = serde_json::json!({
        "schema_version": "1.0",
        "kind": "crawl.summary",
        "producer": {"mission": "m", "task": "summary", "step": "summary-step", "machine_id": "t"},
        "produced_at": "2026-09-04T00:00:00Z",
        "body": summary_body,
    })
    .to_string();

    // An ENDPOINT profile, so nothing here is a local placement and the
    // scheduler never tries to make a model resident. Nothing is ever
    // called at this URL: `brief_refs` resolution refuses first.
    let profiles = home.path().join("profiles.json");
    fs::write(
        &profiles,
        serde_json::json!({
            "schema_version": "1.5",
            "default_profile": "stub",
            "profiles": {"stub": {"models": [
                {"id": "stub-model", "n_ctx": 8000, "endpoint": {"url": "http://127.0.0.1:9"}}
            ]}}
        })
        .to_string(),
    )
    .unwrap();

    let config_dir = home.path().join("mission-configs");
    fs::create_dir_all(&config_dir).unwrap();
    fs::write(
        config_dir.join("follow-on-e2e.json"),
        serde_json::json!({
            "id": "follow-on-e2e",
            "name": "Follow-on E2E",
            "schema_version": "3.2",
            "inputs": [],
            "phases": [
                {"id": "summarize", "tasks": [{
                    "id": "summary",
                    "steps": [{"id": "summary-step", "kind": "procedural.noop",
                               "config": {"output": summary_output}}]
                }]},
                {"id": "follow-on", "tasks": [{
                    "id": "follow-on",
                    "role_id": "coder",
                    "depends_on": ["summary"],
                    "grow": {"from": "summary", "items": "finding_refs", "id": "{{item.id}}",
                             "config": {
                                 "workdir": "{{item.tree_root}}",
                                 "brief_refs": [{"kind": "finding", "key": "{{item.key}}"}],
                                 "message": "make the change the finding describes",
                                 "profile_name": "stub",
                                 "skip_preflight": true
                             }},
                    "steps": [{"id": "follow-on-step", "kind": "dispatch.internal", "config": {}}]
                }]}
            ]
        })
        .to_string(),
    )
    .unwrap();

    let out = darkmux_cmd()
        .env("DARKMUX_HOME", home.path())
        .env("DARKMUX_FLOWS_DIR", flows.path())
        .env("DARKMUX_FINDINGS_DIR", findings.path())
        .env("DARKMUX_LMS_BIN", "/usr/bin/true")
        .env("DARKMUX_PROFILES", &profiles)
        .args(["mission", "launch", "follow-on-e2e"])
        .output()
        .expect("mission launch follow-on-e2e runs");
    let combined =
        format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));

    // Two tasks grew, from the summary, one per finding.
    let mission_dir = one_mission_dir(&home);
    let report: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(mission_dir.join("graph-report.json")).unwrap()).unwrap();
    let grown = report["grown"].as_array().expect("growth is recorded");
    assert_eq!(grown.len(), 1, "{report}");
    assert_eq!(grown[0]["from"], serde_json::json!("summary"), "grown from the SUMMARY: {report}");
    assert_eq!(grown[0]["items"].as_u64().unwrap(), 2);
    let minted: Vec<&str> = grown[0]["minted"].as_array().unwrap().iter().map(|v| v.as_str().unwrap()).collect();
    assert_eq!(minted.len(), 2, "{report}");
    assert!(minted[0].ends_with("follow-on-sess-a-1"), "the id is `/`-free: {report}");
    assert!(minted[1].ends_with("follow-on-sess-a-2"), "the id is `/`-free: {report}");
    assert!(minted.iter().all(|m| !m.contains('/')), "a task id is one segment: {report}");

    // Each grown step config carries the SUBSTITUTED ref and its provenance.
    let steps_dir = mission_dir.join("steps");
    let phase_dir = fs::read_dir(&steps_dir)
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .find(|p| p.file_name().unwrap().to_string_lossy().ends_with("-follow-on"))
        .expect("a follow-on phase dir under steps/");
    let mut seen: Vec<String> = Vec::new();
    let mut refusals: Vec<String> = Vec::new();
    for entry in fs::read_dir(&phase_dir).unwrap() {
        let step: serde_json::Value = serde_json::from_str(&fs::read_to_string(entry.unwrap().path()).unwrap()).unwrap();
        assert_eq!(step["kind"], serde_json::json!("dispatch.internal"));
        assert_eq!(step["status"], serde_json::json!("error"), "an unresolvable ref refuses the step: {step}");
        refusals.push(step["output"].as_str().unwrap_or_default().to_string());
        assert_eq!(step["config"]["grown_from"]["task"], serde_json::json!("summary"));
        let refs = step["config"]["brief_refs"].as_array().expect("the grown ref");
        assert_eq!(refs.len(), 1, "{step}");
        assert_eq!(refs[0]["kind"], serde_json::json!("finding"));
        seen.push(refs[0]["key"].as_str().unwrap().to_string());
        assert_eq!(
            step["config"]["workdir"],
            serde_json::json!(home.path().to_string_lossy()),
            "the finding's own tree is the follow-on's workdir: {step}"
        );
    }
    seen.sort();
    assert_eq!(seen, vec!["sess-a/1".to_string(), "sess-a/2".to_string()], "one step per finding, keyed");

    // And with nothing in the store, each step REFUSES BY NAME — before any
    // container work, which is the readiness guard stated out loud.
    assert!(
        combined.contains("2 errored"),
        "both follow-on steps must refuse, and the run must say so:\n{combined}"
    );
    let all = refusals.join("\n");
    for key in ["sess-a/1", "sess-a/2"] {
        assert!(all.contains(key), "the refusal names the key it could not resolve:\n{all}");
    }
    assert!(all.contains("no finding"), "and names the STORE it looked in:\n{all}");
    let lower = all.to_lowercase();
    assert!(!lower.contains("docker"), "nothing downstream of resolution was reached:\n{all}");
}

// ─── `finding` family (#2265) ────────────────────────────────────────────
//
// The finding record is what was observed: an event, keyed `<dispatch>/<seq>`,
// written once and never rewritten. `finding sync` is the SECOND producer —
// it replays the flow stream for anything the live tailer missed (an older
// binary, a killed process) and must be idempotent, because the tailer and it
// race by design.

/// One flow day file holding the three shapes `sync` has to tell apart: an
/// accepted `create_finding` with an emission, the pre-2026-09-03
/// `report_finding` name (historical records carry it; the stream is
/// append-only), and an accepted call from a runtime that predates FLOW
/// 1.33.0 and therefore carried no `emitted` at all.
fn write_finding_day_file(flows: &std::path::Path) {
    fs::create_dir_all(flows).unwrap();
    // `mission_id` / `phase_id` are TOP-LEVEL on a flow record; the crawl's
    // `context` is the launcher's blob (workspace / source / sha / rule / unit)
    // and carries no mission. A fixture that put the mission inside `context`
    // would test a shape the producer never emits.
    let rec = |ts: &str, sess: &str, tool: &str, seq: u64, mission: &str, emitted: Option<serde_json::Value>| {
        let mut payload = serde_json::json!({
            "tool_name": tool, "ok": true, "args": "{}",
            "context": {"unit": "u1", "rule": "unnamed-predicate", "source": "acme"},
        });
        if let Some(e) = emitted {
            payload["emitted"] = e;
            payload["emit_seq"] = serde_json::json!(seq);
        }
        serde_json::json!({
            "ts": ts, "level": "info", "category": "work", "tier": "local",
            "stage": "dispatch", "action": "dispatch.tool", "handle": "crawler",
            "session_id": sess, "model": "darkmux:qwen3.6", "machine_id": "test-machine",
            "mission_id": mission, "phase_id": format!("{mission}-crawl"),
            "payload": payload,
        })
        .to_string()
    };
    let lines = [
        rec("2026-09-03T01:00:00Z", "sess-a", "create_finding", 1, "crawl-1",
            Some(serde_json::json!({"file": "a.ts", "line": 4, "why": "unnamed operands"}))),
        rec("2026-09-03T02:00:00Z", "sess-b", "report_finding", 2, "crawl-2",
            Some(serde_json::json!({"file": "b.ts", "line": 9}))),
        // Pre-FLOW-1.33.0: no `emitted` key at all — in the stream, not a record.
        rec("2026-09-03T03:00:00Z", "sess-c", "create_finding", 3, "crawl-1", None),
    ];
    fs::write(flows.join("2026-09-03.jsonl"), lines.join("\n") + "\n").unwrap();
}

#[test]
fn finding_sync_materializes_then_is_idempotent_and_list_show_read_the_store() {
    let home = TempDir::new().unwrap();
    let flows = home.path().join("flows");
    write_finding_day_file(&flows);

    let dm = |args: &[&str]| {
        darkmux_cmd()
            .env("DARKMUX_HOME", home.path())
            .env("DARKMUX_FLOWS_DIR", &flows)
            .env("DARKMUX_LMS_BIN", "/usr/bin/true")
            .args(args)
            .output()
            .expect("darkmux runs")
    };

    // First pass: two records made, one call that cannot become one.
    let out = dm(&["finding", "sync", "--json"]);
    assert!(out.status.success(), "stderr: {}", String::from_utf8_lossy(&out.stderr));
    let v: serde_json::Value = serde_json::from_str(&String::from_utf8_lossy(&out.stdout))
        .expect("finding sync --json emits JSON");
    assert_eq!(v["created"], 2, "got: {v}");
    assert_eq!(v["present"], 0, "got: {v}");
    assert_eq!(v["skipped_no_emission"], 1, "the pre-1.33.0 call cannot become a record: {v}");
    assert_eq!(v["scanned"], 3, "got: {v}");

    // On disk where the key says, one file per finding.
    assert!(home.path().join("findings/sess-a/1/finding.json").exists());
    assert!(home.path().join("findings/sess-b/2/finding.json").exists());
    assert!(!home.path().join("findings/sess-c").exists());

    // Second pass: idempotent. Nothing new, both already present.
    let out = dm(&["finding", "sync", "--json"]);
    let v: serde_json::Value =
        serde_json::from_str(&String::from_utf8_lossy(&out.stdout)).unwrap();
    assert_eq!(v["created"], 0, "a second sync must create nothing: {v}");
    assert_eq!(v["present"], 2, "got: {v}");

    // The human output NAMES the calls that cannot become records, rather
    // than dropping them silently.
    let out = dm(&["finding", "sync"]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("no emission"), "human output must name the skip: {stdout}");

    // `finding list` reads the STORE, one line per finding, ts-ascending.
    let out = dm(&["finding", "list"]);
    assert!(out.status.success(), "stderr: {}", String::from_utf8_lossy(&out.stderr));
    let stdout = String::from_utf8_lossy(&out.stdout);
    let rows: Vec<&str> = stdout.lines().filter(|l| l.contains("sess-")).collect();
    assert_eq!(rows.len(), 2, "one line per finding: {stdout}");
    assert!(rows[0].contains("sess-a/1"), "ts-ascending, keyed: {stdout}");
    assert!(rows[0].contains("crawler"), "the proposer is named: {stdout}");

    let out = dm(&["finding", "list", "--json"]);
    let v: serde_json::Value =
        serde_json::from_str(&String::from_utf8_lossy(&out.stdout)).unwrap();
    let arr = v["findings"].as_array().expect("findings array");
    assert_eq!(arr.len(), 2, "got: {v}");
    assert_eq!(arr[0]["emitted"]["file"], "a.ts", "the emission rides whole: {v}");

    // The record carries the mission the dispatch ran under as its OWN field —
    // never read out of the launcher's context blob, which has no mission in it.
    assert_eq!(arr[0]["mission_id"], "crawl-1", "got: {v}");
    assert_eq!(arr[0]["phase_id"], "crawl-1-crawl", "got: {v}");
    assert!(
        arr[0]["context"].get("mission_id").is_none(),
        "the launcher's context stays verbatim — no mission injected into it: {v}"
    );

    // `--mission` selects on that field. THE #2288 live-proof gap: sync made
    // every record, `--rule` matched them all, and `--mission` matched none,
    // because the filter read a key `context` never holds.
    let mission_ids = |args: &[&str]| -> Vec<String> {
        let out = dm(args);
        let v: serde_json::Value =
            serde_json::from_str(&String::from_utf8_lossy(&out.stdout)).unwrap();
        v["findings"]
            .as_array()
            .expect("findings array")
            .iter()
            .map(|f| f["key"].as_str().unwrap().to_string())
            .collect()
    };
    assert_eq!(
        mission_ids(&["finding", "list", "--mission", "crawl-1", "--json"]),
        vec!["sess-a/1".to_string()],
        "--mission must return exactly that mission's findings"
    );
    assert_eq!(
        mission_ids(&["finding", "list", "--mission", "crawl-2", "--json"]),
        vec!["sess-b/2".to_string()],
    );
    assert!(
        mission_ids(&["finding", "list", "--mission", "no-such-mission", "--json"]).is_empty(),
        "an unknown mission returns none"
    );

    // `--dispatch` narrows to one dispatch. Unpinned, the filter could be
    // `.filter(|_| true)` and nothing would notice.
    assert_eq!(
        mission_ids(&["finding", "list", "--dispatch", "sess-b", "--json"]),
        vec!["sess-b/2".to_string()],
        "--dispatch must return exactly that dispatch's findings"
    );
    assert!(
        mission_ids(&["finding", "list", "--dispatch", "sess-nope", "--json"]).is_empty(),
        "an unknown dispatch returns none"
    );
    // …and the three filters compose rather than replacing each other.
    assert!(
        mission_ids(&["finding", "list", "--mission", "crawl-1", "--dispatch", "sess-b", "--json"])
            .is_empty(),
        "filters compose: sess-b is not in crawl-1"
    );

    // A filter that matches nothing must not read like an EMPTY STORE — the
    // remedy for the two is different ("your filter matched nothing" vs "run
    // sync").
    let out = dm(&["finding", "list", "--mission", "no-such-mission"]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("no findings match"), "got:\n{stdout}");
    assert!(
        !stdout.contains("finding sync"),
        "the store is NOT empty — do not tell the operator to sync: {stdout}"
    );

    // A malformed --since would match no day file and exit clean, which reads
    // exactly like "there are no findings". It must refuse instead.
    let out = dm(&["finding", "sync", "--since", "last-tuesday"]);
    assert_ne!(out.status.code(), Some(0), "a non-date --since must not exit clean");
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("YYYY-MM-DD"),
        "the error names the shape: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    // A key that would escape the store is refused, not resolved — proved
    // against a record that DOES exist at the escaped path. The store is
    // `<home>/findings`, so `../x/1` addresses `<home>/x/1`, the store's own
    // parent. Without the key check this read would succeed and print a file
    // from outside the store; an assertion that planted nothing there would
    // have passed on the absence instead of on the refusal.
    fs::create_dir_all(home.path().join("x/1")).unwrap();
    fs::write(
        home.path().join("x/1/finding.json"),
        serde_json::json!({
            "key": "x/1", "dispatch": "x", "seq": 1, "ts": "2026-09-03T00:00:00Z",
            "tool_name": "create_finding",
            "proposer": {"handle": "outside-the-store", "model": "m"},
            "context": serde_json::Value::Null,
            "emitted": {"file": "ESCAPED-THE-STORE.ts"},
            "schema_version": "1",
        })
        .to_string(),
    )
    .unwrap();
    let out = dm(&["finding", "show", "../x/1"]);
    assert_eq!(out.status.code(), Some(1), "a traversal key must not resolve");
    let escaped_stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        escaped_stdout.is_empty(),
        "nothing from outside the store is printed: {escaped_stdout}"
    );
    let escaped_stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        escaped_stderr.contains("not a finding key"),
        "the key is INVALID, not merely missing — a 'no finding <key>' message would mean \
the key was ACCEPTED and only the file happened to be absent: {escaped_stderr}"
    );
    assert!(!escaped_stderr.contains("ESCAPED-THE-STORE"), "got: {escaped_stderr}");

    // The human list names the mission when the finding has one.
    let out = dm(&["finding", "list", "--mission", "crawl-1"]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("crawl-1"), "the mission is shown: {stdout}");

    // Filters otherwise read the record's own context — never the emission.
    let out = dm(&["finding", "list", "--rule", "nope", "--json"]);
    let v: serde_json::Value =
        serde_json::from_str(&String::from_utf8_lossy(&out.stdout)).unwrap();
    assert!(v["findings"].as_array().unwrap().is_empty(), "got: {v}");

    // `finding show` prints ONE record, addressed by its key.
    let out = dm(&["finding", "show", "sess-a/1"]);
    assert!(out.status.success(), "stderr: {}", String::from_utf8_lossy(&out.stderr));
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("sess-a/1"), "got:\n{stdout}");
    assert!(stdout.contains("a.ts"), "the emission is shown: {stdout}");
    assert!(stdout.contains("crawl-1"), "show names the mission: {stdout}");
    assert!(!stdout.contains("sess-b"), "show is ONE record: {stdout}");

    // A missing key is an error with a clear message, not an empty success.
    let out = dm(&["finding", "show", "sess-a/99"]);
    assert_eq!(out.status.code(), Some(1), "a missing finding exits 1");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("sess-a/99"), "the message names the key: {stderr}");
}

// ─── `mod` family (#2265) ────────────────────────────────────────────────
//
// A mod is how something COULD change: a KIT of instructions plus data, in
// whatever form the proposer chose. darkmux never types a kit and never opens
// it. The key is MINTED per mod, never derived from a finding, so two agents
// proposing for one observation produce two records rather than one
// overwriting the other. The view from a finding to its mods is DERIVED by
// scanning mods — nothing is written back onto the finding.

#[test]
fn mod_create_mints_per_call_copies_attachments_and_finding_show_lists_the_mods() {
    let home = TempDir::new().unwrap();
    let flows = home.path().join("flows");
    write_finding_day_file(&flows);

    let dm = |args: &[&str]| {
        darkmux_cmd()
            .env("DARKMUX_HOME", home.path())
            .env("DARKMUX_FLOWS_DIR", &flows)
            .env("DARKMUX_LMS_BIN", "/usr/bin/true")
            .args(args)
            .output()
            .expect("darkmux runs")
    };
    // Same invocation, with a kit piped on stdin.
    let dm_stdin = |args: &[&str], stdin: &str| {
        darkmux_cmd()
            .env("DARKMUX_HOME", home.path())
            .env("DARKMUX_FLOWS_DIR", &flows)
            .env("DARKMUX_LMS_BIN", "/usr/bin/true")
            .args(args)
            .write_stdin(stdin)
            .output()
            .expect("darkmux runs")
    };

    // The findings the mods will name have to exist first.
    assert!(dm(&["finding", "sync"]).status.success());

    // ── create, from stdin ────────────────────────────────────────────────
    let out = dm_stdin(
        &["mod", "create", "--by", "sonnet", "--for", "sess-a/1", "--kit", "-"],
        "rename the predicate, then add a test\n",
    );
    assert!(out.status.success(), "stderr: {}", String::from_utf8_lossy(&out.stderr));
    // stdout is the KEY, alone, on the last line — the orchestrator pipes it
    // straight into `mod show <key>` / `--for`. Anything else it has to say
    // (the path it wrote, a missing `for`) goes to stderr, so `$(...)` around
    // this command captures a key and never a path.
    let key_a = String::from_utf8_lossy(&out.stdout).lines().last().unwrap().trim().to_string();
    assert!(key_a.starts_with("mod-"), "create prints the minted key: {key_a}");
    assert_eq!(
        String::from_utf8_lossy(&out.stdout).trim(),
        key_a,
        "stdout is the key ALONE — a path on it would be captured by `$(darkmux mod create …)`"
    );
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("mod.json"),
        "the path it wrote is still reported, on stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    // The captured line is directly usable as an address.
    let out_show = dm(&["mod", "show", &key_a]);
    assert!(
        out_show.status.success(),
        "`mod show $(darkmux mod create …)` must work: {}",
        String::from_utf8_lossy(&out_show.stderr)
    );
    let rec: serde_json::Value = serde_json::from_str(
        &fs::read_to_string(home.path().join("mods").join(&key_a).join("mod.json")).unwrap(),
    )
    .expect("the record is on disk where the key says");
    assert_eq!(rec["by"], "sonnet");
    assert_eq!(rec["for"], serde_json::json!(["sess-a/1"]));
    // The kit is kept VERBATIM — prose stays the prose that was written.
    assert_eq!(rec["kit"], "rename the predicate, then add a test\n");
    // The named finding's own provenance is copied on, so a reader of the mod
    // never has to go find the finding.
    assert_eq!(rec["context"]["findings"][0]["mission_id"], "crawl-1");
    assert_eq!(rec["context"]["findings"][0]["emitted"]["file"], "a.ts");

    // ── create, from a file, with two attachments ─────────────────────────
    let src = home.path().join("src");
    fs::create_dir_all(&src).unwrap();
    // Duplicate keys and an integer past f64 — both survive a byte-exact
    // store and neither survives a parse/re-serialize round trip.
    let kit_text = "{\n  \"diff\": \"--- a\",\n  \"diff\": \"+++ b\",\n  \"n\": 12345678901234567890123\n}\n";
    fs::write(src.join("kit.json"), kit_text).unwrap();
    fs::write(src.join("patch.diff"), b"--- a\n+++ b\n").unwrap();
    fs::write(src.join("shot.png"), [0x89u8, 0x50, 0x4e, 0x47, 0x0d]).unwrap();
    let out = dm(&[
        "mod", "create", "--by", "kain", "--for", "sess-b/2",
        "--kit", src.join("kit.json").to_str().unwrap(),
        "--attach", src.join("patch.diff").to_str().unwrap(),
        "--attach", src.join("shot.png").to_str().unwrap(),
    ]);
    assert!(out.status.success(), "stderr: {}", String::from_utf8_lossy(&out.stderr));
    let key_b = String::from_utf8_lossy(&out.stdout).trim().to_string();
    let attach_dir = home.path().join("mods").join(&key_b).join("attachments");
    assert_eq!(
        fs::read(attach_dir.join("patch.diff")).unwrap(),
        b"--- a\n+++ b\n",
        "an attachment is copied byte for byte"
    );
    assert_eq!(fs::read(attach_dir.join("shot.png")).unwrap(), [0x89u8, 0x50, 0x4e, 0x47, 0x0d]);
    // A JSON-looking kit is stored as the RAW TEXT, byte for byte — parsing
    // and re-serializing it would collapse duplicate keys and round large
    // integers, so a kit is never parsed on write.
    let rec_b: serde_json::Value = serde_json::from_str(
        &fs::read_to_string(home.path().join("mods").join(&key_b).join("mod.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(
        rec_b["kit"].as_str().expect("the kit is ALWAYS a string"),
        kit_text,
        "byte-exact: the file's own bytes, not a re-serialization"
    );
    assert_eq!(rec_b["kit_looks_json"], true, "a reader hint, computed once at write time");
    assert_eq!(rec_b["attachments"], serde_json::json!(["patch.diff", "shot.png"]));

    // ── two creates for ONE finding are two mods ──────────────────────────
    let out = dm_stdin(
        &["mod", "create", "--by", "kain", "--for", "sess-a/1", "--kit", "-"],
        "just add a comment",
    );
    let key_a2 = String::from_utf8_lossy(&out.stdout).trim().to_string();
    assert_ne!(key_a, key_a2, "the key is MINTED per mod — the second must not overwrite the first");
    assert!(home.path().join("mods").join(&key_a).join("mod.json").exists());
    assert!(home.path().join("mods").join(&key_a2).join("mod.json").exists());

    // (#2386) A `for` key with no stored finding is REFUSED — it would be a
    // link nothing can follow, and the usual cause is a typo or a copied
    // example. The refusal names the key and the escape hatch.
    let out = dm_stdin(
        &["mod", "create", "--by", "kain", "--for", "sess-z/9", "--kit", "-"],
        "for something not in the store",
    );
    assert!(!out.status.success(), "a dangling `for` key is refused");
    let refusal = String::from_utf8_lossy(&out.stderr).to_string();
    assert!(refusal.contains("sess-z/9"), "the refusal names it: {refusal}");
    assert!(refusal.contains("--allow-missing-finding"), "and the hatch: {refusal}");
    assert!(
        String::from_utf8_lossy(&out.stdout).trim().is_empty(),
        "and nothing goes to stdout, which is the key channel"
    );

    // The hatch itself: the operator asked for the link anyway, so it is
    // recorded — and still NAMED on stderr, so stdout stays pipeable.
    let out = dm_stdin(
        &["mod", "create", "--by", "kain", "--for", "sess-z/9", "--allow-missing-finding", "--kit", "-"],
        "for something not in the store",
    );
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    let key_z = stdout.trim().to_string();
    assert!(key_z.starts_with("mod-"), "stdout is still the key alone: {stdout}");
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("sess-z/9"),
        "a missing finding is named, not silent — on stderr, so stdout stays pipeable: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    // `--json` is where the path lives, beside the record itself.
    let out = dm_stdin(
        &["mod", "create", "--by", "kain", "--kit", "-", "--json"],
        "a standalone kit",
    );
    let v: serde_json::Value =
        serde_json::from_str(&String::from_utf8_lossy(&out.stdout)).expect("--json emits JSON");
    let key_j = v["record"]["key"].as_str().expect("the key").to_string();
    let path_j = home.path().join("mods").join(&key_j).join("mod.json");
    assert_eq!(v["path"].as_str().expect("--json carries the path"), path_j.to_str().unwrap());
    assert_eq!(v["record"]["by"], "kain", "the whole record is there: {v}");
    // `path` sits BESIDE the record, never inside it: the printed record has
    // to carry exactly the stored record's fields and nothing darkmux added.
    // Compared as VALUES (both parsed) — this pins the field set, not the
    // formatting, which pretty-printing changes either way.
    let on_disk: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&path_j).unwrap()).unwrap();
    assert_eq!(v["record"], on_disk, "the printed record has the stored record's fields");

    // A non-canonical finding key is CANONICALIZED on create, so one finding
    // has one address: `sess-a/01` must be findable as `sess-a/1` by both the
    // filter and the finding's own derived section.
    let out = dm_stdin(
        &["mod", "create", "--by", "zero-padded", "--for", "sess-a/01", "--kit", "-"],
        "same finding, padded key",
    );
    assert!(out.status.success(), "stderr: {}", String::from_utf8_lossy(&out.stderr));
    let key_pad = String::from_utf8_lossy(&out.stdout).trim().to_string();
    let rec_pad: serde_json::Value = serde_json::from_str(
        &fs::read_to_string(home.path().join("mods").join(&key_pad).join("mod.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(rec_pad["for"], serde_json::json!(["sess-a/1"]), "stored canonical");

    // A key that can address no finding is refused loudly, not stored as a
    // link nothing can follow.
    let out = dm_stdin(&["mod", "create", "--by", "kain", "--for", "no-slash", "--kit", "-"], "x");
    assert_ne!(out.status.code(), Some(0), "an unaddressable --for must not be stored");
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("finding key"),
        "the error names the shape: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    // A mod with neither instructions nor data is not a kit.
    let out = dm(&["mod", "create", "--by", "kain"]);
    assert_ne!(out.status.code(), Some(0), "a mod needs a kit or an attachment");

    // ── list ──────────────────────────────────────────────────────────────
    let keys = |args: &[&str]| -> Vec<String> {
        let out = dm(args);
        let v: serde_json::Value =
            serde_json::from_str(&String::from_utf8_lossy(&out.stdout)).expect("--json emits JSON");
        v["mods"].as_array().unwrap().iter().map(|m| m["key"].as_str().unwrap().to_string()).collect()
    };
    assert_eq!(keys(&["mod", "list", "--json"]).len(), 6, "every mod, ts-ascending");
    let for_a = keys(&["mod", "list", "--for", "sess-a/1", "--json"]);
    assert_eq!(for_a.len(), 3, "one observation can attract competing changes");
    assert!(
        for_a.contains(&key_pad),
        "the canonicalized `sess-a/01` mod is found under `sess-a/1`: {for_a:?}"
    );
    // The QUERY is canonicalized too. Canonicalizing only on write left one
    // finding with two addresses from the reader's side: the mod created with
    // `--for sess-a/01` was invisible to `--for sess-a/01`.
    assert_eq!(
        keys(&["mod", "list", "--for", "sess-a/01", "--json"]),
        for_a,
        "a non-canonical query returns exactly what the canonical one does"
    );
    // An unaddressable query is refused loudly — an empty result would read
    // as "no mods for that finding" when the key names no finding at all.
    let out = dm(&["mod", "list", "--for", "no-slash", "--json"]);
    assert_ne!(out.status.code(), Some(0), "an unaddressable --for must not exit clean");
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("finding key"),
        "the error names the shape: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(keys(&["mod", "list", "--for", "sess-b/2", "--json"]), vec![key_b.clone()]);
    // `--mission` matches through the `for` finding's OWN recorded mission.
    assert_eq!(keys(&["mod", "list", "--mission", "crawl-2", "--json"]), vec![key_b.clone()]);
    assert_eq!(keys(&["mod", "list", "--mission", "crawl-1", "--json"]).len(), 3);
    assert!(
        !keys(&["mod", "list", "--mission", "crawl-1", "--json"]).contains(&key_z),
        "a mod whose finding is not in the store belongs to no mission"
    );
    assert!(keys(&["mod", "list", "--mission", "no-such-mission", "--json"]).is_empty());
    assert!(keys(&["mod", "list", "--for", "sess-nope/1", "--json"]).is_empty());

    // A filter that matches nothing must not read like an EMPTY STORE.
    let out = dm(&["mod", "list", "--for", "sess-nope/1"]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("no mods match — 6 in the store"), "got:\n{stdout}");
    assert!(!stdout.contains("mod create"), "the store is NOT empty: {stdout}");

    // The human list previews the RAW kit and names the mod's findings.
    let out = dm(&["mod", "list", "--for", "sess-a/1"]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("rename the predicate"), "the raw kit is previewed: {stdout}");
    assert!(stdout.contains("sess-a/1"), "the `for` keys are shown: {stdout}");

    // ── show ──────────────────────────────────────────────────────────────
    let out = dm(&["mod", "show", &key_b]);
    assert!(out.status.success(), "stderr: {}", String::from_utf8_lossy(&out.stderr));
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains(&key_b) && stdout.contains("kain"), "got:\n{stdout}");
    assert!(stdout.contains("sess-b/2"), "show names the findings: {stdout}");
    assert!(stdout.contains("patch.diff") && stdout.contains("bytes"), "attachments + sizes: {stdout}");
    assert!(
        stdout.contains("\"diff\": \"--- a\",\n  \"diff\": \"+++ b\""),
        "the kit is printed as its own bytes, duplicate keys and all: {stdout}"
    );
    assert!(
        stdout.contains("12345678901234567890123"),
        "an integer past f64 survives, because nothing parsed it: {stdout}"
    );
    // Nothing is appended to the kit. The fixture ends in a newline, so the
    // output ends in exactly one — a second would be a byte darkmux invented.
    assert!(stdout.ends_with("}\n"), "the kit's own bytes end the output: {stdout:?}");
    assert!(!stdout.contains(&key_a), "show is ONE record: {stdout}");

    let out = dm(&["mod", "show", "mod-nope"]);
    assert_eq!(out.status.code(), Some(1), "a missing mod exits 1");
    assert!(String::from_utf8_lossy(&out.stderr).contains("mod-nope"), "the message names the key");

    // A key that would escape the store is refused, proved against a record
    // that DOES exist at the escaped path — the store is `<home>/mods`, so
    // `../x-mod` addresses `<home>/x-mod`, its parent.
    fs::create_dir_all(home.path().join("x-mod")).unwrap();
    fs::write(
        home.path().join("x-mod/mod.json"),
        serde_json::json!({
            "key": "x-mod", "ts": "2026-09-03T00:00:00Z", "by": "outside-the-store",
            "for": [], "kit": "ESCAPED-THE-STORE", "attachments": [],
            "context": {"findings": []}, "schema_version": "1",
        })
        .to_string(),
    )
    .unwrap();
    let out = dm(&["mod", "show", "../x-mod"]);
    assert_eq!(out.status.code(), Some(1), "a traversal key must not resolve");
    assert!(String::from_utf8_lossy(&out.stdout).is_empty(), "nothing outside the store is printed");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("not a mod key"),
        "the key is INVALID, not merely missing — the two need different remedies: {stderr}"
    );
    assert!(!stderr.contains("ESCAPED-THE-STORE"), "got: {stderr}");

    // ── the derived view: `finding show` lists the mods that name it ──────
    let out = dm(&["finding", "show", "sess-a/1"]);
    assert!(out.status.success(), "stderr: {}", String::from_utf8_lossy(&out.stderr));
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("\nmods\n"), "the section is present: {stdout}");
    assert!(
        stdout.contains(&key_a) && stdout.contains(&key_a2) && stdout.contains(&key_pad),
        "every mod naming this finding is listed, canonicalized one included: {stdout}"
    );
    assert!(!stdout.contains(&key_b), "only the mods naming THIS finding: {stdout}");
    // Nothing is written back onto the finding — the view is derived.
    let on_disk = fs::read_to_string(home.path().join("findings/sess-a/1/finding.json")).unwrap();
    assert!(!on_disk.contains("mod-"), "the finding record is never rewritten: {on_disk}");

    // A finding with no mods prints no section at all.
    let out = dm(&["finding", "show", "sess-b/2"]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains(&key_b), "sess-b/2's own mod is listed: {stdout}");
    let out = dm(&["finding", "list", "--json"]);
    assert!(out.status.success(), "the finding verbs still read the store unchanged");
}

/// (#2265) A mod the host recorded PARTIALLY — a dropped attachment, an
/// unaddressable `for` key — carries `warnings`. `mod show` must print them:
/// the field exists so the record is honest about being partial, and a
/// rendering that hides it makes the record look whole again.
#[test]
fn mod_show_prints_the_warnings_of_a_partial_mod() {
    let home = TempDir::new().unwrap();
    let dir = home.path().join("mods").join("mod-1788430000-abc123");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("mod.json"),
        r#"{"schema_version":"1","key":"mod-1788430000-abc123","ts":"2026-09-03T10:00:00Z","by":"coder (qwen)",
            "for":["sess-a/1"],"kit":"apply mod.diff\n","attachments":[],
            "warnings":["dropped attachment \"mod.diff\": no path/bytes pair"]}"#,
    )
    .unwrap();
    let out = darkmux_cmd()
        .env("DARKMUX_HOME", home.path())
        .env("DARKMUX_LMS_BIN", "/usr/bin/true")
        .args(["mod", "show", "mod-1788430000-abc123"])
        .output()
        .expect("darkmux runs");
    assert!(out.status.success(), "stderr: {}", String::from_utf8_lossy(&out.stderr));
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("warnings"), "a partial mod says so: {stdout}");
    assert!(stdout.contains("dropped attachment \"mod.diff\""), "and names the part: {stdout}");
    // The kit still ends the output byte-exact — the warnings print ABOVE it.
    assert!(stdout.ends_with("apply mod.diff\n"), "got: {stdout:?}");
}

/// (#2299) `enabled: false` is honored at mint: the disabled step never exists
/// in the run, the config snapshot keeps the flag, `graph-report.json` names
/// what was pruned and why, the `mission start` record carries the same
/// report, and `mission status` counts it. No CLI override exists: the config
/// is the only place the run's shape comes from.
#[test]
fn mission_launch_prunes_disabled_steps_at_mint_and_reports_them() {
    let home = TempDir::new().unwrap();
    let flows = TempDir::new().unwrap();
    let config_dir = home.path().join("mission-configs");
    fs::create_dir_all(&config_dir).unwrap();
    // 4 steps declared, 1 disabled; a task that depends only on the disabled
    // task goes with it; the freeform phase stays.
    let config_json = r#"{
        "id": "enabled-test",
        "name": "Enabled Test",
        "schema_version": "3.1",
        "phases": [{
            "id": "p1",
            "tasks": [
                {"id": "t-off", "enabled": false, "steps": [{"id": "s-off", "kind": "procedural.noop"}]},
                {"id": "t-on", "steps": [
                    {"id": "s-on-1", "kind": "procedural.noop"},
                    {"id": "s-on-2", "kind": "procedural.noop", "enabled": false}
                ]},
                {"id": "t-orphan", "depends_on": ["t-off"], "steps": [{"id": "s-orphan", "kind": "procedural.noop"}]}
            ]
        }]
    }"#;
    fs::write(config_dir.join("enabled-test.json"), config_json).unwrap();

    let out = darkmux_cmd()
        .env("DARKMUX_HOME", home.path())
        .env("DARKMUX_FLOWS_DIR", flows.path())
        .env("DARKMUX_LMS_BIN", "/usr/bin/true")
        .args(["mission", "launch", "enabled-test", "--timeout", "60"])
        .output()
        .expect("darkmux mission launch runs");
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "stdout:\n{stdout}\nstderr:\n{stderr}");
    assert!(stdout.contains("graph: 1 of 4 steps minted (3 left out by config)"), "got:\n{stdout}");

    // One mission on disk; its report and snapshot say what happened.
    let missions_dir = home.path().join("missions");
    let mission_dir = fs::read_dir(&missions_dir)
        .unwrap()
        .filter_map(|e| e.ok().map(|e| e.path()))
        .find(|p| p.is_dir())
        .expect("one mission dir");
    let report: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(mission_dir.join("graph-report.json")).unwrap()).unwrap();
    assert_eq!(report["steps_in_config"], 4);
    assert_eq!(report["steps_minted"], 1);
    assert_eq!(report["tasks_minted"], 1);
    let pruned: Vec<(String, String)> = report["pruned"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| (p["id"].as_str().unwrap().to_string(), p["reason"].as_str().unwrap().to_string()))
        .collect();
    assert!(pruned.contains(&("t-off".into(), "disabled".into())), "{pruned:?}");
    assert!(pruned.contains(&("s-on-2".into(), "disabled".into())), "{pruned:?}");
    assert!(pruned.contains(&("t-orphan".into(), "all_dependencies_pruned".into())), "{pruned:?}");
    assert!(pruned.contains(&("s-orphan".into(), "parent_pruned".into())), "{pruned:?}");
    let snapshot: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(mission_dir.join("config-snapshot.json")).unwrap()).unwrap();
    assert_eq!(snapshot["phases"][0]["tasks"][0]["enabled"], false, "the snapshot keeps the DECLARED config");
    assert_eq!(snapshot["phases"][0]["tasks"].as_array().unwrap().len(), 3);
    // The pruned items left no task record behind — nothing gray on disk.
    // Task records live under `tasks/<phase-id>/`; walk one level down.
    let task_files: Vec<String> = fs::read_dir(mission_dir.join("tasks"))
        .map(|d| {
            d.filter_map(|e| e.ok())
                .flat_map(|phase_dir| {
                    fs::read_dir(phase_dir.path())
                        .map(|dd| dd.filter_map(|e| e.ok()).map(|e| e.file_name().to_string_lossy().to_string()).collect::<Vec<_>>())
                        .unwrap_or_default()
                })
                .collect()
        })
        .unwrap_or_default();
    assert!(task_files.iter().any(|f| f.contains("t-on")), "{task_files:?}");
    assert!(!task_files.iter().any(|f| f.contains("t-off") || f.contains("t-orphan")), "{task_files:?}");

    // The `mission start` record carries the same report.
    let mut day = String::new();
    for e in fs::read_dir(flows.path()).unwrap().filter_map(|e| e.ok()) {
        day.push_str(&fs::read_to_string(e.path()).unwrap());
    }
    let start = day
        .lines()
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
        .find(|r| r["action"] == "mission start")
        .expect("a mission start record");
    assert_eq!(start["payload"]["graph"]["steps_minted"], 1, "{start}");
    assert_eq!(start["payload"]["graph"]["steps_in_config"], 4);

    // `mission status` counts it, human and JSON.
    let status = darkmux_cmd()
        .env("DARKMUX_HOME", home.path())
        .env("DARKMUX_FLOWS_DIR", flows.path())
        .env("DARKMUX_LMS_BIN", "/usr/bin/true")
        .args(["mission", "status", "--json"])
        .output()
        .unwrap();
    let v: serde_json::Value = serde_json::from_str(&String::from_utf8_lossy(&status.stdout)).unwrap();
    assert_eq!(v["missions"][0]["graph"]["steps_in_config"], 4, "{v}");
    assert_eq!(v["missions"][0]["graph"]["steps_minted"], 1);
    let human = darkmux_cmd()
        .env("DARKMUX_HOME", home.path())
        .env("DARKMUX_FLOWS_DIR", flows.path())
        .env("DARKMUX_LMS_BIN", "/usr/bin/true")
        .args(["mission", "status", "--all"])
        .output()
        .unwrap();
    let human_out = String::from_utf8_lossy(&human.stdout);
    assert!(human_out.contains("1 of 4 steps minted (3 left out by config)"), "got:\n{human_out}");
}

// ── (#2682 fix-pass MUST FIX 3) `running-phase-session-dead` end-to-end ────
//
// The unit tests inside `src/mission_status.rs` (`detect_drift`,
// `running_phase_session_drift`) all hand-type the `local_status`/
// `local_evidence` values `run()` is supposed to compute — none of them go
// through `run()` itself, and `tests/cli.rs` had NO `mission status`
// invocation that touched this rule at all. Mutation-proven per the review:
// replacing `run()`'s own `local_dispatch_status.get(&m.id)` lookup with a
// constant `(None, None)` left `cargo test -p darkmux --bin darkmux
// mission_status::` at 93/93 green — nothing proved the CLI's own wiring
// was connected to the rule the unit tests were exercising. These two tests
// go through the REAL `darkmux mission status --json` subprocess.

/// Writes a bare-minimum `mission.json` + one `phases/<id>.json` directly
/// (no `mission launch`, no dispatch, no container — pure static JSON, so
/// this never touches a model or the network). `started_ts` is a caller-
/// supplied Unix-seconds value so each test can place it however far in
/// the past its scenario needs.
fn write_running_mission(home: &std::path::Path, mission_id: &str, phase_id: &str, started_ts: u64) {
    let mission_dir = home.join("missions").join(mission_id);
    fs::create_dir_all(mission_dir.join("phases")).unwrap();
    fs::write(
        mission_dir.join("mission.json"),
        serde_json::json!({
            "id": mission_id,
            "description": "fix-pass e2e fixture",
            "status": "active",
            "phase_ids": [phase_id],
            "created_ts": started_ts,
            "started_ts": started_ts,
        })
        .to_string(),
    )
    .unwrap();
    fs::write(
        mission_dir.join("phases").join(format!("{phase_id}.json")),
        serde_json::json!({
            "id": phase_id,
            "mission_id": mission_id,
            "description": "fix-pass e2e phase",
            "status": "running",
            "created_ts": started_ts,
            "started_ts": started_ts,
            "task_ids": [],
        })
        .to_string(),
    )
    .unwrap();
}

fn mission_drift<'a>(board: &'a serde_json::Value, mission_id: &str) -> &'a serde_json::Value {
    board["missions"]
        .as_array()
        .unwrap_or_else(|| panic!("no missions array in board: {board}"))
        .iter()
        .find(|m| m["id"] == mission_id)
        .unwrap_or_else(|| panic!("mission {mission_id} not on the board: {board}"))
}

/// (#2682 fix-pass MUST FIX 1) Probe A: an Active mission whose Running
/// phase has NEVER dispatched anything at all — no flow records exist for
/// it — but `started_ts` is old enough that `mission_run_status_and_evidence`
/// reads it `Abandoned` on the mission's own AGE
/// (`DispatchSessionEvidence::NoAttributableSession`).
///
/// (round 2, MUST FIX 1) The board must stay SILENT here. That shape is the
/// ordinary state of a mission parked at a sign-off gate, and the permanent
/// state of every Active mission whose dispatches aged out of
/// `RUNS_FLOW_SCAN_WINDOW_DAYS` — round 1 fired a drift on it (re-worded,
/// but still firing), which removed the board's clean checkmark forever.
///
/// Deliberately NOT vacuous: it first proves `darkmux run list` really does
/// read this fixture `abandoned`, so the assertion below is measuring a
/// KNOWN disagreement the board declines to render, not an empty board.
///
/// (MUST FIX 2) `DARKMUX_INACTIVITY_TIMEOUT_SECONDS` is pinned. The verdict
/// is the mission's 25-minute age measured against `stale_after_ms()` —
/// twice that knob — so without the pin an operator with the documented
/// `7200` exported turns the budget into 4 hours and the fixture's own
/// premise (that `run list` reads it `abandoned` at all) evaporates.
#[test]
fn mission_status_running_phase_with_no_dispatch_ever_stays_silent() {
    let home = TempDir::new().unwrap();
    let flows = TempDir::new().unwrap(); // deliberately left empty
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    write_running_mission(home.path(), "never-dispatched-e2e", "p1", now - 25 * 60);

    // 60s knob → a 120s staleness budget: 25 minutes is unambiguously past
    // it, whatever the environment running this suite has exported.
    let run_list = darkmux_cmd()
        .env("DARKMUX_HOME", home.path())
        .env("DARKMUX_FLOWS_DIR", flows.path())
        .env("DARKMUX_INACTIVITY_TIMEOUT_SECONDS", "60")
        .args(["run", "list", "--json", "--all"])
        .output()
        .unwrap();
    assert!(run_list.status.success(), "{}", String::from_utf8_lossy(&run_list.stderr));
    let runs: serde_json::Value = serde_json::from_slice(&run_list.stdout).unwrap();
    let row = runs["runs"]
        .as_array()
        .expect("run list --json always emits a runs array")
        .iter()
        .find(|r| r["id"] == "never-dispatched-e2e")
        .unwrap_or_else(|| panic!("no run row for the fixture: {runs}"));
    assert_eq!(
        row["status"], "abandoned",
        "fixture premise: `run list` must genuinely read this mission abandoned, or the board's \
         silence below proves nothing: {row}"
    );

    let out = darkmux_cmd()
        .env("DARKMUX_HOME", home.path())
        .env("DARKMUX_FLOWS_DIR", flows.path())
        .env("DARKMUX_INACTIVITY_TIMEOUT_SECONDS", "60")
        .args(["mission", "status", "--json"])
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let board: serde_json::Value = serde_json::from_str(&String::from_utf8_lossy(&out.stdout)).unwrap();
    let m = mission_drift(&board, "never-dispatched-e2e");
    let drifts = m["drift"].as_array().unwrap();
    assert!(
        !drifts.iter().any(|d| d["kind"] == "running-phase-session-dead"),
        "a mission with no attributable dispatch session must not be flagged as one whose \
         dispatch session died — this is the sign-off-gate false alarm round 2 removed: {m}"
    );
}

/// (#2682 fix-pass MUST FIX 5) darkmux POSITIVELY recorded this mission's
/// one dispatch session ENDING (a `session.end` crash/kill/timeout
/// close-edge, attributed via the record's own `mission_id` field). The
/// board must describe that as an observation, never as "no evidence of
/// life", and it may still offer `mission abort`.
#[test]
fn mission_status_recorded_session_end_describes_an_observation_not_an_absence() {
    let home = TempDir::new().unwrap();
    let flows = TempDir::new().unwrap();
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    write_running_mission(home.path(), "recorded-end-e2e", "p1", now - 25 * 60);

    let day = darkmux_flow::day_utc_now();
    fs::write(
        flows.path().join(format!("{day}.jsonl")),
        serde_json::json!({
            "ts": "2024-01-01T09:00:00Z",
            "action": "session.end",
            "session_id": "e2e-crashed-session",
            "mission_id": "recorded-end-e2e",
        })
        .to_string()
            + "\n",
    )
    .unwrap();

    let out = darkmux_cmd()
        .env("DARKMUX_HOME", home.path())
        .env("DARKMUX_FLOWS_DIR", flows.path())
        .args(["mission", "status", "--json"])
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let board: serde_json::Value = serde_json::from_str(&String::from_utf8_lossy(&out.stdout)).unwrap();
    let m = mission_drift(&board, "recorded-end-e2e");
    let drifts = m["drift"].as_array().unwrap();
    let hit = drifts
        .iter()
        .find(|d| d["kind"] == "running-phase-session-dead")
        .unwrap_or_else(|| panic!("no running-phase-session-dead drift for a recorded session.end: {m}"));
    let detail = hit["detail"].as_str().unwrap().to_lowercase();
    assert!(
        detail.contains("recorded") && detail.contains("ending"),
        "must describe the POSITIVE observation: {detail}"
    );
    assert!(
        !detail.contains("no evidence of life"),
        "a recorded end is a fact, not the same claim as silence: {detail}"
    );
    let suggest: Vec<&str> = hit["suggest"].as_array().unwrap().iter().map(|s| s.as_str().unwrap()).collect();
    assert!(
        suggest.iter().any(|s| s.contains("mission abort")),
        "a positively recorded end is a reasonable abort case: {suggest:?}"
    );
}

/// (#2682 fix-pass round 2, CONSIDER) The third evidence variant,
/// `StaleNoTerminal` — a session that really did start and never announced
/// an ending, past the staleness budget. It had unit coverage only; the
/// other two were pinned end-to-end. This is also the ONE variant that is
/// genuinely the "SIGKILLed mission still reads clean" shape #2682 was
/// filed over, so its end-to-end absence was the least comfortable of the
/// three.
///
/// Doubles as the HUMAN-RENDER assertion (round 2's second CONSIDER):
/// every other assertion in this family reads `--json`, and mutating
/// `src/mission_status.rs`'s drift-render loop to print nothing left the
/// whole suite green. The second half of this test runs the same fixture
/// WITHOUT `--json` and requires the warning line and the copy-pasteable
/// command to actually reach stdout.
///
/// (MUST FIX 2) Budget pinned like its siblings, though this one would
/// survive without it: the verdict is an idle distance measured against
/// `stale_after_ms()`, and the pin says so — the 2024 stamp below happens
/// to be past any budget a person would set, which is luck, not a property
/// the fixture states.
#[test]
fn mission_status_stale_session_with_no_terminal_drifts_and_renders_for_a_human() {
    let home = TempDir::new().unwrap();
    let flows = TempDir::new().unwrap();
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    write_running_mission(home.path(), "stale-session-e2e", "p1", now - 25 * 60);

    // A `dispatch start` bookend with NO terminal ever landing, stamped far
    // enough back that it is stale under any budget — a fixed past stamp
    // rather than arithmetic on `now`, so nothing here can land on the
    // wrong side of a UTC midnight while the suite runs.
    let day = darkmux_flow::day_utc_now();
    fs::write(
        flows.path().join(format!("{day}.jsonl")),
        serde_json::json!({
            "ts": "2024-01-01T09:00:00Z",
            "action": "dispatch start",
            "session_id": "e2e-stale-session",
            "mission_id": "stale-session-e2e",
            "handle": "coder",
        })
        .to_string()
            + "\n",
    )
    .unwrap();

    let out = darkmux_cmd()
        .env("DARKMUX_HOME", home.path())
        .env("DARKMUX_FLOWS_DIR", flows.path())
        .env("DARKMUX_INACTIVITY_TIMEOUT_SECONDS", "60")
        .args(["mission", "status", "--json"])
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let board: serde_json::Value = serde_json::from_str(&String::from_utf8_lossy(&out.stdout)).unwrap();
    let m = mission_drift(&board, "stale-session-e2e");
    let drifts = m["drift"].as_array().unwrap();
    let hit = drifts
        .iter()
        .find(|d| d["kind"] == "running-phase-session-dead")
        .unwrap_or_else(|| panic!("no running-phase-session-dead drift for a started, never-terminated session: {m}"));
    let detail = hit["detail"].as_str().unwrap().to_lowercase();
    assert!(
        detail.contains("no evidence of life") || detail.contains("no terminal record"),
        "the one variant the fixed wording was always accurate for: {detail}"
    );
    let suggest: Vec<&str> = hit["suggest"].as_array().unwrap().iter().map(|s| s.as_str().unwrap()).collect();
    assert!(
        suggest.iter().any(|s| s.contains("mission abort")),
        "a genuinely stale session is a real abort case: {suggest:?}"
    );

    // ── the human board, same fixture ──────────────────────────────────
    let human = darkmux_cmd()
        .env("DARKMUX_HOME", home.path())
        .env("DARKMUX_FLOWS_DIR", flows.path())
        .env("DARKMUX_INACTIVITY_TIMEOUT_SECONDS", "60")
        .args(["mission", "status", "--all"])
        .output()
        .unwrap();
    assert!(human.status.success(), "{}", String::from_utf8_lossy(&human.stderr));
    let human_out = String::from_utf8_lossy(&human.stdout);
    assert!(
        human_out.contains("no terminal record seen"),
        "the drift's own sentence must reach the human board, not just --json:\n{human_out}"
    );
    assert!(
        human_out.contains("darkmux mission abort stale-session-e2e"),
        "the copy-pasteable reconcile command must reach the human board:\n{human_out}"
    );
}

// ── (#2300) growth: a step's OUTPUT grows tasks into the graph ───────────
//
// The invariant under test is the SEAM, not any one mission: phase 1 writes
// a JSON artifact and returns its path as the step's `output`; phase 2
// declares a `grow` template naming phase 1's task; the launcher reads that
// artifact at the phase boundary and mints one copy of the template per
// item. Everything here is `procedural.shell`/`procedural.noop`, so no
// model, no container and no network is involved.

/// Writes a two-phase config where `plan-task` emits `units` and
/// `unit-task` grows over them. Returns (home, flows) tempdirs.
fn grow_fixture(units_json: &str, config_extra: &str) -> (TempDir, TempDir, std::path::PathBuf) {
    let home = TempDir::new().unwrap();
    let flows = TempDir::new().unwrap();
    let plan_path = home.path().join("plan.json");
    fs::write(&plan_path, units_json).unwrap();

    let config_dir = home.path().join("mission-configs");
    fs::create_dir_all(&config_dir).unwrap();
    // `procedural.shell`'s output is the command's stdout, so echoing the
    // path IS "the step's output is a path to a JSON file" — the contract
    // every producing step honors (`crawl.plan` writes `plan/<rule>.json`
    // and returns that path).
    let config_json = format!(
        r#"{{
        "id": "grow-test",
        "name": "Grow Test",
        "schema_version": "3.2",
        "phases": [
          {{
            "id": "plan",
            "tasks": [{{
              "id": "plan-task",
              "steps": [{{
                "id": "plan-step",
                "kind": "procedural.shell",
                "config": {{ "command": "echo {plan}" }}
              }}]
            }}]
          }},
          {{
            "id": "units",
            "tasks": [{{
              "id": "unit-task",
              "depends_on": ["plan-task"],
              "grow": {{
                "from": "plan-task",
                "items": "units",
                "id": "{{{{item.id}}}}",
                "config": {{ "unit": "{{{{item.id}}}}", "rule": "{{{{item.rule}}}}" }}
              }},
              "steps": [{{ "id": "unit-step", "kind": "procedural.noop", "config": {{}} }}]
            }}]
          }}
        ]{extra}
    }}"#,
        plan = plan_path.display(),
        extra = config_extra,
    );
    fs::write(config_dir.join("grow-test.json"), config_json).unwrap();
    (home, flows, plan_path)
}

fn launch_grow(home: &TempDir, flows: &TempDir) -> std::process::Output {
    darkmux_cmd()
        .env("DARKMUX_HOME", home.path())
        .env("DARKMUX_FLOWS_DIR", flows.path())
        .env("DARKMUX_LMS_BIN", "/usr/bin/true")
        .args(["mission", "launch", "grow-test"])
        .output()
        .unwrap()
}

fn one_mission_dir(home: &TempDir) -> std::path::PathBuf {
    let missions = home.path().join("missions");
    let mut entries: Vec<_> = fs::read_dir(&missions)
        .unwrap_or_else(|e| panic!("reading {}: {e}", missions.display()))
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.is_dir())
        .collect();
    entries.sort();
    assert_eq!(entries.len(), 1, "expected exactly one minted mission: {entries:?}");
    entries.pop().unwrap()
}

fn phase_record(mission_dir: &std::path::Path, phase_id: &str) -> serde_json::Value {
    let path = mission_dir.join("phases").join(format!("{phase_id}.json"));
    serde_json::from_str(
        &fs::read_to_string(&path).unwrap_or_else(|e| panic!("reading {}: {e}", path.display())),
    )
    .unwrap()
}

fn flow_actions(flows: &TempDir) -> Vec<serde_json::Value> {
    let mut out = Vec::new();
    for entry in fs::read_dir(flows.path()).unwrap().filter_map(|e| e.ok()) {
        let Ok(text) = fs::read_to_string(entry.path()) else { continue };
        for line in text.lines().filter(|l| !l.trim().is_empty()) {
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(line) {
                out.push(v);
            }
        }
    }
    out
}

#[test]
fn mission_launch_grows_one_task_per_plan_unit_with_provenance() {
    let (home, flows, plan_path) = grow_fixture(
        r#"{"units":[{"id":"u-1","rule":"r"},{"id":"u-2","rule":"r"}]}"#,
        "",
    );
    let out = launch_grow(&home, &flows);
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "launch failed\nstdout:\n{stdout}\nstderr:\n{stderr}");

    let dir = one_mission_dir(&home);
    // Phase ids are composed from the real mission id at mint, so the
    // grown phase's directory name is `<mission id>-units`, not `units`.
    let mission_id = dir.file_name().unwrap().to_string_lossy().to_string();
    let grown_phase = format!("{mission_id}-units");
    // Two grown tasks on disk, in the GROWN phase, one per unit.
    for unit in ["u-1", "u-2"] {
        let task = dir.join("tasks").join(&grown_phase).join(format!("unit-task-{unit}.json"));
        assert!(task.is_file(), "missing grown task record {}\n{stdout}", task.display());
        let step_path = dir.join("steps").join(&grown_phase).join(format!("unit-step-{unit}.json"));
        let step: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&step_path).unwrap()).unwrap();
        assert_eq!(step["config"]["unit"], serde_json::json!(unit), "step: {step}");
        assert_eq!(step["config"]["rule"], serde_json::json!("r"), "step: {step}");
        assert_eq!(step["config"]["grown_from"]["task"], serde_json::json!("plan-task"));
        assert_eq!(step["config"]["grown_from"]["item"], serde_json::json!(unit));
        assert_eq!(step["status"], serde_json::json!("complete"), "the grown step must RUN: {step}");
    }
    // The template itself is never minted.
    assert!(
        !dir.join("tasks").join(&grown_phase).join("unit-task.json").is_file(),
        "the `grow` template must not be minted as a task of its own"
    );

    // (#2300) The phase record names the tasks it owns — the declared ids
    // at mint, the grown ids appended at the boundary. `crawl_launch.rs`
    // has always written this field; the generic launcher does now too.
    let plan_phase = phase_record(&dir, &format!("{mission_id}-plan"));
    assert_eq!(
        plan_phase["task_ids"],
        // The placeholder-prefix rule rewrites `plan-task` (prefixed by the
        // document phase id `plan`) into `<real phase id>-task`.
        serde_json::json!([format!("{mission_id}-plan-task")]),
        "the producing phase lists its declared task: {plan_phase}"
    );
    let units_phase = phase_record(&dir, &grown_phase);
    let listed = units_phase["task_ids"].as_array().expect("task_ids is a list");
    assert_eq!(
        listed.len(),
        2,
        "the grown phase lists exactly its grown tasks (the template is not one): {units_phase}"
    );
    for unit in ["u-1", "u-2"] {
        assert!(
            listed.contains(&serde_json::json!(format!("unit-task-{unit}"))),
            "missing grown id in task_ids: {units_phase}"
        );
    }
    assert!(
        !listed.contains(&serde_json::json!("unit-task")),
        "the `grow` template must never be listed as a task: {units_phase}"
    );

    // graph-report.json carries the growth event.
    let report: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(dir.join("graph-report.json")).unwrap()).unwrap();
    let grown = report["grown"].as_array().expect("graph-report has a `grown` section");
    assert_eq!(grown.len(), 1, "report: {report}");
    assert_eq!(grown[0]["from"], serde_json::json!("plan-task"));
    assert_eq!(grown[0]["task_template"], serde_json::json!("unit-task"));
    assert_eq!(grown[0]["items"], serde_json::json!(2));
    // (#2310 swarm F / S2-2) `source` is the PRODUCING STEP's id — one
    // meaning on every arm, and never the absolute host path #2301 put
    // here (this same struct rides the fleet stream as
    // `mission.grow.source`). `plan_path` is still reachable: it is that
    // step's own `output`.
    let grown_source = grown[0]["source"].as_str().expect("`source` is a string");
    assert!(
        !grown_source.starts_with('/') && grown_source != plan_path.display().to_string(),
        "`source` must be a step id, not a host path: {grown_source}"
    );
    assert!(grown_source.contains("plan-step"), "`source` names the producing STEP: {grown_source}");
    assert!(grown[0]["source_path"].is_null(), "the old key is renamed, not aliased");
    assert_eq!(grown[0]["minted"].as_array().unwrap().len(), 2);

    // One `mission.grow` flow record naming the same facts.
    let records = flow_actions(&flows);
    let grow_records: Vec<_> = records.iter().filter(|r| r["action"] == "mission.grow").collect();
    assert_eq!(grow_records.len(), 1, "records: {records:?}");
    let payload = &grow_records[0]["payload"];
    assert_eq!(payload["from"], serde_json::json!("plan-task"));
    assert_eq!(payload["items"], serde_json::json!(2));
    assert_eq!(payload["minted"].as_array().unwrap().len(), 2);
    assert!(
        payload.get("reason").is_none(),
        "`reason` is omitted, never null, when the growth minted something: {payload}"
    );

    // `mission status` names the growth.
    let status = darkmux_cmd()
        .env("DARKMUX_HOME", home.path())
        .env("DARKMUX_FLOWS_DIR", flows.path())
        .args(["mission", "status"])
        .output()
        .unwrap();
    let status_out = String::from_utf8_lossy(&status.stdout);
    assert!(
        status_out.contains("grew 2 task(s) from `plan-task`"),
        "mission status must name the growth, got:\n{status_out}"
    );
}

#[test]
fn mission_launch_grows_nothing_from_an_empty_plan_and_still_completes() {
    let (home, flows, _) = grow_fixture(r#"{"units":[]}"#, "");
    let out = launch_grow(&home, &flows);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "an empty plan is a real outcome, not a failure:\n{stdout}");
    assert!(
        stdout.contains("grew nothing") && stdout.contains("grew_nothing"),
        "the zero-item outcome must be named, got:\n{stdout}"
    );
    let dir = one_mission_dir(&home);
    let mission_id = dir.file_name().unwrap().to_string_lossy().to_string();
    let grown_dir = dir.join("tasks").join(format!("{mission_id}-units"));
    assert!(
        !grown_dir.exists() || fs::read_dir(&grown_dir).unwrap().next().is_none(),
        "zero items must mint zero tasks"
    );
    // (#2300) The phase must read `complete`, not `abandoned`. A phase with
    // no steps is invisible to the step-driven lazy start/close, so without
    // an explicit completion it sits Planned all run and the #1504 finalize
    // backstop sweeps it to Abandoned — recording a failure where the plan
    // simply planned nothing.
    let units_phase = phase_record(&dir, &format!("{mission_id}-units"));
    assert_eq!(
        units_phase["status"],
        serde_json::json!("complete"),
        "a phase that grew nothing must COMPLETE: {units_phase}"
    );
    assert!(
        !stdout.contains("reconciled to Abandoned"),
        "no backstop reconcile warning may print for a phase that legitimately grew nothing:\n{stdout}"
    );
    let records = flow_actions(&flows);
    let grow = records.iter().find(|r| r["action"] == "mission.grow").expect("a grow record");
    assert_eq!(grow["payload"]["reason"], serde_json::json!("grew_nothing"), "{grow}");
    assert_eq!(grow["payload"]["items"], serde_json::json!(0));
}

#[test]
fn mission_launch_fails_loudly_when_the_producer_output_is_not_a_json_path() {
    let home = TempDir::new().unwrap();
    let flows = TempDir::new().unwrap();
    let config_dir = home.path().join("mission-configs");
    fs::create_dir_all(&config_dir).unwrap();
    // The producing step completes fine — it just echoes a path that does
    // not exist. A silent zero-task growth here is exactly the failure the
    // retired `expand` primitive shipped; this must be an error naming the
    // task AND the path.
    let config_json = r#"{
        "id": "grow-test",
        "name": "Grow Test",
        "schema_version": "3.2",
        "phases": [
          { "id": "plan", "tasks": [{ "id": "plan-task", "steps": [
              { "id": "plan-step", "kind": "procedural.shell",
                "config": { "command": "echo /nope/not-a-plan.json" } }]}]},
          { "id": "units", "tasks": [{ "id": "unit-task", "depends_on": ["plan-task"],
              "grow": { "from": "plan-task", "items": "units", "id": "{{item.id}}", "config": {} },
              "steps": [{ "id": "unit-step", "kind": "procedural.noop", "config": {} }]}]}
        ]
    }"#;
    fs::write(config_dir.join("grow-test.json"), config_json).unwrap();

    let out = launch_grow(&home, &flows);
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(!out.status.success(), "a missing artifact must fail the run, got:\n{combined}");
    assert!(
        combined.contains("unit-task") && combined.contains("/nope/not-a-plan.json"),
        "the error must name the task and the path, got:\n{combined}"
    );
}

// ── (#2310 fix-loop packet C) scheduler semantics under failure ─────────
//
// The probe config the #2310 backend review (findings S4-1/S4-2/S4-3,
// C1/C4) reproduced its end state from, as a fixture. Three phases, a
// deliberate failure in the first, and dependents in the SECOND that reach
// back across the phase boundary into it — the shape every built-in config
// (`review.json`, `crawl.json`) actually has, and the one the per-phase
// scheduler split (#2300) broke. Everything is `procedural.shell` /
// `procedural.noop`: no model, no container, no network.
//
//   p1  t-fail    s-fail (`exit 3`) → s-after            (two steps)
//   p2  t-dep     depends_on t-fail                       (default run_on)
//       t-chain   depends_on t-dep                        (default run_on)
//       t-dep2    depends_on t-fail, run_on complete+error
//       t-chain-err depends_on t-dep2 AND t-chain, run_on complete+error
//   p3  t-deliver (no depends_on)
//
// Expected AFTER the packet: t-chain-err RUNS; s-dep/s-chain are Abandoned
// naming s-fail; s-dep2 receives s-fail's reason as its input; no step is
// left Planned/Running under a terminal mission; p1/p2 close Abandoned on
// disk AND in the envelope.

/// `deliver_command` is the shell body of p3's delivering step — `"true"`
/// for the ordinary probe, `"exit 1"` for the C5 exit-code case.
fn fail_probe_fixture(deliver_command: &str) -> (TempDir, TempDir) {
    fail_probe_fixture_with(deliver_command, "")
}

/// `deliver_extra` is spliced into p3's `t-deliver` object — the
/// review shape is `"depends_on": [...], "run_on": ["complete","error"],`
/// naming a task in an EARLIER phase.
fn fail_probe_fixture_with(deliver_command: &str, deliver_extra: &str) -> (TempDir, TempDir) {
    let home = TempDir::new().unwrap();
    let flows = TempDir::new().unwrap();
    let config_dir = home.path().join("mission-configs");
    fs::create_dir_all(&config_dir).unwrap();
    let config_json = format!(
        r#"{{
        "id": "fail-probe",
        "name": "Fail Probe",
        "schema_version": "3.4",
        "outcome_from": "t-deliver",
        "phases": [
          {{
            "id": "p1",
            "tasks": [{{
              "id": "t-fail",
              "steps": [
                {{ "id": "s-fail", "kind": "procedural.shell", "config": {{ "command": "echo boom >&2; exit 3" }} }},
                {{ "id": "s-after", "kind": "procedural.noop", "config": {{}} }}
              ]
            }}]
          }},
          {{
            "id": "p2",
            "tasks": [
              {{ "id": "t-dep", "depends_on": ["t-fail"],
                 "steps": [{{ "id": "s-dep", "kind": "procedural.noop", "config": {{}} }}] }},
              {{ "id": "t-chain", "depends_on": ["t-dep"],
                 "steps": [{{ "id": "s-chain", "kind": "procedural.noop", "config": {{}} }}] }},
              {{ "id": "t-dep2", "depends_on": ["t-fail"], "run_on": ["complete", "error"],
                 "steps": [{{ "id": "s-dep2", "kind": "procedural.shell",
                              "config": {{ "command": "echo dep2-saw:${{DARKMUX_STEP_INPUT_T_FAIL:-none}}" }} }}] }},
              {{ "id": "t-chain-err", "depends_on": ["t-dep2", "t-chain"], "run_on": ["complete", "error"],
                 "steps": [{{ "id": "s-chain-err", "kind": "procedural.shell",
                              "config": {{ "command": "echo chain-err-ran" }} }}] }}
            ]
          }},
          {{
            "id": "p3",
            "tasks": [{{
              "id": "t-deliver", {deliver_extra}
              "steps": [{{ "id": "s-deliver", "kind": "procedural.shell",
                           "config": {{ "command": "{deliver}" }} }}]
            }}]
          }}
        ]
    }}"#,
        deliver = deliver_command,
        deliver_extra = deliver_extra,
    );
    fs::write(config_dir.join("fail-probe.json"), config_json).unwrap();
    (home, flows)
}

fn launch_fail_probe(home: &TempDir, flows: &TempDir) -> std::process::Output {
    darkmux_cmd()
        .env("DARKMUX_HOME", home.path())
        .env("DARKMUX_FLOWS_DIR", flows.path())
        .env("DARKMUX_LMS_BIN", "/usr/bin/true")
        .args(["mission", "launch", "fail-probe"])
        .output()
        .unwrap()
}

/// Every persisted step of the mission, keyed by step id (ids are unique
/// across phases in this fixture), as `(status, output)`.
fn probe_steps(dir: &std::path::Path) -> BTreeMap<String, (String, String)> {
    let mut out = BTreeMap::new();
    let steps_root = dir.join("steps");
    for phase_dir in fs::read_dir(&steps_root).unwrap().filter_map(|e| e.ok()) {
        if !phase_dir.path().is_dir() {
            continue;
        }
        for f in fs::read_dir(phase_dir.path()).unwrap().filter_map(|e| e.ok()) {
            let Ok(text) = fs::read_to_string(f.path()) else { continue };
            let v: serde_json::Value = serde_json::from_str(&text).unwrap();
            out.insert(
                v["id"].as_str().unwrap().to_string(),
                (
                    v["status"].as_str().unwrap_or("?").to_string(),
                    v["output"].as_str().unwrap_or("").to_string(),
                ),
            );
        }
    }
    out
}

fn probe_mission_and_phases(
    home: &TempDir,
) -> (std::path::PathBuf, String, serde_json::Value, BTreeMap<String, String>) {
    let dir = one_mission_dir(home);
    let mission_id = dir.file_name().unwrap().to_string_lossy().to_string();
    let mission: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(dir.join("mission.json")).unwrap()).unwrap();
    let mut phases = BTreeMap::new();
    for p in ["p1", "p2", "p3"] {
        let path = dir.join("phases").join(format!("{mission_id}-{p}.json"));
        if let Ok(text) = fs::read_to_string(&path) {
            let v: serde_json::Value = serde_json::from_str(&text).unwrap();
            phases.insert(p.to_string(), v["status"].as_str().unwrap_or("?").to_string());
        }
    }
    (dir, mission_id, mission, phases)
}

/// (#2310 swarm F, from the C2 post-merge review) A run whose declared
/// work was ABANDONED — never ran, and never will — must not read
/// "clean, 0 errored". `t-forward` sits in p1 and depends FORWARD on
/// `t-late` in p2 with `run_on: ["error"]`; `t-late` completes, so
/// `t-forward`'s condition can never be met and the phase-exit sweep
/// abandons it. Nothing errors, so before this fix `build_envelope`
/// reported `status: clean`, `errored_steps: []` and a summary line
/// saying `1 step(s) complete, 0 errored` on a run that silently dropped
/// half of what the config declared.
fn forward_edge_fixture() -> (TempDir, TempDir) {
    let home = TempDir::new().unwrap();
    let flows = TempDir::new().unwrap();
    let config_dir = home.path().join("mission-configs");
    fs::create_dir_all(&config_dir).unwrap();
    fs::write(
        config_dir.join("forward-probe.json"),
        r#"{
        "id": "forward-probe",
        "name": "Forward Probe",
        "schema_version": "3.4",
        "phases": [
          {
            "id": "p1",
            "tasks": [{
              "id": "t-forward",
              "depends_on": ["t-late"],
              "run_on": ["error"],
              "steps": [{ "id": "s-forward", "kind": "procedural.noop", "config": {} }]
            }]
          },
          {
            "id": "p2",
            "tasks": [{
              "id": "t-late",
              "steps": [{ "id": "s-late", "kind": "procedural.shell",
                          "config": { "command": "echo late-ran" } }]
            }]
          }
        ]
    }"#,
    )
    .unwrap();
    (home, flows)
}

#[test]
fn an_abandoned_declared_step_makes_the_run_degraded_never_clean() {
    let (home, flows) = forward_edge_fixture();
    let out = darkmux_cmd()
        .env("DARKMUX_HOME", home.path())
        .env("DARKMUX_FLOWS_DIR", flows.path())
        .env("DARKMUX_LMS_BIN", "/usr/bin/true")
        .args(["mission", "launch", "forward-probe"])
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    let dir = one_mission_dir(&home);
    let steps = probe_steps(&dir);

    // Precondition — the fixture must actually produce the shape under
    // test: one completed step, one ABANDONED step, nothing errored.
    assert_eq!(steps["s-late"].0, "complete", "steps: {steps:#?}\n{stdout}");
    assert_eq!(
        steps["s-forward"].0, "abandoned",
        "the forward edge's run_on can never be met, so its step must abandon: {steps:#?}\n{stdout}"
    );
    assert!(
        steps.values().all(|(status, _)| status != "error"),
        "the fixture must contain NO errored step, or it proves nothing: {steps:#?}"
    );

    let envelope: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(dir.join("envelope.json")).unwrap()).unwrap();
    assert_eq!(
        envelope["status"],
        serde_json::json!("degraded"),
        "declared work that never ran is not a clean run: {envelope}"
    );
    let abandoned = envelope["payload"]["abandoned_steps"]
        .as_array()
        .unwrap_or_else(|| panic!("the envelope must name its abandoned steps: {envelope}"));
    assert!(
        abandoned.iter().any(|v| v.as_str().is_some_and(|s| s.contains("s-forward"))),
        "the abandoned step must be named: {envelope}"
    );
    let warnings = envelope["warnings"].as_array().cloned().unwrap_or_default();
    assert!(
        warnings.iter().any(|w| w.as_str().is_some_and(|s| s.contains("never ran"))),
        "the warning must count the steps that never ran: {envelope}"
    );
    assert!(
        stdout.contains("1 never ran"),
        "the run summary line must count what never ran, got:\n{stdout}"
    );
}

/// The fail probe's own envelope: it abandons three steps (`s-after`,
/// `s-dep`, `s-chain`) alongside its errored one, and every one of them
/// must be counted rather than silently dropped from the tally.
#[test]
fn fail_probe_envelope_counts_its_abandoned_steps_too() {
    let (home, flows) = fail_probe_fixture("true");
    let _ = launch_fail_probe(&home, &flows);
    let dir = one_mission_dir(&home);
    let envelope: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(dir.join("envelope.json")).unwrap()).unwrap();
    let abandoned: Vec<&str> = envelope["payload"]["abandoned_steps"]
        .as_array()
        .expect("abandoned_steps")
        .iter()
        .filter_map(|v| v.as_str())
        .collect();
    for want in ["s-after", "s-dep", "s-chain"] {
        assert!(
            abandoned.iter().any(|id| id.contains(want)),
            "`{want}` abandoned on disk but missing from the envelope: {envelope}"
        );
    }
    assert_eq!(envelope["status"], serde_json::json!("degraded"), "{envelope}");
}

/// (#2310 fix-loop C1 / S4-2) A later phase's task that declares
/// `run_on: ["complete","error"]` on a dependency that errored in an
/// EARLIER phase RUNS, and its default-`run_on` siblings are
/// cascade-abandoned with the originating step named. Before this packet
/// `run_step_graph` ran once per phase over only the phases entered so
/// far, so `cascade_abandon` never reached p2 and
/// `dependency_satisfies_run_on` saw a still-`Planned` t-fail: s-dep,
/// s-chain, s-dep2 and s-chain-err all sat `Planned` forever (s-dep2 ran
/// only because its OWN readiness came from a Planned dependency it never
/// waited on — and t-chain-err never ran at all).
#[test]
fn fail_probe_cross_phase_cascade_and_run_on_error() {
    let (home, flows) = fail_probe_fixture("true");
    let out = launch_fail_probe(&home, &flows);
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    let (_dir, _mid, _mission, _phases) = probe_mission_and_phases(&home);
    let steps = probe_steps(&one_mission_dir(&home));

    assert_eq!(steps["s-fail"].0, "error", "steps: {steps:#?}");
    // The run_on-error chain gets a real chance to run, across the phase
    // boundary, both hops.
    assert_eq!(steps["s-dep2"].0, "complete", "s-dep2 must run\n{stdout}\n{stderr}");
    assert_eq!(
        steps["s-chain-err"].0, "complete",
        "t-chain-err must run once its own run_on-error dependency reaches a terminal \
         status\nsteps: {steps:#?}\n{stdout}\n{stderr}"
    );
    // The errored task's OWN later step is stranded, never abandoned by the
    // cascade (that is deliberately outside its domain — see
    // `cascade_abandon`'s doc) — the phase-exit sweep terminalizes it, and
    // names the task's own failure so the operator reads the cause off the
    // step itself rather than a bare "never started".
    assert_eq!(steps["s-after"].0, "abandoned", "steps: {steps:#?}");
    assert!(
        steps["s-after"].1.contains("s-fail"),
        "the stranded step must name its own task's failure: {:?}",
        steps["s-after"].1
    );

    // The default-run_on chain is abandoned, naming the ORIGINATING step.
    for id in ["s-dep", "s-chain"] {
        assert_eq!(steps[id].0, "abandoned", "{id} must be Abandoned\nsteps: {steps:#?}");
        assert!(
            steps[id].1.contains("s-fail"),
            "{id}'s output must name the originating step: {:?}",
            steps[id].1
        );
    }
}

/// (#2310 fix-loop C3 / S4-3 / #2352 item 2) An errored MULTI-step task
/// forwards the output of the step that made it terminal — not
/// `step_ids.last()`, which for `t-fail` is the never-run `s-after`.
#[test]
fn fail_probe_forwards_the_errored_steps_output_to_a_run_on_error_dependent() {
    let (home, flows) = fail_probe_fixture("true");
    let out = launch_fail_probe(&home, &flows);
    let steps = probe_steps(&one_mission_dir(&home));
    let dep2_output = &steps["s-dep2"].1;
    assert!(
        dep2_output.contains("exited with") || dep2_output.contains("boom"),
        "s-dep2 must receive s-fail's own failure text as its `t-fail` input, got {dep2_output:?}\n\
         stdout:\n{}",
        String::from_utf8_lossy(&out.stdout)
    );
}

/// (#2310 fix-loop C2 / S4-1) The whole-run invariant: a terminal mission
/// never persists a non-terminal step, and the envelope's per-phase
/// outcomes equal the phase records on disk.
#[test]
fn fail_probe_terminal_mission_has_no_non_terminal_step_and_envelope_matches_disk() {
    let (home, flows) = fail_probe_fixture("true");
    let _ = launch_fail_probe(&home, &flows);
    let (dir, _mid, mission, phases) = probe_mission_and_phases(&home);
    let steps = probe_steps(&dir);

    assert_eq!(mission["status"], serde_json::json!("finalized"), "mission: {mission}");
    let live: Vec<_> = steps
        .iter()
        .filter(|(_, (status, _))| status == "planned" || status == "running")
        .collect();
    assert!(live.is_empty(), "terminal mission still holds non-terminal steps: {live:#?}");

    let envelope: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(dir.join("envelope.json")).unwrap()).unwrap();
    for phase in envelope["phases"].as_array().unwrap() {
        let real_id = phase["phase_id"].as_str().unwrap();
        let short = real_id.rsplit('-').next().unwrap().to_string();
        // (#2406) `Degraded` drives the SAME lifecycle terminal `Complete`
        // does — `PhaseStatus` has no third terminal to persist it as (see
        // `crew::envelope::PhaseOutcomeKind`'s own doc). Before #2406 this
        // match only knew Complete/else-Abandoned, which is exactly the
        // over-collapse the fixture below caught: p2 has a genuine mix (2
        // of 4 steps completed, 2 cascade-abandoned) that the pre-#2406
        // rule read straight to Abandoned on disk — this real end-to-end
        // fixture is independent proof the bug was reachable outside the
        // review pipeline too.
        let want = match phase["outcome"].as_str().unwrap() {
            "Complete" | "complete" | "Degraded" | "degraded" => "complete",
            _ => "abandoned",
        };
        assert_eq!(
            phases.get(&short).map(String::as_str),
            Some(want),
            "envelope says {want} for `{short}` but disk says {:?}\nenvelope: {envelope}",
            phases.get(&short)
        );
    }
    // p1 errored with nothing else completing in it: Abandoned, unchanged
    // by #2406 (the "nothing complete" case).
    assert_eq!(phases.get("p1").map(String::as_str), Some("abandoned"), "{phases:#?}");
    // (#2406) p2's default `run_on` chain cascade-abandoned TWO steps, but
    // TWO OTHER steps in the same phase completed for real
    // (`s-chain-err`/`s-deliver` — see the envelope's own `payload.
    // completed_steps`) — a genuine terminal mix. Before #2406 this read
    // "abandoned" on disk, discarding the fact real work shipped; the
    // envelope's own per-phase outcome is the more precise assertion (it
    // must say `degraded`, not merely "not abandoned"), and the disk
    // status is its lifecycle-terminal consequence.
    let p2_outcome = envelope["phases"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["phase_id"].as_str().unwrap().ends_with("-p2"))
        .and_then(|p| p["outcome"].as_str())
        .unwrap();
    assert_eq!(p2_outcome, "degraded", "p2 has 2 complete + 2 abandoned steps — a mix, not a clean abandon");
    assert_eq!(phases.get("p2").map(String::as_str), Some("complete"), "{phases:#?}");

    // …and the BOARD agrees. `mission status` reported "board is clean,
    // drift: []" through the whole S4-1/S4-2 class because every rule
    // stopped at the phase; now that the step-level rules exist (#2310
    // fix-loop C4), a clean board is evidence rather than a blind spot —
    // this assertion is red the moment either half regresses.
    let board = darkmux_cmd()
        .env("DARKMUX_HOME", home.path())
        .env("DARKMUX_FLOWS_DIR", flows.path())
        .env("DARKMUX_LMS_BIN", "/usr/bin/true")
        .args(["mission", "status", "--json", "--all"])
        .output()
        .unwrap();
    let board_json: serde_json::Value =
        serde_json::from_slice(&board.stdout).expect("mission status --json must be JSON");
    let drift = &board_json["missions"][0]["drift"];
    assert_eq!(
        drift.as_array().map(Vec::len),
        Some(0),
        "the board must be genuinely clean after a failed-but-fully-reconciled run, \
         got {drift}"
    );
}

/// (#2310 fix-loop C2 / C2-2) The two step-level drift kinds suggest
/// `darkmux mission finalize <id>` — so that command has to be a REMEDY,
/// not a no-op. `mission_terminal_*` bails "already Finalized" BEFORE its
/// #1504 reconcile runs, so on an already-closed mission (which is every
/// mission these two rules can fire on) the row was permanent: the board
/// told the operator to run a command that changed nothing, forever.
///
/// Builds the exact shape by hand — a `Planned` step persisted under a
/// Finalized mission's Complete phase, the residue a SIGKILLed run leaves —
/// then asserts drift fires, `mission finalize` clears it, and the step is
/// Abandoned on disk with a reason naming why.
///
/// This is also the disk→rule wiring pin for `live_steps_for` (C2-3):
/// mutating it to return an empty map turns this test red.
#[test]
fn finalize_reconciles_a_planned_step_under_an_already_terminal_phase() {
    let (home, flows) = forward_dep_fixture();
    let launched = darkmux_cmd()
        .env("DARKMUX_HOME", home.path())
        .env("DARKMUX_FLOWS_DIR", flows.path())
        .env("DARKMUX_LMS_BIN", "/usr/bin/true")
        .args(["mission", "launch", "forward-dep"])
        .output()
        .unwrap();
    assert_eq!(launched.status.code(), Some(0));
    let (dir, mission_id, mission, phases) = probe_mission_and_phases(&home);
    assert_eq!(mission["status"], serde_json::json!("finalized"));
    assert_eq!(phases.get("p1").map(String::as_str), Some("complete"));

    // Seed the residue: one more step, `Planned`, under p1 (Complete).
    let p1_dir = fs::read_dir(dir.join("steps"))
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .find(|p| p.file_name().unwrap().to_string_lossy().ends_with("-p1"))
        .expect("p1 steps dir");
    let mut seeded: serde_json::Value = serde_json::from_str(
        &fs::read_to_string(p1_dir.join("s-early.json")).unwrap(),
    )
    .unwrap();
    seeded["id"] = serde_json::json!("s-orphan");
    seeded["status"] = serde_json::json!("planned");
    seeded["output"] = serde_json::Value::Null;
    seeded["completed_ts"] = serde_json::Value::Null;
    fs::write(p1_dir.join("s-orphan.json"), serde_json::to_string(&seeded).unwrap()).unwrap();

    let board = |label: &str| -> serde_json::Value {
        let out = darkmux_cmd()
            .env("DARKMUX_HOME", home.path())
            .env("DARKMUX_FLOWS_DIR", flows.path())
            .env("DARKMUX_LMS_BIN", "/usr/bin/true")
            .args(["mission", "status", "--json", "--all"])
            .output()
            .unwrap();
        serde_json::from_slice(&out.stdout)
            .unwrap_or_else(|e| panic!("{label}: mission status --json must be JSON: {e}"))
    };

    let before = board("before");
    let kinds: Vec<&str> = before["missions"][0]["drift"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|d| d["kind"].as_str())
        .collect();
    assert!(
        kinds.contains(&"phase-terminal-live-step"),
        "the seeded residue must drift, got {kinds:?} — board: {before}"
    );
    let suggest = before["missions"][0]["drift"][0]["suggest"][0]
        .as_str()
        .unwrap_or_default()
        .to_string();
    assert!(suggest.contains("mission finalize"), "got {suggest:?}");

    // Run the command the board itself suggested.
    let fixed = darkmux_cmd()
        .env("DARKMUX_HOME", home.path())
        .env("DARKMUX_FLOWS_DIR", flows.path())
        .env("DARKMUX_LMS_BIN", "/usr/bin/true")
        .args(["mission", "finalize", &mission_id])
        .output()
        .unwrap();
    assert_eq!(
        fixed.status.code(),
        Some(0),
        "stdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&fixed.stdout),
        String::from_utf8_lossy(&fixed.stderr)
    );

    let after_steps = probe_steps(&dir);
    assert_eq!(
        after_steps["s-orphan"].0, "abandoned",
        "the remedy must terminalize the step: {after_steps:#?}"
    );
    assert!(
        after_steps["s-orphan"].1.contains("already terminal"),
        "the reason must say why it was reconciled, got {:?}",
        after_steps["s-orphan"].1
    );

    let after = board("after");
    let after_kinds: Vec<&str> = after["missions"][0]["drift"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|d| d["kind"].as_str())
        .collect();
    assert!(
        after_kinds.is_empty(),
        "the drift must CLEAR after running its own suggested remedy, got {after_kinds:?} — \
         board: {after}"
    );
}

/// (#2310 fix-loop, S4-4 — NOT this packet) The SIGKILL / hang shape: a
/// `procedural.shell` step running `sleep` has no deadline and no child
/// registration today, so SIGTERM/SIGINT do nothing until the command
/// returns and a SIGKILL leaves the mission Active, the phase running, an
/// orphan `sh`/`sleep`, and — before C4 — a board that called that clean.
///
/// `#[ignore]`d because the bounded-execution helper it asserts against
/// belongs to fix-loop packet D (`mods.gate` + `procedural.shell` timeout +
/// child registration) and has not merged. The assertions are written
/// against the shape D will produce, so the test flips to live by deleting
/// the attribute once D lands rather than being invented from scratch then
/// — the "assertion written now and skipped, naming what is missing and who
/// owns it" discipline. What THIS packet already guarantees, and what the
/// test therefore also asserts, is the C4 half: whatever state the kill
/// leaves behind, the board NAMES it instead of reporting clean.
#[test]
#[ignore = "needs fix-loop packet D: procedural.shell bounded deadline + child registration (S4-4)"]
fn hung_shell_step_is_bounded_and_the_board_names_what_the_kill_left() {
    let (home, flows) = fail_probe_fixture("sleep 600");
    let out = darkmux_cmd()
        .env("DARKMUX_HOME", home.path())
        .env("DARKMUX_FLOWS_DIR", flows.path())
        .env("DARKMUX_LMS_BIN", "/usr/bin/true")
        // Packet D's knob: the per-step bounded-execution deadline.
        .env("DARKMUX_STEP_EXEC_TIMEOUT_SECONDS", "2")
        .args(["mission", "launch", "fail-probe"])
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("timeout") || stderr.contains("deadline"),
        "the hung step must be killed at its deadline and say so, got:\n{stderr}"
    );

    // Whatever the kill left, no step stays non-terminal under a terminal
    // mission — and if one somehow does, the board says so rather than
    // reporting clean (#2310 fix-loop C4).
    let dir = one_mission_dir(&home);
    let steps = probe_steps(&dir);
    let live: Vec<_> = steps
        .iter()
        .filter(|(_, (status, _))| status == "planned" || status == "running")
        .collect();
    let board = darkmux_cmd()
        .env("DARKMUX_HOME", home.path())
        .env("DARKMUX_FLOWS_DIR", flows.path())
        .env("DARKMUX_LMS_BIN", "/usr/bin/true")
        .args(["mission", "status", "--json", "--all"])
        .output()
        .unwrap();
    let board_json: serde_json::Value = serde_json::from_slice(&board.stdout).unwrap();
    let kinds: Vec<&str> = board_json["missions"][0]["drift"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|d| d["kind"].as_str())
        .collect();
    if live.is_empty() {
        assert!(!kinds.iter().any(|k| k.ends_with("live-step")), "{kinds:?}");
    } else {
        assert!(
            kinds.iter().any(|k| k.ends_with("live-step")),
            "steps left live by the kill ({live:#?}) must be NAMED on the board, got {kinds:?}"
        );
    }
}

/// (#2310 fix-loop C5 / S4-C1) A run whose DELIVERING task — the one
/// `outcome_from` names — itself ended Error exits non-zero, even though
/// earlier steps completed and the run-level status is therefore
/// `Degraded`. Nothing was delivered; a workflow consumer must not read
/// that as success.
#[test]
fn fail_probe_errored_delivery_task_exits_non_zero() {
    let (home, flows) = fail_probe_fixture("exit 1");
    let out = launch_fail_probe(&home, &flows);
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(
        out.status.code(),
        Some(1),
        "a run whose delivery task errored must exit 1\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
}

/// (#2310 fix-loop C1 / S4-2) The review / crawl shape specifically: a
/// DELIVERY task in the last phase whose `depends_on` reaches back into an
/// earlier phase (`deliver` ← `create-mod` / `records-gather`) and which
/// declares `run_on: ["complete","error"]` so it survives an upstream
/// failure. Every edge in both built-in configs crosses a phase boundary,
/// which is exactly the set the per-phase scheduler split (#2300) made the
/// `run_on` contract inert over: `deliver` works there today only because
/// it happens to declare NO `depends_on`. Wire one up and, before this
/// packet, it never ran at all — its dependency sat `Planned` forever
/// instead of reaching the `Abandoned` its `run_on` accepts.
#[test]
fn fail_probe_cross_phase_delivery_task_runs_on_an_upstream_error() {
    let (home, flows) = fail_probe_fixture_with(
        "echo delivered:${DARKMUX_STEP_INPUT_T_DEP:-none}",
        r#""depends_on": ["t-dep"], "run_on": ["complete", "error"],"#,
    );
    let out = launch_fail_probe(&home, &flows);
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    let dir = one_mission_dir(&home);
    let steps = probe_steps(&dir);

    assert_eq!(
        steps["s-deliver"].0, "complete",
        "the delivery task must still run when its cross-phase dependency died\n\
         steps: {steps:#?}\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
    // …and it delivers KNOWING WHY: the cascade's origin text reaches it
    // through `t-dep`, two hops and two phase boundaries from `s-fail`.
    assert!(
        steps["s-deliver"].1.contains("s-fail"),
        "the delivery task must receive the ORIGINATING failure's reason as its input, \
         got {:?}",
        steps["s-deliver"].1
    );
    // And the whole-run invariant holds on this shape too.
    let live: Vec<_> = steps
        .iter()
        .filter(|(_, (status, _))| status == "planned" || status == "running")
        .collect();
    assert!(live.is_empty(), "terminal mission still holds non-terminal steps: {live:#?}");
}

// ─── forward-dependency probe (#2310 fix-loop C2 / C2-1) ───────────────
//
// The A/B the post-merge review of #2368 proved. `depends_on` ids are
// DOCUMENT-WIDE (see `TaskConfig::depends_on`), so a task in phase 1 may
// legally name a task in phase 2 — the per-phase scheduler split (#2300)
// runs every pass over the CUMULATIVE maps, so phase 2's pass runs it.
//
//   p1  t-early                                (no deps)
//       t-forward  depends_on ["t-late"]       (a FORWARD edge)
//   p2  t-late
//
// The first cut of the phase-exit sweep abandoned `t-forward` "not
// started" at p1's exit: declared work dropped, p1 closed Abandoned, exit
// 0, "0 errored". The whole shape must survive instead.
fn forward_dep_fixture() -> (TempDir, TempDir) {
    let home = TempDir::new().unwrap();
    let flows = TempDir::new().unwrap();
    let config_dir = home.path().join("mission-configs");
    fs::create_dir_all(&config_dir).unwrap();
    let config_json = r#"{
        "id": "forward-dep",
        "name": "Forward Dep Probe",
        "schema_version": "3.4",
        "phases": [
          {
            "id": "p1",
            "tasks": [
              { "id": "t-early",
                "steps": [{ "id": "s-early", "kind": "procedural.shell",
                            "config": { "command": "echo early-ran" } }] },
              { "id": "t-forward", "depends_on": ["t-late"],
                "steps": [{ "id": "s-forward", "kind": "procedural.shell",
                            "config": { "command": "echo forward-ran" } }] }
            ]
          },
          {
            "id": "p2",
            "tasks": [
              { "id": "t-late",
                "steps": [{ "id": "s-late", "kind": "procedural.shell",
                            "config": { "command": "echo late-ran" } }] }
            ]
          }
        ]
    }"#;
    fs::write(config_dir.join("forward-dep.json"), config_json).unwrap();
    (home, flows)
}

/// A forward `depends_on` still RUNS — and the phase that declared it
/// closes `Complete`, on disk and in the envelope.
#[test]
fn forward_dep_task_runs_in_the_later_phases_pass() {
    let (home, flows) = forward_dep_fixture();
    let out = darkmux_cmd()
        .env("DARKMUX_HOME", home.path())
        .env("DARKMUX_FLOWS_DIR", flows.path())
        .env("DARKMUX_LMS_BIN", "/usr/bin/true")
        .args(["mission", "launch", "forward-dep"])
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    let (dir, _mid, mission, phases) = probe_mission_and_phases(&home);
    let steps = probe_steps(&dir);

    assert_eq!(
        steps["s-forward"].0, "complete",
        "a forward `depends_on` must still run\nsteps: {steps:#?}\n{stdout}\n{stderr}"
    );
    assert!(
        steps["s-forward"].1.contains("forward-ran"),
        "the forward step's own work must actually happen, got {:?}\n{stdout}\n{stderr}",
        steps["s-forward"].1
    );
    for id in ["s-early", "s-late"] {
        assert_eq!(steps[id].0, "complete", "steps: {steps:#?}");
    }
    assert_eq!(steps.len(), 3, "steps: {steps:#?}");
    assert!(
        stdout.contains("3 step(s) complete, 0 errored"),
        "the summary must count all three\n{stdout}"
    );
    assert_eq!(out.status.code(), Some(0), "{stdout}\n{stderr}");

    // The phase that DECLARED the forward edge earned Complete — it is not
    // "abandoned" for having held a step its own pass could not run.
    assert_eq!(phases.get("p1").map(String::as_str), Some("complete"), "{phases:#?}");
    assert_eq!(phases.get("p2").map(String::as_str), Some("complete"), "{phases:#?}");
    assert_eq!(mission["status"], serde_json::json!("finalized"), "mission: {mission}");
    let envelope: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(dir.join("envelope.json")).unwrap()).unwrap();
    for phase in envelope["phases"].as_array().unwrap() {
        assert_eq!(
            phase["outcome"].as_str().map(str::to_ascii_lowercase).as_deref(),
            Some("complete"),
            "envelope: {envelope}"
        );
    }
}

/// (#2310 fix-loop C2 / S4-1) The same whole-run invariant on a CLEAN run:
/// persisting every minted step at mint time must not leave a `Planned`
/// file behind on a run where everything completed.
#[test]
fn clean_generic_run_leaves_no_non_terminal_step_behind() {
    let (home, flows, _plan) = grow_fixture(r#"{"units":[{"id":"u-1","rule":"r"}]}"#, "");
    let out = launch_grow(&home, &flows);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let steps = probe_steps(&one_mission_dir(&home));
    let live: Vec<_> = steps
        .iter()
        .filter(|(_, (status, _))| status != "complete")
        .collect();
    assert!(live.is_empty(), "a clean run must persist only Complete steps: {live:#?}");
}

/// (live proof 2026-09-05, frontier control) The create-mod message is the
/// one lever left after the gate learned to absorb three kit shapes
/// mechanically: local coder seats wrote kits in container coordinates, with
/// miscounted hunk headers, in code fences, and without a terminating
/// newline — a clean-context frontier seat given the SAME message wrote
/// four of four kits that applied raw.
///
/// (#2310 P4e) This test used to assert the message was byte-identical in
/// `crawl.json` and `review.json`. P4e made review's create-mods task a
/// bounded WAIT with no local seat and no message at all, so for one packet
/// the message was `crawl.json`'s alone.
///
/// (#2310 P4f) It is SHARED again, by a different task: review now also
/// ships `create-mod-dispatch`, off by default, which staffs the same seat
/// with a `coder` on a HOSTED ENDPOINT profile for the unattended runner.
/// That template does read a message, and it must be the same one, because
/// it is the same job — read the finding, write the smallest applying diff,
/// call `create_mod`. Its kit-shape clauses are what the measurement found
/// local seats failing and a frontier-class seat passing; a divergence
/// between the two configs would mean two different specifications of the
/// artifact `mods.gate` has to `git apply`, drifting silently.
///
/// The WAIT template still carries no message and must not grow one — that
/// would mean a local seat came back into the attended path.
#[test]
fn the_create_mod_message_names_the_kit_shape_and_is_shared_by_both_configs() {
    fn create_mod_messages(doc: &str) -> Vec<String> {
        let v: serde_json::Value = serde_json::from_str(doc).expect("built-in config parses");
        let mut found = Vec::new();
        fn walk(x: &serde_json::Value, out: &mut Vec<String>) {
            match x {
                serde_json::Value::Object(m) => {
                    if let Some(serde_json::Value::String(s)) = m.get("message") {
                        if s.contains("PROPOSE the change") {
                            out.push(s.clone());
                        }
                    }
                    m.values().for_each(|v| walk(v, out));
                }
                serde_json::Value::Array(a) => a.iter().for_each(|v| walk(v, out)),
                _ => {}
            }
        }
        walk(&v, &mut found);
        found
    }
    let mut crawl = create_mod_messages(include_str!("../templates/builtin/mission-configs/crawl.json"));
    assert_eq!(crawl.len(), 1, "exactly one create-mod message in crawl.json: {crawl:?}");
    let crawl = crawl.remove(0);
    // (#2310 P4f) The review config carries EXACTLY ONE, and it is the
    // `create-mod-dispatch` template's — the endpoint seat. The WAIT
    // template must still carry none.
    let mut review =
        create_mod_messages(include_str!("../templates/builtin/mission-configs/review.json"));
    assert_eq!(
        review.len(),
        1,
        "exactly one create-mod message in review.json — the `create-mod-dispatch` template's; the \
         attended `create-mod` template WAITS for a frontier mod and must never grow a message of its \
         own: {review:?}"
    );
    let review = review.remove(0);
    assert_eq!(
        review, crawl,
        "the create-mod message must be BYTE-IDENTICAL in crawl.json and review.json — same job, \
         same specification of the kit `mods.gate` has to `git apply`"
    );
    // And it must hang off `create-mod-dispatch`, not off the wait
    // template: the message reaching the wrong task would pass the
    // byte-identity assertion above while meaning the opposite thing.
    let doc: serde_json::Value =
        serde_json::from_str(include_str!("../templates/builtin/mission-configs/review.json"))
            .expect("review.json parses");
    let tasks = doc["phases"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["id"] == serde_json::json!("create-mods"))
        .expect("the create-mods phase")["tasks"]
        .as_array()
        .unwrap();
    let dispatch = tasks
        .iter()
        .find(|t| t["id"] == serde_json::json!("create-mod-dispatch"))
        .expect("the create-mod-dispatch template");
    assert_eq!(dispatch["grow"]["config"]["message"], serde_json::json!(review));
    let wait = tasks
        .iter()
        .find(|t| t["id"] == serde_json::json!("create-mod"))
        .expect("the create-mod wait template");
    assert!(wait["grow"]["config"]["message"].is_null(), "the wait template carries no message");
    for needle in [
        "relative to the repository root",
        "exactly as they appear",
        "not inside a code fence",
        "end with a newline",
        "mounted at `/workspace/<source>`",
        "A diff written into your reply is not a mod",
    ] {
        assert!(crawl.contains(needle), "the message must name the kit shape ({needle:?}):\n{crawl}");
    }
}

// ─── #2310 P4f: the unattended cloud seat, off by default ──────────────
//
// `create-mod-dispatch` is the SECOND way to staff the create-mods seat: a
// `coder` on a hosted-endpoint profile, for the self-hosted runner where no
// orchestrator session exists to receive the create_finding hook and a
// bounded wait therefore waits for nothing. It ships `enabled: false` and
// `excludes: ["create-mod"]`.

/// The document as SHIPPED validates clean — the new input and the new
/// template add no findings, including on the inputs path (`{{mod_seat_
/// profile}}` is a declared, optional, whole-value placeholder, so neither
/// the undeclared-placeholder check nor the embedded-optional warning has
/// anything to say about it).
#[test]
fn review_ships_both_mod_seat_templates_and_validates_clean() {
    let cfg: darkmux_crew::mission_config::MissionConfig =
        serde_json::from_str(include_str!("../templates/builtin/mission-configs/review.json"))
            .expect("review.json parses");
    let findings = cfg.validate(&[]);
    assert!(findings.is_empty(), "review as shipped must validate clean: {findings:?}");

    // Defaults: the wait template is live, the endpoint template is not.
    let tasks: Vec<_> = cfg
        .phases
        .iter()
        .find(|p| p.id == "create-mods")
        .expect("the create-mods phase")
        .tasks
        .iter()
        .collect();
    let wait = tasks.iter().find(|t| t.id == "create-mod").expect("the wait template");
    let seat = tasks
        .iter()
        .find(|t| t.id == "create-mod-dispatch")
        .expect("the endpoint template");
    assert!(wait.is_enabled(), "the attended wait template stays the default");
    assert!(!seat.is_enabled(), "the endpoint seat ships OFF");
    assert_eq!(seat.excludes, vec!["create-mod".to_string()]);
    assert_eq!(seat.role_id.as_deref(), Some("coder"));
    // The seat is pinned by the step's own `profile_name` override — the
    // key `dispatch.internal` reads (`builtins::task_or_config_str`) — fed
    // from the `mod_seat_profile` input.
    assert_eq!(
        seat.grow.as_ref().expect("grow").config["profile_name"],
        serde_json::json!("{{mod_seat_profile}}"),
        "the endpoint seat pins its profile through the step config `dispatch.internal` reads"
    );
    assert!(
        cfg.inputs.iter().any(|i| i.name == "mod_seat_profile" && i.required != Some(true)),
        "and `mod_seat_profile` is a declared, optional input"
    );
    // Mutation guard for the `default: 0` the P4e packet set: the endpoint
    // seat must not have changed the wait's own default out from under an
    // attended operator.
    let wait_default = cfg
        .inputs
        .iter()
        .find(|i| i.name == "mod_wait_seconds")
        .and_then(|i| i.default.clone());
    assert_eq!(wait_default, Some(serde_json::json!("0")), "the wait's `0 = skip` default is untouched");
}

/// Both seats enabled is a validate-time `Error` naming BOTH tasks — two
/// mod-writing tasks per finding is not a degraded run, it is a duplicated
/// seat, and the operator's fix is one `enabled` field.
#[test]
fn enabling_both_mod_seat_templates_is_a_validate_error_naming_them() {
    let mut doc: serde_json::Value =
        serde_json::from_str(include_str!("../templates/builtin/mission-configs/review.json"))
            .expect("review.json parses");
    let phase = doc["phases"]
        .as_array_mut()
        .unwrap()
        .iter_mut()
        .find(|p| p["id"] == serde_json::json!("create-mods"))
        .expect("the create-mods phase");
    let tasks = phase["tasks"].as_array_mut().unwrap();
    for t in tasks.iter_mut() {
        t["enabled"] = serde_json::json!(true);
    }
    let cfg: darkmux_crew::mission_config::MissionConfig =
        serde_json::from_value(doc).expect("the edited document parses");
    let errors: Vec<_> = cfg
        .validate(&[])
        .into_iter()
        .filter(|f| f.severity == darkmux_crew::mission_config::FindingSeverity::Error)
        .collect();
    assert_eq!(errors.len(), 1, "exactly one finding for the one conflicting pair: {errors:?}");
    let hit = &errors[0];
    assert!(hit.path.ends_with("excludes"), "path names the field to fix: {}", hit.path);
    assert!(
        hit.message.contains("create-mod\"") && hit.message.contains("create-mod-dispatch"),
        "the finding names BOTH templates: {}",
        hit.message
    );
}

/// The shared fixture for the endpoint-seat tests: a `DARKMUX_HOME` holding
/// a user-tier `review.json` with the two `enabled` fields flipped
/// (exactly how a runner opts in), a workspace spec, an empty diff, and a
/// profile registry defining `grok-endpoint`.
///
/// The registry is written to a temp file and reached through
/// `DARKMUX_PROFILES` deliberately: `profiles::default_locations()` does
/// NOT honor `DARKMUX_HOME` — it searches the cwd and the REAL `$HOME` —
/// so a test that merely sets `DARKMUX_HOME` and names a profile would read
/// whatever registry the developer happens to have, and pass or fail on
/// their machine's contents.
struct EndpointSeatFixture {
    _workdir: TempDir,
    home: TempDir,
    spec_path: std::path::PathBuf,
    diff_path: std::path::PathBuf,
    profiles_path: std::path::PathBuf,
}

fn endpoint_seat_fixture() -> EndpointSeatFixture {
    let workdir = TempDir::new().unwrap();
    let home = TempDir::new().unwrap();
    let workspace_root = workdir.path().join("tree");
    fs::create_dir_all(&workspace_root).unwrap();
    let spec_path = workdir.path().join("workspace.json");
    fs::write(
        &spec_path,
        serde_json::json!({
            "name": "review-fixture",
            "sources": [{"id": "app", "path": workspace_root.to_string_lossy(), "ref": "main"}]
        })
        .to_string(),
    )
    .unwrap();
    let diff_path = workdir.path().join("d.diff");
    fs::write(&diff_path, "").unwrap();

    // An ENDPOINT profile, so nothing here is a local placement. Nothing is
    // ever called at this URL — every test using this fixture stops at the
    // dry run.
    let profiles_path = workdir.path().join("profiles.json");
    fs::write(
        &profiles_path,
        serde_json::json!({
            "schema_version": "1.5",
            "default_profile": "local-default",
            "profiles": {
                "local-default": {"models": [{"id": "local-model", "n_ctx": 8000}]},
                "grok-endpoint": {"models": [
                    {"id": "grok-model", "n_ctx": 8000, "endpoint": {"url": "http://127.0.0.1:9"}}
                ]}
            }
        })
        .to_string(),
    )
    .unwrap();

    // The built-in document, copied to the user tier with TWO fields
    // flipped — the whole opt-in.
    let mut doc: serde_json::Value =
        serde_json::from_str(include_str!("../templates/builtin/mission-configs/review.json"))
            .expect("review.json parses");
    {
        let phase = doc["phases"]
            .as_array_mut()
            .unwrap()
            .iter_mut()
            .find(|p| p["id"] == serde_json::json!("create-mods"))
            .expect("the create-mods phase");
        for t in phase["tasks"].as_array_mut().unwrap() {
            t["enabled"] = serde_json::json!(t["id"] == serde_json::json!("create-mod-dispatch"));
        }
    }
    let config_dir = home.path().join("mission-configs");
    fs::create_dir_all(&config_dir).unwrap();
    fs::write(config_dir.join("review.json"), doc.to_string()).unwrap();

    EndpointSeatFixture { _workdir: workdir, home, spec_path, diff_path, profiles_path }
}

fn endpoint_seat_dry_run(fx: &EndpointSeatFixture, seat: &str) -> std::process::Output {
    darkmux_cmd()
        .args([
            "mission",
            "launch",
            "review",
            "--dry-run",
            "--param",
            &format!("workspace={}", fx.spec_path.display()),
            "--param",
            &format!("diff_file={}", fx.diff_path.display()),
            "--param",
            &format!("mod_seat_profile={seat}"),
        ])
        .env("DARKMUX_HOME", fx.home.path())
        .env("DARKMUX_FLOWS_DIR", fx.home.path().join("flows"))
        .env("DARKMUX_LMS_BIN", "/usr/bin/true")
        .env("DARKMUX_PROFILES", &fx.profiles_path)
        .output()
        .expect("mission launch review --dry-run runs")
}

/// A user-tier copy with the two `enabled` fields flipped — exactly how a
/// runner opts in — mints the coder dispatch and NOT the wait. The
/// dry-run graph is where the operator sees which seat is live: the other
/// template is pruned at mint, never drawn gray.
#[test]
fn review_dry_run_shows_the_endpoint_seat_when_a_user_tier_copy_enables_it() {
    let fx = endpoint_seat_fixture();
    let out = endpoint_seat_dry_run(&fx, "grok-endpoint");
    assert!(out.status.success(), "stderr: {}", String::from_utf8_lossy(&out.stderr));
    let stdout = String::from_utf8_lossy(&out.stdout);

    assert!(stdout.contains("Create mods"), "the phase is in the graph:\n{stdout}");
    assert!(
        stdout.contains("dispatch.internal"),
        "and its step is the coder dispatch:\n{stdout}"
    );
    // `procedural.shell` appears exactly once in this document — the wait
    // step — so its ABSENCE is the proof the wait template was pruned.
    assert!(
        !stdout.contains("procedural.shell"),
        "the wait template is pruned at mint, so no wait step is drawn:\n{stdout}"
    );
    assert!(!fx.home.path().join("missions").exists(), "a dry run mints nothing");
}

/// (#2310 P4f review, CONSIDER 3) A typo'd `mod_seat_profile` is REFUSED,
/// naming the value and pointing at `darkmux profile list`.
///
/// Without this the launch proceeded and the seat silently became the
/// machine's local default: `ProfileRegistry::resolve_active` falls back to
/// `default_profile` when the requested name is undefined — deliberately,
/// and documented as such, so a machine-agnostic caller can name a profile
/// each machine may or may not define. That contract's own doc says the
/// caller "can detect a fallback (resolved name != requested name) and
/// surface it"; nothing on this path did. The operator asked for a cloud
/// seat, got a local model, and every flow record still read
/// `handle: coder` — a substitution with no signal anywhere (#44: never
/// silently substitute the operator's stated intent).
#[test]
fn a_mod_seat_profile_naming_no_defined_profile_refuses_the_launch() {
    let fx = endpoint_seat_fixture();
    let out = endpoint_seat_dry_run(&fx, "grok-endpiont");
    assert!(
        !out.status.success(),
        "a mod_seat_profile naming nothing must refuse, not fall back to the default profile.\n\
         stdout: {}",
        String::from_utf8_lossy(&out.stdout)
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("grok-endpiont"), "the refusal names the typo verbatim: {stderr}");
    assert!(
        stderr.contains("mod_seat_profile"),
        "and names the input it came from: {stderr}"
    );
    assert!(
        stderr.contains("darkmux profile list"),
        "and points at the command that lists the real names: {stderr}"
    );
    // The silent-substitution tell: the refusal must not read as if the
    // default were an acceptable stand-in.
    assert!(
        !out.status.success() && !stderr.contains("falling back"),
        "a fallback is exactly what this refuses to do: {stderr}"
    );
}

/// The positive leg: a DEFINED name proceeds. Without it the test above
/// would pass against a check that refused every launch.
#[test]
fn a_mod_seat_profile_naming_a_defined_profile_proceeds() {
    let fx = endpoint_seat_fixture();
    let out = endpoint_seat_dry_run(&fx, "grok-endpoint");
    assert!(
        out.status.success(),
        "a defined profile must launch: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        String::from_utf8_lossy(&out.stdout).contains("dispatch.internal"),
        "and still reaches the endpoint seat's dispatch step"
    );
}

// ─── #2310 P4e: review's create-mods waits for a frontier mod ────────
//
// The seat that writes a mod moved off the local tier (see the config's own
// `create-mods` phase description). What ships in `review.json` is
// therefore not a coder dispatch but a shell command, and a shell command in
// a JSON document is exactly the kind of artifact that rots silently. These
// tests execute the SHIPPED BYTES: `create_mod_wait_command` pulls the
// command out of the embedded config and substitutes only the two
// placeholders the launcher/grow passes would have substituted, so a change
// to the command text is a change these tests see.

/// The `create-mod` task's wait command, as shipped, with `{{item.key}}`
/// (grow's namespace) and `{{mod_wait_seconds}}` (the launch-input
/// namespace) resolved the way the two real substitution passes resolve
/// them.
fn create_mod_wait_command(finding_key: &str, bound: &str) -> String {
    let doc: serde_json::Value =
        serde_json::from_str(include_str!("../templates/builtin/mission-configs/review.json"))
            .expect("review.json parses");
    let phase = doc["phases"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["id"] == serde_json::json!("create-mods"))
        .expect("a create-mods phase");
    let raw = phase["tasks"][0]["grow"]["config"]["command"]
        .as_str()
        .expect("the create-mod task's grow config carries a shell command");
    raw.replace("{{item.key}}", finding_key).replace("{{mod_wait_seconds}}", bound)
}

/// Record a mod naming `finding_key` through the real `mod create` verb, in
/// the store `home` roots — the same call the `darkmux-mod-create` skill's
/// subagent makes.
fn record_mod_for(home: &std::path::Path, workdir: &std::path::Path, finding_key: &str) {
    let kit = workdir.join(format!("kit-{}.diff", finding_key.replace('/', "-")));
    fs::write(&kit, "--- a/src/a.ts\n+++ b/src/a.ts\n@@ -1 +1 @@\n-old\n+new\n").unwrap();
    let out = darkmux_cmd()
        .env("DARKMUX_HOME", home)
        .args([
            "mod",
            "create",
            "--by",
            "frontier",
            "--for",
            finding_key,
            "--kit-kind",
            "unified-diff",
            // (#2386) These fixtures seed a mod for a finding key they never
            // store — exactly the case the flag exists for. Without it the
            // seed is refused, which is the new contract working.
            "--allow-missing-finding",
            "--kit",
            &kit.to_string_lossy(),
        ])
        .output()
        .expect("mod create runs");
    assert!(
        out.status.success(),
        "seeding a mod failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// Run the shipped wait command with a real store behind it.
fn run_wait_command(home: &std::path::Path, command: &str) -> std::process::Output {
    // (#2184) The child here is `sh`, not darkmux — but the shell script it
    // runs invokes `$DARKMUX_BIN`, so the isolation still has to be applied,
    // and the structural guard cannot see it (nothing on this line names the
    // binary except through the helper). `DARKMUX_HOME` is the caller's on
    // purpose: these tests seed a mod store there and the command must read
    // the same one. `HOME` is a throwaway so the accessors that resolve
    // through `dirs::home_dir()` instead (see `darkmux_std_cmd`'s doc) can't
    // reach the operator either.
    std::process::Command::new("sh")
        .arg("-c")
        .arg(command)
        .env("HOME", isolated_roots().0)
        .env("DARKMUX_HOME", home)
        .env("DARKMUX_BIN", darkmux_bin_path())
        .output()
        .expect("the wait command runs")
}

/// (#2310 P4e) The SHAPE of the change: no local seat is staffed for mod
/// creation any more. Red-proved by restoring `dispatch.internal` as the
/// first step — every assertion below fails.
#[test]
fn review_create_mods_waits_for_a_mod_instead_of_dispatching_a_coder() {
    let doc: serde_json::Value =
        serde_json::from_str(include_str!("../templates/builtin/mission-configs/review.json"))
            .expect("review.json parses");

    let input = doc["inputs"]
        .as_array()
        .unwrap()
        .iter()
        .find(|i| i["name"] == serde_json::json!("mod_wait_seconds"))
        .expect("review declares mod_wait_seconds");
    assert_eq!(input["required"], serde_json::json!(false));
    // The unattended path (the self-hosted runner) has no orchestrator
    // session to receive the hook and no frontier seat, so the DEFAULT must
    // be "do not wait".
    assert_eq!(
        input["default"],
        serde_json::json!("0"),
        "the default must be 0 — a CI runner waiting per finding is pure wall-clock"
    );

    let phase = doc["phases"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["id"] == serde_json::json!("create-mods"))
        .expect("a create-mods phase");
    let task = &phase["tasks"][0];
    assert!(
        task.get("role_id").is_none(),
        "the create-mod task staffs no seat: {task}"
    );
    assert!(
        task["grow"]["config"].get("message").is_none()
            && task["grow"]["config"].get("brief_refs").is_none(),
        "a dispatch brief here would mean a local coder is still being asked to write the kit: {task}"
    );
    let kinds: Vec<&str> =
        task["steps"].as_array().unwrap().iter().map(|s| s["kind"].as_str().unwrap()).collect();
    assert_eq!(
        kinds,
        vec!["procedural.shell", "mods.gate"],
        "wait then gate — no dispatch step: {task}"
    );
    let command = task["grow"]["config"]["command"].as_str().unwrap();
    for needle in ["{{item.key}}", "{{mod_wait_seconds}}", "mod list --for", "DARKMUX_BIN"] {
        assert!(command.contains(needle), "the wait command must name {needle:?}:\n{command}");
    }
}

/// (#2310 P4e) A mod already in the store when the step starts: the wait
/// returns at once. Red-proved by inverting `grep -q` to `grep -qv` — the
/// command then runs the full bound and exits 1.
#[test]
fn the_wait_command_returns_at_once_when_a_mod_already_names_the_finding() {
    let home = TempDir::new().unwrap();
    let workdir = TempDir::new().unwrap();
    record_mod_for(home.path(), workdir.path(), "sess-already/1");

    let started = std::time::Instant::now();
    let out = run_wait_command(home.path(), &create_mod_wait_command("sess-already/1", "60"));
    let elapsed = started.elapsed();

    let stdout = String::from_utf8_lossy(&out.stdout).to_string();
    assert!(out.status.success(), "stdout: {stdout}\nstderr: {}", String::from_utf8_lossy(&out.stderr));
    assert!(stdout.contains("\"waited\":true"), "{stdout}");
    assert!(
        elapsed < std::time::Duration::from_secs(10),
        "an already-recorded mod must not cost a poll cycle, took {elapsed:?}"
    );
}

/// (#2310 P4e) The real shape of the loop: the mod does not exist when the
/// step starts and the frontier records it while the step is waiting.
/// Red-proved by deleting the `while` loop's `sleep`/re-check (a single
/// probe then exit) — the mod lands after the probe and the step errors.
#[test]
fn the_wait_command_completes_when_the_mod_appears_mid_wait() {
    let home = TempDir::new().unwrap();
    let workdir = TempDir::new().unwrap();
    let home_path = home.path().to_path_buf();
    let workdir_path = workdir.path().to_path_buf();

    let writer = std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_secs(2));
        record_mod_for(&home_path, &workdir_path, "sess-midwait/3");
    });

    let out = run_wait_command(home.path(), &create_mod_wait_command("sess-midwait/3", "60"));
    writer.join().unwrap();

    let stdout = String::from_utf8_lossy(&out.stdout).to_string();
    assert!(
        out.status.success(),
        "the wait must see a mod recorded after it started: stdout {stdout}\nstderr {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(stdout.contains("\"waited\":true"), "{stdout}");
    assert!(stdout.contains("sess-midwait/3"), "the output names the finding it waited for: {stdout}");
}

/// (#2310 P4e review, item 8) No mod inside the bound is a CLEAN outcome,
/// not an error. A frontier that read the finding and declined to write a
/// mod — because it is a search-form finding, or because the finding does
/// not hold — is the correct behavior, and the skill instructs it
/// explicitly. Exiting non-zero there made every decline an Error step and
/// finalized the run Degraded, which reads as "darkmux broke" for the one
/// path the design calls right. So the bound-exhausted branch exits 0 with
/// `found: false`; the gate then records its ordinary no-mod skip and the
/// deliverer renders the finding as a question. A non-numeric bound is
/// still exit 2 (a real config defect), and an infra failure still errors.
///
/// The store is deliberately NOT empty: it holds a mod for a DIFFERENT
/// finding, because the probe's whole job is `mod list --for <this key>`
/// and an empty store would pass just as well with the filter dropped.
/// Red-proved three ways: change the terminal `exit 1`/`exit 0` back to
/// `exit 1` (the status assertion fails); delete `--for "$key"` from the
/// command (the unrelated mod satisfies this finding's wait and `found`
/// comes back true); flip the terminal `"found":false` to `"found":true`.
#[test]
fn the_wait_command_completes_with_found_false_when_no_mod_appears_within_the_bound() {
    let home = TempDir::new().unwrap();
    let workdir = TempDir::new().unwrap();
    record_mod_for(home.path(), workdir.path(), "sess-someone-else/2");
    let out = run_wait_command(home.path(), &create_mod_wait_command("sess-never/9", "3"));

    let stdout = String::from_utf8_lossy(&out.stdout).to_string();
    let stderr = String::from_utf8_lossy(&out.stderr).to_string();
    assert!(
        out.status.success(),
        "a decline is a clean outcome, never a step error: stdout {stdout}\nstderr {stderr}"
    );
    assert!(
        stdout.contains("\"found\":false"),
        "and it says so in its output, so the gate and the deliverer can tell: {stdout}"
    );
    assert!(
        stdout.contains("\"waited\":true"),
        "the wait DID run — `waited` reports that, `found` reports the outcome: {stdout}"
    );
    assert!(stdout.contains("sess-never/9"), "naming the finding it waited for: {stdout}");
    assert!(stderr.contains("sess-never/9"), "the note names the finding: {stderr}");
    assert!(stderr.contains("3s"), "and the bound it waited: {stderr}");
}

/// Run the shipped wait command with `DARKMUX_BIN` overridden — for tests
/// that need the `mod list` call itself to fail, rather than the real
/// binary under test.
fn run_wait_command_with_bin(
    home: &std::path::Path,
    command: &str,
    darkmux_bin: &str,
) -> std::process::Output {
    std::process::Command::new("sh")
        .arg("-c")
        .arg(command)
        .env("HOME", isolated_roots().0)
        .env("DARKMUX_HOME", home)
        .env("DARKMUX_BIN", darkmux_bin)
        .output()
        .expect("the wait command runs")
}

/// (#2552) `mod list` never runs at all — the binary the step calls back
/// into does not exist (a worktree build not yet installed, a `PATH` a
/// launcher forgot to carry). Before the fix, this poll's own `if ... |
/// grep -q '"key"'` collapsed a 127 (command not found) into the same
/// branch as "no mod yet" — silently discarded by `2>/dev/null` — and the
/// step polled for the FULL bound before reporting `found: false`, naming
/// no cause. Now it must fail on the FIRST probe, naming the exit code and
/// what the shell said, so an operator debugging a stuck wait is pointed at
/// the binary rather than at the mod-writing step.
///
/// Red-proved: reverting this poll to the shipped
/// `... 2>/dev/null | grep -q '"key"'` shape turns this red — the step
/// exits 0 with `found:false` after burning the whole bound instead of
/// failing on the first probe.
#[test]
fn the_wait_command_fails_fast_when_the_darkmux_binary_is_missing() {
    let home = TempDir::new().unwrap();
    let started = std::time::Instant::now();
    let out = run_wait_command_with_bin(
        home.path(),
        &create_mod_wait_command("sess-nobinary/1", "60"),
        "/nonexistent/path/darkmux",
    );
    let elapsed = started.elapsed();

    let stdout = String::from_utf8_lossy(&out.stdout).to_string();
    let stderr = String::from_utf8_lossy(&out.stderr).to_string();
    assert!(
        !out.status.success(),
        "a missing binary must fail the step, not report a clean no-mod outcome: stdout {stdout}\nstderr {stderr}"
    );
    assert!(
        stderr.contains("mod list failed"),
        "the failure names WHICH command failed, not just that something did: {stderr}"
    );
    assert!(
        stderr.contains("exit"),
        "and the exit status, so a missing binary (127) reads differently from a real error: {stderr}"
    );
    // The word "exit" alone would still pass a regression to a flat
    // `exit 1` with a generic message — assert the real shell "command not
    // found" code the wait command's `exit "$rc"` actually propagates.
    assert_eq!(
        out.status.code(),
        Some(127),
        "a missing binary is shell exit 127 (command not found), not a made-up 1: stdout {stdout}\nstderr {stderr}"
    );
    assert!(
        !stdout.contains("\"found\":false"),
        "must not report the clean-decline shape for an infra failure: {stdout}"
    );
    assert!(
        elapsed < std::time::Duration::from_secs(10),
        "a missing binary is diagnosable on the FIRST probe — it must not burn the 60s bound: {elapsed:?}"
    );
}

/// (#2552) `mod list` runs and errors (here: a `--for` key that cannot
/// address any finding, refused by `canonical_finding_key` before the
/// store is even read) — a real infra/config failure, distinct from both
/// "no mod yet" and "the binary is missing". Same collapse as the test
/// above: the shipped poll's `2>/dev/null | grep -q` swallowed this error
/// and treated it exactly like "not found yet", polling for the full bound.
///
/// Uses a STUB rather than the real `mod list` invocation, on purpose: the
/// real anyhow-based failure for an invalid key happens to exit 1 — the
/// SAME number a hardcoded `exit "$rc"` -> `exit 1` regression would also
/// produce, so pinning `Some(1)` against the real invocation cannot tell a
/// genuinely propagated code from a flattened one; both read as 1 (a
/// frontier review of this file found exactly that: mutating the
/// propagation to a literal `exit 1` left this test green, because 1 == 1
/// by coincidence — only the missing-binary test's 127 caught it). The
/// stub exits 42, a number nothing in this command's normal operation
/// produces, so the assertion below can only pass if `$rc` genuinely
/// reaches `exit "$rc"` unmodified.
///
/// Red-proved two ways: (1) reverting to the shipped `2>/dev/null | grep
/// -q` shape turns this red (burns the full 60s bound instead of failing
/// fast); (2) flattening `exit "$rc"` to a literal `exit 1` ALSO turns this
/// red now (`Some(42)` != `Some(1)`) — the coincidence-pass window this
/// test used to have is closed.
#[test]
fn the_wait_command_fails_fast_when_mod_list_itself_errors() {
    let home = TempDir::new().unwrap();
    let stub_dir = TempDir::new().unwrap();
    let stub = stub_dir.path().join("fake-darkmux");
    fs::write(
        &stub,
        "#!/bin/sh\nprintf 'not a finding key: \"not-a-valid-key\" (expected <dispatch>/<seq>, e.g. sess-abc/1)\\n' >&2\nexit 42\n",
    )
    .unwrap();
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&stub, fs::Permissions::from_mode(0o755)).unwrap();
    }

    let started = std::time::Instant::now();
    // Not `<dispatch>/<seq>` — real `mods::canonical_finding_key` would
    // refuse this before the store is even read; the stub mirrors that
    // shape with a distinctive exit code instead of the real one (see doc
    // comment above for why).
    let out = run_wait_command_with_bin(
        home.path(),
        &create_mod_wait_command("not-a-valid-key", "60"),
        &stub.to_string_lossy(),
    );
    let elapsed = started.elapsed();

    let stdout = String::from_utf8_lossy(&out.stdout).to_string();
    let stderr = String::from_utf8_lossy(&out.stderr).to_string();
    assert!(
        !out.status.success(),
        "a real `mod list` error must fail the step: stdout {stdout}\nstderr {stderr}"
    );
    assert!(
        stderr.contains("mod list failed"),
        "the failure names WHICH command failed: {stderr}"
    );
    assert!(
        stderr.contains("not a finding key") || stderr.contains("not-a-valid-key"),
        "and carries the real command's own error text, not a generic message: {stderr}"
    );
    assert_eq!(
        out.status.code(),
        Some(42),
        "the stub's distinctive exit code must reach here unmodified — a hardcoded `exit 1` \
         regression would report 1, not 42, and this is the assertion that would catch it: \
         stdout {stdout}\nstderr {stderr}"
    );
    assert!(
        !stdout.contains("\"found\":false"),
        "must not report the clean-decline shape for a real error: {stdout}"
    );
    assert!(
        elapsed < std::time::Duration::from_secs(10),
        "a config/infra error is diagnosable on the FIRST probe — it must not burn the 60s bound: {elapsed:?}"
    );
}

/// A `mod list --for` that exits 0 with an empty `{"mods": []}` on stdout
/// but happens to write a line containing the five bytes `"key"` to
/// stderr (a wrapper's startup warning, loader noise — `DARKMUX_BIN` is
/// not guaranteed to be darkmux itself, see `builtins.rs`'s
/// `current_exe()` default) must NOT be reported as `found: true`. The
/// probe now redirects `mod list`'s stderr to a temp file and greps only
/// stdout — before that fix the probe merged stderr into the string it
/// grepped (`2>&1`), so this exact stub produced `found: true` with
/// nothing ever generated.
///
/// Red-proved: reverting the probe's `2>"$errf"` back to `2>&1` (and the
/// grepped variable back to the merged one) turns this red — the stub's
/// stderr line satisfies the `grep -q '"key"'` check on the FIRST probe.
#[test]
fn the_wait_command_does_not_false_positive_on_key_text_in_mod_list_stderr() {
    let home = TempDir::new().unwrap();
    let stub_dir = TempDir::new().unwrap();
    let stub = stub_dir.path().join("fake-darkmux");
    fs::write(
        &stub,
        "#!/bin/sh\nprintf 'warning: unrecognized \"key\" in config\\n' >&2\nprintf '{\"mods\": []}\\n'\nexit 0\n",
    )
    .unwrap();
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&stub, fs::Permissions::from_mode(0o755)).unwrap();
    }

    let started = std::time::Instant::now();
    let out = run_wait_command_with_bin(
        home.path(),
        &create_mod_wait_command("sess-falsepos/1", "3"),
        &stub.to_string_lossy(),
    );
    let elapsed = started.elapsed();

    let stdout = String::from_utf8_lossy(&out.stdout).to_string();
    let stderr = String::from_utf8_lossy(&out.stderr).to_string();
    assert!(
        out.status.success(),
        "an exit-0 mod list, however noisy on stderr, is not a step failure: stdout {stdout}\nstderr {stderr}"
    );
    assert!(
        !stdout.contains("\"found\":true"),
        "nothing was generated — `\"key\"` landing on stderr must not read as a match: stdout {stdout}\nstderr {stderr}"
    );
    assert!(
        stdout.contains("\"found\":false"),
        "the honest outcome for an empty store is the clean decline: {stdout}"
    );
    assert!(
        elapsed >= std::time::Duration::from_secs(3) && elapsed < std::time::Duration::from_secs(15),
        "the poll burns its full 3s bound rather than false-matching on the first probe: {elapsed:?}"
    );
}

/// (#2310 P4e, operator refinement) `mod_wait_seconds=0` — the DEFAULT, and
/// the unattended path's behavior — is "do not wait", not "wait zero
/// seconds and then check". It completes immediately with an honest output
/// saying no frontier monitor was expected, so the gate finds no mod and
/// the deliverer renders the finding as a question exactly as today.
///
/// Red-proved by deleting the `if [ "$bound" -eq 0 ]` early return: the loop
/// then probes the store once, finds nothing, and exits 1, so a CI run's
/// every finding would surface a step error instead of a detection.
#[test]
fn the_wait_command_does_not_wait_at_all_when_the_bound_is_zero() {
    let home = TempDir::new().unwrap();
    let started = std::time::Instant::now();
    let out = run_wait_command(home.path(), &create_mod_wait_command("sess-unattended/1", "0"));
    let elapsed = started.elapsed();

    let stdout = String::from_utf8_lossy(&out.stdout).to_string();
    assert!(
        out.status.success(),
        "0 must complete the step, never error it: stdout {stdout}\nstderr {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(stdout.contains("\"waited\":false"), "{stdout}");
    assert!(stdout.contains("no frontier monitor expected"), "the reason is in the output: {stdout}");
    assert!(
        elapsed < std::time::Duration::from_secs(2),
        "0 must not consult the store or sleep, took {elapsed:?}"
    );
}

/// (#2310 P4e) The document default reaches a real launch. `--dry-run`
/// prints the resolved inputs, which is the surface an operator reads to
/// see what a launch will use — so this proves both the new
/// `MissionInput::default` plumbing and that `review` ships `0`.
/// Red-proved by removing `apply_input_defaults`'s call site: the line
/// disappears from the dry-run output and a real launch is refused for an
/// uncollected embedded placeholder.
#[test]
fn review_dry_run_resolves_mod_wait_seconds_to_the_documents_default() {
    let workdir = TempDir::new().unwrap();
    let home = TempDir::new().unwrap();
    let flows = TempDir::new().unwrap();
    let app = workdir.path().join("app");
    write_app_repo(&app, "^1.0.0");
    let spec_path = workdir.path().join("workspace.json");
    fs::write(
        &spec_path,
        serde_json::json!({
            "name": "review-default-inputs",
            "sources": [{"id": "app", "path": app.to_string_lossy(), "ref": "main"}]
        })
        .to_string(),
    )
    .unwrap();
    let diff_path = workdir.path().join("d.diff");
    fs::write(&diff_path, "").unwrap();

    let out = darkmux_cmd()
        .env("DARKMUX_HOME", home.path())
        .env("DARKMUX_FLOWS_DIR", flows.path())
        .env("DARKMUX_LMS_BIN", "/usr/bin/true")
        .args([
            "mission",
            "launch",
            "review",
            "--param",
            &format!("workspace={}", spec_path.display()),
            "--param",
            &format!("diff_file={}", diff_path.display()),
            "--dry-run",
        ])
        .output()
        .expect("mission launch review --dry-run runs");
    let stdout = String::from_utf8_lossy(&out.stdout).to_string();
    assert!(
        stdout.contains("mod_wait_seconds = 0"),
        "the document's default must resolve into the launch's own inputs:\n{stdout}"
    );
}

/// (#2310 P4e) An operator-supplied value beats the document default —
/// the launcher must never substitute its own judgment for a typed one.
#[test]
fn a_supplied_mod_wait_seconds_beats_the_documents_default() {
    let workdir = TempDir::new().unwrap();
    let home = TempDir::new().unwrap();
    let flows = TempDir::new().unwrap();
    let app = workdir.path().join("app");
    write_app_repo(&app, "^1.0.0");
    let spec_path = workdir.path().join("workspace.json");
    fs::write(
        &spec_path,
        serde_json::json!({
            "name": "review-supplied-input",
            "sources": [{"id": "app", "path": app.to_string_lossy(), "ref": "main"}]
        })
        .to_string(),
    )
    .unwrap();
    let diff_path = workdir.path().join("d.diff");
    fs::write(&diff_path, "").unwrap();

    let out = darkmux_cmd()
        .env("DARKMUX_HOME", home.path())
        .env("DARKMUX_FLOWS_DIR", flows.path())
        .env("DARKMUX_LMS_BIN", "/usr/bin/true")
        .args([
            "mission",
            "launch",
            "review",
            "--param",
            &format!("workspace={}", spec_path.display()),
            "--param",
            &format!("diff_file={}", diff_path.display()),
            "--param",
            "mod_wait_seconds=45",
            "--dry-run",
        ])
        .output()
        .expect("mission launch review --dry-run runs");
    let stdout = String::from_utf8_lossy(&out.stdout).to_string();
    assert!(stdout.contains("mod_wait_seconds = 45"), "{stdout}");
}

/// (#2406) END-TO-END: a phase that ships some tasks and cascade-abandons
/// others reaches the OPERATOR's board as `degraded`, not silently folded
/// into `complete`.
///
/// The fail-probe's p2 is exactly the shape the finding names — `t-dep2`
/// and `t-chain-err` run and complete (`run_on: ["complete","error"]`),
/// while `t-dep` and `t-chain` cascade-abandon off the errored `t-fail`.
/// Two completed tasks, two abandoned ones, all terminal.
///
/// This covers the wiring no unit test can: launcher → `envelope.json` →
/// `mission_status::degraded_phase_ids`'s real disk read → the `--json`
/// payload. `Degraded` drives `lifecycle::phase_complete` on purpose, so
/// disk says `complete` for this phase and always will — the board can only
/// tell the difference by reading the envelope back.
#[test]
fn fail_probe_board_reports_the_mixed_phase_as_degraded_not_complete() {
    let (home, flows) = fail_probe_fixture("true");
    let _ = launch_fail_probe(&home, &flows);

    let dir = one_mission_dir(&home);
    let envelope: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(dir.join("envelope.json")).unwrap()).unwrap();
    let p2_outcome = envelope["phases"]
        .as_array()
        .expect("phases")
        .iter()
        .find(|p| p["phase_id"].as_str().is_some_and(|id| id.ends_with("-p2")))
        .expect("p2 in the envelope")
        .clone();
    assert_eq!(
        p2_outcome["outcome"],
        serde_json::json!("degraded"),
        "2 completed tasks + 2 cascade-abandoned ones is a MIX: {envelope}"
    );

    let out = darkmux_cmd()
        .env("DARKMUX_HOME", home.path())
        .env("DARKMUX_FLOWS_DIR", flows.path())
        .env("DARKMUX_LMS_BIN", "/usr/bin/true")
        .args(["mission", "status", "--json", "--all"])
        .output()
        .unwrap();
    let board: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    let phases = &board["missions"][0]["phases"];
    assert_eq!(
        phases["degraded"], 1,
        "the mixed phase must land in its OWN bucket, not inside `complete`: {phases}"
    );
    assert_eq!(
        phases["complete"], 1,
        "three phases: p1 abandoned, p2 degraded, p3 complete — so exactly ONE clean: {phases}"
    );
    assert_eq!(phases["abandoned"], 1, "{phases}");
    assert_eq!(phases["total"], 3, "{phases}");

    // …and the human board says the word, not just the JSON.
    let human = darkmux_cmd()
        .env("DARKMUX_HOME", home.path())
        .env("DARKMUX_FLOWS_DIR", flows.path())
        .env("DARKMUX_LMS_BIN", "/usr/bin/true")
        .args(["mission", "status", "--all"])
        .output()
        .unwrap();
    let text = String::from_utf8_lossy(&human.stdout);
    assert!(text.contains("degraded"), "the board never says `degraded`:\n{text}");
}

// ─── #1711: `mission status` is no longer fleet-blind ──────────────────
//
// The daemon (#1705) fixed this by rolling missions up from the merged
// flow record set — local day-files PLUS the shared Redis stream — rather
// than from durable per-machine JSON alone. `mission status` reuses that
// EXACT aggregation (`darkmux_serve::build_runs`, already `pub` and
// already called by `darkmux run list` — #1905) rather than a second
// implementation.
//
// These tests never touch Redis (no `DARKMUX_REDIS_URL` is set, so the
// fleet half resolves to `SourceState::Off`) — they simulate "a peer's
// mission" the same way `build_runs`'s own aggregation is source-agnostic:
// a flow record naming a `mission_id` this machine has no durable JSON
// for, stamped with a foreign `machine_id`. That is exactly the shape a
// record arriving over the shared Redis stream would have once it reaches
// `build_flow_mission_index` — the function does not care whether the
// `Vec<serde_json::Value>` it folds came from a local day-file or a fleet
// read. This is "test against fakes", never a real peer.
//
// `mission status` calls `darkmux_serve::peer_mission_runs` — the NARROW
// half of #1705's aggregation (#1711 review finding: the full
// `darkmux_serve::build_runs`, also used by `darkmux run list`, redundantly
// reloads `Mission`/`Phase` JSON and rebuilds a `Run` for every local
// mission this board never uses).

/// One flow day-file with a mission this machine never launched — a
/// `mission start`/`mission close` (or no terminal record at all) pair
/// stamped with `machine_id: "peer-host"`, and zero corresponding
/// `~/.darkmux/missions/<id>/mission.json` on this machine.
fn write_peer_mission_day_file(
    flows: &std::path::Path,
    mission_id: &str,
    machine_id: &str,
    terminal_action: Option<&str>,
) {
    fs::create_dir_all(flows).unwrap();
    let rec = |ts: &str, action: &str| {
        serde_json::json!({
            "ts": ts, "level": "info", "category": "work", "tier": "local",
            "stage": "dispatch", "action": action, "handle": "review",
            "session_id": format!("{mission_id}-s1"), "machine_id": machine_id,
            "mission_id": mission_id,
        })
        .to_string()
    };
    let mut lines = vec![rec("2026-09-10T01:00:00Z", "mission start")];
    if let Some(action) = terminal_action {
        lines.push(rec("2026-09-10T01:05:00Z", action));
    }
    fs::write(flows.join("2026-09-10.jsonl"), lines.join("\n") + "\n").unwrap();
}

#[test]
fn mission_status_json_surfaces_a_peer_mission_seen_only_via_the_flow_stream() {
    let home = TempDir::new().unwrap();
    let flows = TempDir::new().unwrap();
    write_peer_mission_day_file(flows.path(), "review-peer-1711", "peer-host", Some("mission close"));

    let out = darkmux_cmd()
        .env("DARKMUX_HOME", home.path())
        .env("DARKMUX_FLOWS_DIR", flows.path())
        .env("DARKMUX_LMS_BIN", "/usr/bin/true")
        .args(["mission", "status", "--json", "--all"])
        .output()
        .unwrap();
    assert!(out.status.success(), "stderr: {}", String::from_utf8_lossy(&out.stderr));
    let board: serde_json::Value = serde_json::from_slice(&out.stdout)
        .unwrap_or_else(|e| panic!("mission status --json must be JSON: {e}"));

    // Never in the local `missions` array — this machine has no durable
    // record for it, and #1711's whole point is that the OLD behavior
    // (silently absent everywhere) must not be replaced by a NEW bug
    // (silently merged into the local array as if it were owned here).
    assert_eq!(
        board["missions"].as_array().map(Vec::len),
        Some(0),
        "a peer mission must never land in the local `missions` array: {board}"
    );
    let peer = board["peer_missions"].as_array().expect("peer_missions must be an array");
    assert_eq!(peer.len(), 1, "the peer mission must surface exactly once: {board}");
    assert_eq!(peer[0]["id"], "review-peer-1711");
    assert_eq!(peer[0]["machine"], "peer-host");
    assert_eq!(peer[0]["tracked"], false, "an observed-not-owned row must say so on the wire");
    assert_eq!(peer[0]["status"], "complete", "a seen `mission close` must read as complete");
    assert_eq!(board["fleet"]["state"], "off", "no DARKMUX_REDIS_URL was set — this is a real Off");
    assert_eq!(board["summary"]["fleet_complete"], true, "`Off` is a correct, complete answer");
}

#[test]
fn mission_status_text_shows_a_rostered_but_silent_peer_distinctly_from_a_running_one() {
    let home = TempDir::new().unwrap();
    let flows = TempDir::new().unwrap();
    // No terminal record at all, and the one session is far in the past —
    // this is the "rostered but silent" case: seen once, gone quiet, never
    // torn down on purpose. Must NOT read as "abandoned" (a verdict this
    // board cannot support) or "running" (it is not live).
    write_peer_mission_day_file(flows.path(), "review-peer-silent", "peer-2", None);

    let out = darkmux_cmd()
        .env("DARKMUX_HOME", home.path())
        .env("DARKMUX_FLOWS_DIR", flows.path())
        .env("DARKMUX_LMS_BIN", "/usr/bin/true")
        .args(["mission", "status", "--all"])
        .output()
        .unwrap();
    assert!(out.status.success(), "stderr: {}", String::from_utf8_lossy(&out.stderr));
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(
        text.contains("OBSERVED ON THE FLEET"),
        "a peer mission must get its own labeled section:\n{text}"
    );
    assert!(text.contains("peer-2"), "the row must name the machine that ran it:\n{text}");
    assert!(
        text.contains("silent") && !text.contains("aborted"),
        "a mission with no terminal record must read as silent, never as an aborted verdict:\n{text}"
    );
    // The board must not hang and must not crash — this is the exact
    // "degraded case" the issue asks for: a peer that went quiet, not one
    // that answered cleanly.
    assert!(out.status.success());
}

#[test]
fn mission_status_degrades_legibly_when_the_fleet_stream_could_not_be_read() {
    // (#1711 hard requirement) An unreachable-fleet simulation: point
    // `DARKMUX_REDIS_URL` at a host that refuses the connection outright
    // (loopback, a port nothing listens on) — bounded by
    // `REDIS_CONNECT_TIMEOUT`, so this must return promptly, never hang.
    // This is a fake unreachable service, never a real peer or a real
    // operator credential.
    let home = TempDir::new().unwrap();
    let flows = TempDir::new().unwrap();

    let out = darkmux_cmd()
        .env("DARKMUX_HOME", home.path())
        .env("DARKMUX_FLOWS_DIR", flows.path())
        .env("DARKMUX_LMS_BIN", "/usr/bin/true")
        .env("DARKMUX_REDIS_URL", "redis://127.0.0.1:1/")
        .args(["mission", "status", "--json", "--all"])
        .timeout(std::time::Duration::from_secs(15))
        .output()
        .unwrap();
    assert!(out.status.success(), "stderr: {}", String::from_utf8_lossy(&out.stderr));
    let board: serde_json::Value = serde_json::from_slice(&out.stdout)
        .unwrap_or_else(|e| panic!("mission status --json must be JSON: {e}"));
    assert_eq!(
        board["fleet"]["state"], "unavailable",
        "an unreachable Redis with nothing cached must read as unavailable: {board}"
    );
    assert_eq!(board["summary"]["fleet_complete"], false);
    assert_eq!(board["missions"].as_array().map(Vec::len), Some(0));
    assert_eq!(
        board["peer_missions"].as_array().map(Vec::len),
        Some(0),
        "no cached snapshot exists, so peer_missions must be empty, never fabricated: {board}"
    );

    // …and the human board admits it, never a false "board is clean".
    let human = darkmux_cmd()
        .env("DARKMUX_HOME", home.path())
        .env("DARKMUX_FLOWS_DIR", flows.path())
        .env("DARKMUX_LMS_BIN", "/usr/bin/true")
        .env("DARKMUX_REDIS_URL", "redis://127.0.0.1:1/")
        .args(["mission", "status", "--all"])
        .timeout(std::time::Duration::from_secs(15))
        .output()
        .unwrap();
    let text = String::from_utf8_lossy(&human.stdout);
    assert!(
        !text.contains("✓ board is clean"),
        "an incomplete fleet read must never render the unqualified green checkmark:\n{text}"
    );
    assert!(
        text.to_lowercase().contains("fleet"),
        "the board must name that the fleet-wide read did not complete:\n{text}"
    );
}

#[test]
fn mission_status_is_byte_identical_on_a_standalone_machine_with_no_peers() {
    // (#1711 hard requirement) The local-only case is UNCHANGED: a real
    // mission, launched and finalized exactly as before #1711, with no
    // flow records for any other mission and no Redis configured, must
    // still print the exact pre-#1711 clean-board line — no new section,
    // no new caveat.
    let (home, flows) = forward_dep_fixture();
    let launched = darkmux_cmd()
        .env("DARKMUX_HOME", home.path())
        .env("DARKMUX_FLOWS_DIR", flows.path())
        .env("DARKMUX_LMS_BIN", "/usr/bin/true")
        .args(["mission", "launch", "forward-dep"])
        .output()
        .unwrap();
    assert_eq!(launched.status.code(), Some(0));

    let out = darkmux_cmd()
        .env("DARKMUX_HOME", home.path())
        .env("DARKMUX_FLOWS_DIR", flows.path())
        .env("DARKMUX_LMS_BIN", "/usr/bin/true")
        .args(["mission", "status", "--all"])
        .output()
        .unwrap();
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(
        !text.contains("OBSERVED ON THE FLEET"),
        "a standalone board with no peers must show no fleet section:\n{text}"
    );
    assert!(
        !text.to_lowercase().contains("fleet:"),
        "a standalone board must show no fleet-scope warning:\n{text}"
    );
    assert!(
        text.trim_end().ends_with("✓ board is clean — every mission's phases are reconciled"),
        "the exact pre-#1711 clean-board line must be unchanged:\n{text}"
    );

    let json_out = darkmux_cmd()
        .env("DARKMUX_HOME", home.path())
        .env("DARKMUX_FLOWS_DIR", flows.path())
        .env("DARKMUX_LMS_BIN", "/usr/bin/true")
        .args(["mission", "status", "--json", "--all"])
        .output()
        .unwrap();
    let board: serde_json::Value = serde_json::from_slice(&json_out.stdout).unwrap();
    assert_eq!(board["peer_missions"].as_array().map(Vec::len), Some(0));
    assert_eq!(board["fleet"]["state"], "off");
    assert_eq!(board["summary"]["fleet_complete"], true);
}

/// (#1711 review finding, PROVEN against real operator data) A mission with
/// no durable `Mission` JSON here, but whose flow records carry THIS
/// machine's OWN resolved id, is an ORPHAN — not a peer. Mislabeling it
/// "OBSERVED ON THE FLEET ... not owned by this machine" is actively wrong
/// when it IS this machine (a deleted mission dir, a malformed
/// `mission.json` the loader silently skips, or a subsystem that stamped
/// `mission_id` without minting under `missions_dir()`). This pins the
/// fix end to end through the real binary, not just the unit-level
/// aggregation.
#[test]
fn mission_status_never_labels_a_same_machine_orphan_as_observed_on_the_fleet() {
    let home = TempDir::new().unwrap();
    let flows = TempDir::new().unwrap();
    // Same shape `write_peer_mission_day_file` uses, but `machine_id`
    // matches the child's OWN `DARKMUX_MACHINE_ID` below — that is the
    // whole point of this test.
    write_peer_mission_day_file(flows.path(), "orphan-local-1711", "this-machine", Some("mission close"));

    let out = darkmux_cmd()
        .env("DARKMUX_HOME", home.path())
        .env("DARKMUX_FLOWS_DIR", flows.path())
        .env("DARKMUX_LMS_BIN", "/usr/bin/true")
        .env("DARKMUX_MACHINE_ID", "this-machine")
        .args(["mission", "status", "--json", "--all"])
        .output()
        .unwrap();
    assert!(out.status.success(), "stderr: {}", String::from_utf8_lossy(&out.stderr));
    let board: serde_json::Value = serde_json::from_slice(&out.stdout)
        .unwrap_or_else(|e| panic!("mission status --json must be JSON: {e}"));
    assert_eq!(
        board["peer_missions"].as_array().map(Vec::len),
        Some(0),
        "a same-machine orphan must never surface as a peer row: {board}"
    );
    assert_eq!(
        board["missions"].as_array().map(Vec::len),
        Some(0),
        "an orphan has no durable JSON, so it cannot be a local `missions` row either: {board}"
    );

    let human = darkmux_cmd()
        .env("DARKMUX_HOME", home.path())
        .env("DARKMUX_FLOWS_DIR", flows.path())
        .env("DARKMUX_LMS_BIN", "/usr/bin/true")
        .env("DARKMUX_MACHINE_ID", "this-machine")
        .args(["mission", "status", "--all"])
        .output()
        .unwrap();
    let text = String::from_utf8_lossy(&human.stdout);
    assert!(
        !text.contains("OBSERVED ON THE FLEET"),
        "a same-machine orphan must not spawn a fleet section at all:\n{text}"
    );
}
