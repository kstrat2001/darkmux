//! Process helpers that are not about any one engine (#316 stage 3.6).

use std::process::Command;

/// Pin a model-host (or model-host-adjacent) child's working directory to
/// `/`, so its spawn survives a daemon or dispatch process whose cwd was a
/// git worktree that has since been removed (#1863).
///
/// **Named per #2534.** #1863 pinned every such spawn's cwd, but only the
/// `run_bounded` chokepoint got a test — three spawns that bypass it (a
/// bespoke stdio-inheriting load loop, a dispatch-path `lms ps` probe, and
/// a lab-run hardware fingerprint) each carried their OWN copy of
/// `cmd.current_dir("/")` with no test protecting it: deleting any one of
/// them left the whole crate suite green. Giving the pin a name a reader
/// can follow — and a conformance test that scans for every site that
/// spawns `lms` (or an equally cwd-fragile probe) and asserts it calls
/// this helper — is what makes a NEW unpinned spawn loud instead of silent.
/// See `pin_cwd_conformance` (this crate's `tests/`) for the enumeration
/// and `crate::gestalt_host::lms_host::run_bounded` for the chokepoint this
/// helper is factored out of.
///
/// `/` needs no resolution (no `dirs::home_dir()` call that could return
/// `None`, no darkmux-root lookup that could itself be cwd-sensitive) and
/// is guaranteed to exist for the whole life of the process on every POSIX
/// target this project ships (Windows is not a build target).
pub fn pin_cwd(cmd: &mut Command) {
    cmd.current_dir("/");
}
