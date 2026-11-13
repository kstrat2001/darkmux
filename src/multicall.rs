//! (#3112) The darkmux binary as a multi-call entry: hidden helper
//! subcommands, routed before anything else in `main` runs.
//!
//! A sandbox runs this executable as a helper (`darkmux runtime …` is the
//! agent runtime a Seatbelt sandbox runs), so a user never builds, downloads
//! or installs a separate program. [`hand_over`] is the first thing `main`
//! does: before clap, the provider registry or any setting is read. A
//! helper's stdout, stderr and exit code are a contract its caller parses,
//! and inside a sandbox `HOME` is the sandbox's own `tmp`, so a warning about
//! `~/.darkmux/config.json` would both be wrong and break that contract. For
//! the same reason a helper is not a clap subcommand, which also keeps it out
//! of `--help`. Only the exact form `darkmux <helper> <args>` (the helper's
//! name as the first argument) routes; any other command line is an ordinary
//! darkmux one.
//!
//! ## Adding a helper
//!
//! One row in [`HELPERS`]: the first argument that selects it, and its entry.
//! The entry reads its own arguments (everything after the first two, in the
//! form it needs: `std::env::args` or `args_os`) and returns the process's
//! exit code, which `main` returns unchanged. Nothing else changes.

use std::ffi::OsString;
use std::process::{ExitCode, Termination};

/// A helper's entry: it reads its own arguments and returns the exit code.
type Entry = fn() -> ExitCode;

/// Every hidden helper: the first argument that selects it, and its entry.
/// Add a helper as one more row.
const HELPERS: &[(&str, Entry)] = &[
    // The agent runtime (`runtime/src/lib.rs`), what a Seatbelt sandbox runs.
    ("runtime", runtime),
    // A Landlock sandbox's helper (`crates/darkmux-sandbox/src/landlock.rs`):
    // it confines itself, then execs what the sandbox runs.
    (darkmux_sandbox::landlock::HELPER_ARG, landlock),
];

/// `darkmux runtime …`: the runtime's own entry, given its arguments the way
/// the standalone `darkmux-runtime` reads its own (`std::env::args`, so the
/// same refusal of a non-UTF-8 argument).
fn runtime() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(2).collect();
    darkmux_runtime::main_with(&args)
}

/// `darkmux __darkmux-landlock <dir> <program> [args]`: the Landlock helper,
/// given its own arguments; it returns only when it cannot start.
fn landlock() -> ExitCode {
    let args: Vec<OsString> = std::env::args_os().skip(2).collect();
    darkmux_sandbox::landlock::helper(&args)
}

/// The helper `argv` (the program name first) selects, if any: its row.
fn helper(argv: &[OsString]) -> Option<&'static (&'static str, Entry)> {
    let first = argv.get(1)?;
    HELPERS.iter().find(|(name, _)| first == name)
}

/// Run the helper this process was started as, and return its exit code;
/// `None` for an ordinary darkmux command line.
pub(crate) fn hand_over() -> Option<ExitCode> {
    let argv: Vec<OsString> = std::env::args_os().collect();
    helper(&argv).map(|(_, entry)| entry())
}

/// How the process ends: a helper's own exit code, returned from `main`
/// exactly as the helper returns it, or a darkmux command's result, reported
/// by std's own `Result` reporting (`Error: …`, exit 1), unchanged.
pub(crate) enum Exit {
    Helper(ExitCode),
    Darkmux(anyhow::Result<()>),
}

impl Termination for Exit {
    fn report(self) -> ExitCode {
        match self {
            Exit::Helper(code) => code,
            Exit::Darkmux(result) => result.report(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(args: &[&str]) -> Vec<OsString> {
        args.iter().map(OsString::from).collect()
    }

    fn routes_to_runtime(args: &[&str]) -> bool {
        helper(&argv(args)).is_some_and(|(name, _)| *name == "runtime")
    }

    #[test]
    fn only_a_helpers_name_as_the_first_argument_routes_to_it() {
        assert!(routes_to_runtime(&["darkmux", "runtime"]));
        assert!(routes_to_runtime(&["/opt/homebrew/bin/darkmux", "runtime", "run", "--model", "m"]));
        let landlock = helper(&argv(&["darkmux", "__darkmux-landlock", "/d", "sh"]));
        assert!(landlock.is_some_and(|(name, _)| *name == darkmux_sandbox::landlock::HELPER_ARG));
        for ordinary in [
            &["darkmux"][..],
            &["darkmux", "run", "list"],
            &["darkmux", "--verbose", "runtime"],
            &["darkmux", "runtimes"],
            &["darkmux", "dispatch", "runtime"],
        ] {
            assert!(helper(&argv(ordinary)).is_none(), "{ordinary:?} is an ordinary command line");
        }
    }

    /// Every helper is selected by its own row: names are unique, and none
    /// shadows a real darkmux subcommand a user types.
    #[test]
    fn helper_names_are_unique_and_none_is_a_darkmux_command() {
        use clap::CommandFactory;
        let names: Vec<&str> = HELPERS.iter().map(|(n, _)| *n).collect();
        let mut unique = names.clone();
        unique.sort();
        unique.dedup();
        assert_eq!(unique.len(), names.len(), "{names:?}");
        let cli = crate::cli::Cli::command();
        for name in names {
            assert!(cli.find_subcommand(name).is_none(), "`{name}` would hide the darkmux command of that name");
        }
    }
}
