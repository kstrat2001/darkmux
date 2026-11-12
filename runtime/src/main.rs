//! The `darkmux-runtime` binary: the Docker image's entrypoint. The runtime
//! itself is the library (`src/lib.rs`); darkmux runs the same entry as its
//! hidden `runtime` subcommand (#3112).

use std::process::ExitCode;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    darkmux_runtime::main_with(&args)
}
