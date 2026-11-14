//! The runtime's subcommands other than `run`, as the process shows them:
//! what each prints, on which stream, and its exit code. `main_with` is the
//! one entry both builds run (the standalone binary and darkmux's hidden
//! `runtime` subcommand), so these spawn the real `darkmux-runtime` binary.
//! `--check` reads its workspace from `DARKMUX_RUNTIME_WORKSPACE`, pointed
//! here at a temporary folder.

use std::path::Path;
use std::process::{Command, Output};

const VERSION: &str = env!("CARGO_PKG_VERSION");

fn runtime(args: &[&str], workspace: Option<&Path>) -> Output {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_darkmux-runtime"));
    cmd.args(args);
    match workspace {
        Some(dir) => cmd.env("DARKMUX_RUNTIME_WORKSPACE", dir),
        None => cmd.env_remove("DARKMUX_RUNTIME_WORKSPACE"),
    };
    cmd.output().expect("spawning darkmux-runtime")
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

/// `--version` and `version` print the version alone and exit 0.
#[test]
fn version_prints_the_version_and_exits_zero() {
    for arg in ["--version", "version"] {
        let out = runtime(&[arg], None);
        assert_eq!(out.status.code(), Some(0), "{arg}");
        assert_eq!(text(&out.stdout), format!("darkmux-runtime {VERSION}\n"), "{arg}");
        assert_eq!(text(&out.stderr), "", "{arg}");
    }
}

/// With no subcommand the runtime prints its version and usage on stdout
/// and exits 0: every subcommand and `run`'s flags, then what `--json` emits.
#[test]
fn no_subcommand_prints_the_usage_and_exits_zero() {
    let out = runtime(&[], None);
    assert_eq!(out.status.code(), Some(0));
    let stdout = text(&out.stdout);
    assert!(
        stdout.starts_with(&format!(
            "darkmux-runtime {VERSION}\n\nUsage:\n  darkmux-runtime --check\n  darkmux-runtime --version\n  \
             darkmux-runtime run --model <id> --system <text> --prompt <text>\n"
        )),
        "{stdout}"
    );
    for flag in ["--compactor-auth-header-stdin", "--compact-strategy narrative|structured-slot", "--bail-after-compactions N"] {
        assert!(stdout.contains(flag), "usage lacks `{flag}`: {stdout}");
    }
    assert!(
        stdout.ends_with("\nFlags:\n  --json       Emit structured envelope on stdout (status to stderr).\n               Schema: { result, final_assistant, trajectory_path }\n"),
        "{stdout}"
    );
    assert_eq!(text(&out.stderr), "");
}

/// An unknown subcommand is refused on stderr, naming the known ones, with
/// exit 2 and nothing on stdout.
#[test]
fn an_unknown_subcommand_is_refused_naming_the_known_ones() {
    let out = runtime(&["bogus"], None);
    assert_eq!(out.status.code(), Some(2));
    assert_eq!(text(&out.stderr), "unknown subcommand: bogus\nknown: --check, --version, run\n");
    assert_eq!(text(&out.stdout), "");
}

/// `--check` (or `check`) passes on a writable workspace, leaving no probe
/// behind, and fails with exit 1 on one that does not exist.
#[test]
fn check_passes_on_a_writable_workspace_and_fails_on_a_missing_one() {
    let tmp = tempfile::tempdir().unwrap();
    for arg in ["--check", "check"] {
        let out = runtime(&[arg], Some(tmp.path()));
        let stdout = text(&out.stdout);
        assert_eq!(out.status.code(), Some(0), "{arg}: {stdout}");
        assert!(stdout.starts_with(&format!("darkmux-runtime {VERSION} — container environment check\n")), "{arg}: {stdout}");
        assert!(stdout.contains(&format!("[ok]  {} exists and is writable", tmp.path().display())), "{arg}: {stdout}");
        assert!(stdout.ends_with("phase 1 sanity: PASS\n"), "{arg}: {stdout}");
        assert_eq!(std::fs::read_dir(tmp.path()).unwrap().count(), 0, "{arg}: the write probe was left behind");
    }
    let missing = tmp.path().join("not-mounted");
    let out = runtime(&["--check"], Some(&missing));
    let stdout = text(&out.stdout);
    assert_eq!(out.status.code(), Some(1), "{stdout}");
    assert!(stdout.contains(&format!("[!!]  {} does NOT exist (was the volume mounted?)", missing.display())), "{stdout}");
    assert!(stdout.ends_with("phase 1 sanity: one or more checks failed\n"), "{stdout}");
}
