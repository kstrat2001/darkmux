//! (#3112) `darkmux runtime …`: the agent runtime, inside this binary.
//!
//! A Seatbelt sandbox runs this executable with `runtime` as its first
//! argument, so a user never builds, downloads or installs a separate
//! runtime. Everything after `runtime` goes to the runtime's own entry
//! (`darkmux_runtime::main_with`), the one the Docker image's
//! `darkmux-runtime` binary calls, and the process exits with the code it
//! returns.
//!
//! The hand-over is the first thing `main` does, before clap, the provider
//! registry or any setting is read: the runtime's stdout, stderr and exit
//! code are a contract its host parses, and inside a sandbox `HOME` is the
//! sandbox's own `tmp`, so a warning about `~/.darkmux/config.json` would
//! both be wrong and break that contract. For the same reason it is not a
//! clap subcommand, which also keeps it out of `--help`: only the exact form
//! `darkmux runtime <args>` is the runtime, and any other argument order is
//! an ordinary darkmux command line.

use std::ffi::OsString;
use std::process::{ExitCode, Termination};

/// The first argument that hands this process to the runtime.
pub(crate) const SUBCOMMAND: &str = "runtime";

/// Whether `argv` (the program name first) starts the runtime.
pub(crate) fn is_runtime(argv: &[OsString]) -> bool {
    argv.get(1).is_some_and(|a| a == SUBCOMMAND)
}

/// Run the runtime, when this process was started as `darkmux runtime …`,
/// and return its exit code; otherwise `None`. Its arguments are read the
/// way the standalone binary reads its own (`std::env::args`, so the same
/// refusal of a non-UTF-8 argument).
pub(crate) fn run_if_invoked() -> Option<ExitCode> {
    let argv: Vec<OsString> = std::env::args_os().collect();
    if !is_runtime(&argv) {
        return None;
    }
    let args: Vec<String> = std::env::args().skip(2).collect();
    Some(darkmux_runtime::main_with(&args))
}

/// How the process ends: the runtime's own exit code, returned from `main`
/// exactly as the standalone binary returns it, or a darkmux command's
/// result, reported by std's own `Result` reporting (`Error: …`, exit 1),
/// unchanged.
pub(crate) enum Exit {
    Runtime(ExitCode),
    Darkmux(anyhow::Result<()>),
}

impl Termination for Exit {
    fn report(self) -> ExitCode {
        match self {
            Exit::Runtime(code) => code,
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

    #[test]
    fn only_runtime_as_the_first_argument_starts_the_runtime() {
        assert!(is_runtime(&argv(&["darkmux", "runtime"])));
        assert!(is_runtime(&argv(&["/opt/homebrew/bin/darkmux", "runtime", "run", "--model", "m"])));
        assert!(!is_runtime(&argv(&["darkmux"])));
        assert!(!is_runtime(&argv(&["darkmux", "run", "list"])));
        assert!(!is_runtime(&argv(&["darkmux", "--verbose", "runtime"])));
        assert!(!is_runtime(&argv(&["darkmux", "runtimes"])));
        assert!(!is_runtime(&argv(&["darkmux", "dispatch", "runtime"])));
    }
}
