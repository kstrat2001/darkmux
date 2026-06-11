//! darkmux CLI styling — ANSI escape helpers for terminal output.
//!
//! Provides semantic color functions (success, warn, error, etc.) that wrap
//! strings in ANSI escape codes when coloring is enabled. Callers can force-
//! disable coloring via `set_colorize_override` (e.g. for `--json` output).

use std::io::IsTerminal;
use std::sync::atomic::{AtomicU8, Ordering};

/// Process-global override for colorize behavior.
/// 0 = auto-detect (TTY + NO_COLOR), 1 = force on, 2 = force off.
const OVERRIDE_AUTO: u8 = 0;
const OVERRIDE_ON: u8 = 1;
const OVERRIDE_OFF: u8 = 2;

static COLORIZE_OVERRIDE: AtomicU8 = AtomicU8::new(OVERRIDE_AUTO);

/// Whether colorize is currently enabled.
///
/// Returns `true` only when stdout is a TTY **and** the `NO_COLOR`
/// environment variable is unset.  When disabled, styling helpers return
/// the input string unchanged (no escape codes).
pub fn colorize_enabled() -> bool {
    match COLORIZE_OVERRIDE.load(Ordering::SeqCst) {
        OVERRIDE_ON => true,   // forced on
        OVERRIDE_OFF => false,  // forced off
        _ => std::io::stdout().is_terminal() && std::env::var("NO_COLOR").is_err(),
    }
}

/// Force-disable or force-enable coloring regardless of TTY / NO_COLOR.
///
/// Pass `Some(true)` to force color on (e.g. piping to a pager that supports
/// it).  Pass `Some(false)` to force color off (e.g. `--json` / machine-readable
/// output).  Pass `None` to return to auto-detect mode.
pub fn set_colorize_override(val: Option<bool>) {
    COLORIZE_OVERRIDE.store(match val {
        Some(true) => OVERRIDE_ON,
        Some(false) => OVERRIDE_OFF,
        None => OVERRIDE_AUTO,
    }, Ordering::SeqCst);
}

/// Wrap `s` in green ANSI escape (success indicator).
#[must_use]
pub fn success(s: &str) -> String { colorize("32", s) }

/// Wrap `s` in yellow ANSI escape (warning indicator).
#[must_use]
pub fn warn(s: &str) -> String { colorize("33", s) }

/// Wrap `s` in red ANSI escape (error indicator).
#[must_use]
pub fn error(s: &str) -> String { colorize("31", s) }

/// Wrap `s` in dim ANSI escape (secondary text).
#[must_use]
pub fn dim(s: &str) -> String { colorize("2", s) }

/// Wrap `s` in cyan ANSI escape (accent / label).
#[must_use]
pub fn accent(s: &str) -> String { colorize("36", s) }

/// Wrap `s` in bold + cyan ANSI escape (header / title).
#[must_use]
pub fn header(s: &str) -> String { colorize("1;36", s) }

/// Wrap `s` in bold ANSI escape (emphasis).
#[must_use]
pub fn bold(s: &str) -> String { colorize("1", s) }

/// Internal helper: wrap `s` in the given ANSI code when coloring is enabled.
fn colorize(code: &str, s: &str) -> String {
    if colorize_enabled() {
        format!("\x1b[{}m{}\x1b[0m", code, s)
    } else {
        s.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// When override is OFF, every helper returns the input unchanged —
    /// no ANSI escape sequences at all.
    #[serial_test::serial]
    #[test]
    fn override_off_returns_plain() {
        set_colorize_override(Some(false));

        assert!(!success("ok").contains("\x1b["));
        assert!(!warn("ok").contains("\x1b["));
        assert!(!error("ok").contains("\x1b["));
        assert!(!dim("ok").contains("\x1b["));
        assert!(!accent("ok").contains("\x1b["));
        assert!(!header("ok").contains("\x1b["));
        assert!(!bold("ok").contains("\x1b["));

        // Also verify the string is returned unchanged.
        assert_eq!(success("ok"), "ok");
        assert_eq!(warn("ok"), "ok");

        set_colorize_override(None); // restore
    }

    /// When override is ON, helpers DO wrap with the expected ANSI codes.
    #[serial_test::serial]
    #[test]
    fn override_on_returns_ansi() {
        set_colorize_override(Some(true));

        assert!(success("ok").contains("\x1b[32m"));
        assert!(warn("ok").contains("\x1b[33m"));
        assert!(error("ok").contains("\x1b[31m"));
        assert!(dim("ok").contains("\x1b[2m"));
        assert!(accent("ok").contains("\x1b[36m"));
        assert!(header("ok").contains("\x1b[1;36m"));
        assert!(bold("ok").contains("\x1b[1m"));

        // Every helper should end with the reset code.
        assert!(success("ok").ends_with("\x1b[0m"));
        assert!(warn("ok").ends_with("\x1b[0m"));
        assert!(error("ok").ends_with("\x1b[0m"));

        set_colorize_override(None); // restore
    }

    /// NO_COLOR env var disables coloring even when stdout is a TTY.
    #[serial_test::serial]
    #[test]
    fn no_color_disables_auto() {
        // Save original NO_COLOR value so we can restore it.
        let had_no_color = std::env::var("NO_COLOR").is_ok();

        // Set NO_COLOR to a non-empty value.
        unsafe { std::env::set_var("NO_COLOR", "1"); }

        // Even if stdout were a TTY, colorize_enabled should be false.
        assert!(!colorize_enabled());

        // Clear NO_COLOR and re-check (auto-detect should work again).
        unsafe { std::env::remove_var("NO_COLOR"); }

        // When stdout is NOT a TTY (as in `cargo test`), this will be false.
        // When stdout IS a TTY, it would be true — but we can still assert
        // that removing NO_COLOR doesn't force color on by itself.
        // Verify that with NO_COLOR unset, colorize returns plain text when
        // stdout is not a TTY (the normal `cargo test` scenario).
        assert!(!success("x").contains("\x1b["));

        // Restore original NO_COLOR state.
        if had_no_color {
            unsafe { std::env::set_var("NO_COLOR", "1"); }
        } else {
            unsafe { std::env::remove_var("NO_COLOR"); }
        }

        // Restore override.
        set_colorize_override(None);
    }
}
