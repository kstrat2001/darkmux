//! Whether informational stderr lines print (5.0).
//!
//! An operator who runs `darkmux radio "..."` in a terminal wants the answer,
//! not the liveness markers and sink banners that precede it. Those lines are
//! debugging data for logs, so they print when stderr is NOT an interactive
//! terminal (CI logs, pipes, files, the daemon log, ACP stdio subprocesses),
//! when `--verbose` is passed, or when `DARKMUX_VERBOSE` is truthy, or it is
//! not set to a falsy value and `config.json`'s `runtime.verbose` is `true`
//! (env over config, as for every setting).
//!
//! What goes through [`verbose`]: purely informational lines (liveness echo,
//! sink-enabled banners, dispatch progress headers). What never does: warnings,
//! errors, refusals, and anything the operator must act on. The liveness
//! heartbeat FILE is written regardless, so debugging data is never lost.
//!
//! Dependency-free like `dispatch_liveness` (which calls it before config,
//! Redis or flow exist): the config tier is a raw peek, not `config_access`.

use std::io::IsTerminal;
use std::sync::atomic::{AtomicBool, Ordering};

static VERBOSE_FLAG: AtomicBool = AtomicBool::new(false);

/// Record the global `--verbose` flag for this process.
pub fn set_verbose_flag(on: bool) {
    VERBOSE_FLAG.store(on, Ordering::Relaxed);
}

/// Whether `--verbose` was passed to this process.
pub fn verbose_flag() -> bool {
    VERBOSE_FLAG.load(Ordering::Relaxed)
}

/// The pure decision. Verbose when stderr is not a terminal or the flag is
/// passed; otherwise `env > config.json > off`, so `DARKMUX_VERBOSE=0` turns
/// off a `runtime.verbose: true` for one shell. An explicit "off" never
/// suppresses a non-terminal stderr: log files keep their diagnostics.
pub fn decide(stderr_is_terminal: bool, flag: bool, env: Option<&str>, config: Option<bool>) -> bool {
    // One boolean vocabulary (`1`/`true`/`yes`/`on`, `0`/`false`/`no`/`off`,
    // any case); anything else says nothing. A pure parse: it reads no config.
    let env = env.and_then(crate::config_access::parse_bool_token);
    !stderr_is_terminal || flag || env.or(config).unwrap_or(false)
}

/// Raw peek at `<darkmux-home>/config.json`'s `runtime.verbose`; `None` on any
/// failure. Never touches `config_access` (see the module docs).
pub(crate) fn raw_config_verbose() -> Option<bool> {
    let text = std::fs::read_to_string(crate::paths::user_root_guarded().join("config.json")).ok()?;
    let value: serde_json::Value = serde_json::from_str(&text).ok()?;
    value.get("runtime")?.get("verbose")?.as_bool()
}

/// Whether informational diagnostics print right now.
pub fn verbose() -> bool {
    let is_tty = std::io::stderr().is_terminal();
    if !is_tty || verbose_flag() {
        return true;
    }
    #[cfg(any(test, feature = "test-support"))]
    crate::env_audit::audit_env_read("DARKMUX_VERBOSE");
    let env = std::env::var("DARKMUX_VERBOSE").ok();
    // The config file is read only on the interactive path, where it can
    // change the answer.
    decide(is_tty, false, env.as_deref(), raw_config_verbose())
}

/// `eprintln!` that prints only when [`verbose`] says so. For informational
/// lines only; warnings and errors use plain `eprintln!`.
#[macro_export]
macro_rules! diag_eprintln {
    ($($arg:tt)*) => {
        if $crate::diagnostics::verbose() {
            eprintln!($($arg)*);
        }
    };
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn non_terminal_stderr_is_always_verbose() {
        assert!(decide(false, false, None, None));
        assert!(decide(false, false, Some("0"), Some(false)));
    }

    #[test]
    fn interactive_terminal_is_quiet_by_default() {
        assert!(!decide(true, false, None, None));
        assert!(!decide(true, false, Some("0"), Some(false)));
        assert!(!decide(true, false, Some(""), None));
        assert!(!decide(true, false, Some("nonsense"), None));
    }

    /// `env > config.json`: a falsy `DARKMUX_VERBOSE` turns off a
    /// `runtime.verbose: true` for one shell; a value that is neither
    /// truthy nor falsy says nothing, so config decides.
    #[test]
    fn a_falsy_env_turns_off_config_verbose_on_a_terminal() {
        for v in ["0", "false", "NO", " off "] {
            assert!(!decide(true, false, Some(v), Some(true)), "env {v:?} beats config");
        }
        assert!(decide(true, false, Some("nonsense"), Some(true)), "an unrecognized env value defers to config");
        assert!(decide(true, false, Some(""), Some(true)), "an empty env value defers to config");
        assert!(decide(true, true, Some("0"), Some(true)), "the --verbose flag still wins");
        assert!(decide(false, false, Some("0"), Some(true)), "a non-terminal stays verbose");
    }

    #[test]
    fn each_explicit_tier_turns_a_terminal_verbose() {
        assert!(decide(true, true, None, None), "flag");
        for v in ["1", "true", "TRUE", " yes ", "on"] {
            assert!(decide(true, false, Some(v), None), "env {v:?}");
        }
        assert!(decide(true, false, None, Some(true)), "config");
    }

    #[test]
    #[serial_test::serial]
    fn flag_round_trips() {
        set_verbose_flag(true);
        assert!(verbose_flag());
        set_verbose_flag(false);
        assert!(!verbose_flag());
    }
}
