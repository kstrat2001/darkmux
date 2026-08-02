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

/// Usable terminal width in columns, or `None` when stdout isn't a terminal.
///
/// `None` is meaningful, not a failure: when output is piped or redirected
/// there is no width to adapt to, and a renderer should emit its stable full
/// form. That keeps `darkmux ... | grep` byte-predictable regardless of the
/// window the operator happened to run it in — a narrow terminal must never
/// silently truncate what a script reads.
///
/// Resolution order: `COLUMNS` → the TTY check → `ioctl(TIOCGWINSZ)` → `None`.
///
/// `COLUMNS` is consulted BEFORE the TTY check, and deliberately: a caller who
/// exports it has stated a width on purpose, and honoring that even when output
/// is piped is what makes the adaptive rendering observable and scriptable
/// (`COLUMNS=80 darkmux mission status | cat` renders as an 80-column terminal
/// would). Its mere absence says nothing either way, since most shells keep it
/// as a shell variable and never export it — which is why it can't be the only
/// source, and why `ioctl` is still what answers for a real terminal.
///
/// That env tier doubles as the testing/override seam, which is why there is no
/// separate `set_width_override` global: renderers here take the width as a
/// parameter, so they are already testable at any width without process state.
#[must_use]
pub fn terminal_width() -> Option<usize> {
    if let Some(c) = std::env::var("COLUMNS").ok().and_then(|v| v.parse::<usize>().ok()).filter(|c| *c > 0) {
        return Some(c);
    }
    if !std::io::stdout().is_terminal() {
        return None;
    }
    terminal_width_ioctl()
}

/// `TIOCGWINSZ` query, gated to unix to match this crate's convention for
/// platform calls (`flock` in `lib.rs`, `libc::kill` in `residency_lease.rs`).
/// A non-unix build resolves width from `COLUMNS` alone and otherwise reports
/// `None`, which every caller already handles as "don't adapt".
#[cfg(unix)]
fn terminal_width_ioctl() -> Option<usize> {
    // SAFETY: `winsize` is four `u16`s — plain POD, for which an all-zero bit
    // pattern is a valid value. We pass a pointer to a live local and read the
    // struct only when `ioctl` reports success.
    let cols = unsafe {
        let mut ws: libc::winsize = std::mem::zeroed();
        if libc::ioctl(libc::STDOUT_FILENO, libc::TIOCGWINSZ, &mut ws) == 0 {
            ws.ws_col as usize
        } else {
            0
        }
    };
    // A successful ioctl can still report 0 (e.g. a pty with no size set).
    (cols > 0).then_some(cols)
}

#[cfg(not(unix))]
fn terminal_width_ioctl() -> Option<usize> {
    None
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

/// Render `text` as an OSC 8 terminal hyperlink to `url` (#1569 packet A).
///
/// Gated on the same [`colorize_enabled`] check the color helpers use, for
/// the same reason: when output is piped, redirected, or headed for `--json`,
/// it must stay byte-clean. A terminal that doesn't understand OSC 8 ignores
/// the escape and shows `text` — but a `grep` or a JSON parser does NOT, so
/// "harmless to terminals" is not the same as "safe to always emit".
///
/// **`url` is not escaped or validated here.** OSC 8 terminates its URL on
/// `ST` (`ESC \`) or `BEL`, so a control character in `url` would break out
/// of the escape and corrupt the line. Every caller today builds its URL from
/// a resolved daemon base plus a percent-encoded id, never from raw operator
/// text; a future caller taking arbitrary input must encode before calling.
/// Control bytes are stripped defensively below rather than trusted, because
/// the failure is silent line corruption rather than a visible error.
///
/// Terminal support: iTerm2, WezTerm, Kitty, VS Code's terminal, GNOME
/// Terminal. Terminal.app has none and degrades to plain `text`.
#[must_use]
pub fn link(url: &str, text: &str) -> String {
    if !colorize_enabled() {
        return text.to_string();
    }
    // Strip C0 controls + DEL. `ESC` and `BEL` would terminate the escape
    // early; the rest can't appear in a valid URL and have no business in
    // one. Cheap, and it makes the corruption unreachable rather than
    // merely unlikely.
    let safe: String = url.chars().filter(|c| !c.is_control()).collect();
    format!("\x1b]8;;{safe}\x1b\\{text}\x1b]8;;\x1b\\")
}

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

        // (#1569 packet A) A hyperlink is styling, not content — piped or
        // redirected output must stay byte-clean. "Terminals that don't
        // understand OSC 8 ignore it" is NOT sufficient: `grep`, `jq`, and a
        // golden-file diff all see the bytes.
        assert_eq!(link("http://127.0.0.1:8765/", "m1"), "m1");
        assert!(!link("http://x/", "m1").contains('\x1b'));

        set_colorize_override(None); // restore
    }

    /// (#1569 packet A) The exact OSC 8 byte sequence, frozen. Contract-6
    /// shape: "frozen means one hash, not one intention" — a renderer whose
    /// output a viewer will parse cannot drift silently.
    ///
    /// `ESC ] 8 ; ; <url> ESC \  <text>  ESC ] 8 ; ; ESC \`
    /// The empty `;;` is the (unused) params slot; `ESC \` is ST.
    #[serial_test::serial]
    #[test]
    fn link_emits_the_exact_osc8_sequence() {
        set_colorize_override(Some(true));

        assert_eq!(
            link("http://127.0.0.1:8765/mission/m1/graph", "◆ m1"),
            "\x1b]8;;http://127.0.0.1:8765/mission/m1/graph\x1b\\◆ m1\x1b]8;;\x1b\\"
        );

        set_colorize_override(None);
    }

    /// A control byte in the URL would terminate the escape early and corrupt
    /// the rest of the line — silently, since the terminal would render the
    /// tail as text. Stripped rather than trusted: callers build URLs from a
    /// resolved base today, but "no caller does that yet" is not a guarantee.
    #[serial_test::serial]
    #[test]
    fn link_strips_control_bytes_that_would_break_out_of_the_escape() {
        set_colorize_override(Some(true));

        let out = link("http://x/\x1b\\evil\x07more\n", "t");
        // Exactly two STs: the URL terminator and the closing sequence. An
        // injected ESC/BEL would add more and split the escape.
        assert_eq!(out.matches("\x1b\\").count(), 2, "{out:?}");
        assert!(!out.contains('\x07'), "BEL also terminates OSC 8: {out:?}");
        assert!(!out.contains('\n'), "{out:?}");
        // The literal `\` survives, and should: it is not a control byte and
        // cannot terminate the escape on its own. This helper prevents escape
        // BREAKOUT; it does not validate URLs — see its doc comment.
        assert!(out.contains("http://x/\\evilmore"), "{out:?}");

        set_colorize_override(None);
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
