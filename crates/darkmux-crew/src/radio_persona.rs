//! The radio answering seat's persona and token budget.
//!
//! One place builds what the answering seat sends to its model, so the two
//! machines that can run it agree: the sender, when the seat runs on its own
//! model, and a fleet peer, when the sender submitted a
//! `WorkJob.single_shot` job (`darkmux-fleet`) and the RECEIVER builds the
//! persona from its own `radio-host` template. A sender never pushes a
//! system prompt: the role is the unit of trust in a receiver's allow-list,
//! so the receiver decides what the role says.

use crate::loader::{role_prompt, RADIO_HOST_ROLE_ID};
use anyhow::{anyhow, Result};
use darkmux_flow::payload::RadioSurface;

/// The answering seat's per-call completion budget when the operator has
/// not set `runtime.max_tokens_per_call`. The single-shot path's own
/// default is 4096, and a 35B thinking model spent exactly that reasoning
/// about "how do I see what is loaded?" and returned no text (2026-08-28).
/// 16,384 is the figure the same path already uses when reasoning effort
/// is set; a thinking model is the radio-host's normal staffing.
pub const RADIO_ANSWER_TOKEN_CAP: u32 = 16_384;

/// `runtime.max_tokens_per_call` when set (env or config.json), else
/// [`RADIO_ANSWER_TOKEN_CAP`]. The knob's documented meaning is exactly this
/// budget (reasoning + content of one call), so radio honors it rather than
/// growing a knob of its own.
pub fn answer_token_cap() -> u32 {
    darkmux_types::config_access::max_tokens_per_call().unwrap_or(RADIO_ANSWER_TOKEN_CAP)
}

/// The budget a peer runs a submitted answering seat under: the SMALLER of
/// what the sender asked for and this machine's own [`answer_token_cap`].
/// The machine that runs the model owns its limit, so its
/// `runtime.max_tokens_per_call` (or, unset, the radio default) bounds a
/// sender that asks for more; a sender can lower the budget, never raise it.
pub fn peer_token_cap(requested: u32) -> u32 {
    requested.min(answer_token_cap())
}

/// The `{{surface_instructions}}` substitution for the persona's rule 2
/// (`templates/builtin/roles/radio-host.md`): the same per-surface fact the
/// grounding bundle states (`render_surface_block` in `src/radio_answer.rs`),
/// phrased to slot into that rule's own sentence.
fn surface_instructions(surface: RadioSurface) -> String {
    match surface {
        RadioSurface::Cli | RadioSurface::Unknown => "on the command line, a catalog command is `darkmux mission launch \
             <id>` — never a bare `/id` and never the id on its own, since there is no shell \
             here that runs `/anything` and no such subcommand either; any other darkmux verb \
             is the full line from the command index (e.g. `darkmux machine status`)."
            .to_string(),
        RadioSurface::Panel => "in this panel, a catalog command runs as `/mission launch <id>` \
             (e.g. `/mission launch pr-list`), never as `/<id>` or the id on its own; any other \
             darkmux verb is a command the user types in a separate darkmux CLI shell, cited as \
             the full line from the command index (e.g. `darkmux machine status`)."
            .to_string(),
    }
}

/// Substitute every placeholder in the `radio-host` persona template.
/// A PURE function so the substitution is assertable on the FINISHED text
/// (#1861): the persona golden pins the TEMPLATE, and a template golden
/// structurally cannot catch a substitution that stops firing and ships a
/// raw `{{surface_instructions}}` to the model. Takes `persona` rather than
/// loading it, so a test can pin the SHIPPED template without resolving an
/// operator's own `~/.darkmux/roles/radio-host.md` override.
pub fn substitute_persona(persona: &str, humor: u8, surface: RadioSurface) -> String {
    persona
        .replace("{{humor}}", &humor.to_string())
        .replace("{{surface_instructions}}", &surface_instructions(surface))
}

/// The answering seat's finished system prompt on THIS machine: the
/// `radio-host` template (an operator-tier override wins, per
/// [`role_prompt`]) with `humor` and `surface` substituted.
pub fn answering_system_prompt(humor: u8, surface: RadioSurface) -> Result<String> {
    let persona = role_prompt(RADIO_HOST_ROLE_ID).ok_or_else(|| {
        anyhow!("radio-host role has no readable .md persona template — cannot dispatch the answering seat")
    })?;
    Ok(substitute_persona(&persona, humor, surface))
}

#[cfg(test)]
mod tests {
    use super::*;

    struct EnvGuard {
        key: &'static str,
        prev: Option<String>,
    }

    impl EnvGuard {
        fn set(key: &'static str, value: &str) -> Self {
            let prev = std::env::var(key).ok();
            // SAFETY: caller's test is #[serial_test::serial].
            unsafe { std::env::set_var(key, value) };
            Self { key, prev }
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            // SAFETY: caller's test is #[serial_test::serial].
            unsafe {
                match &self.prev {
                    Some(v) => std::env::set_var(self.key, v),
                    None => std::env::remove_var(self.key),
                }
            }
        }
    }

    #[test]
    fn substitute_persona_fills_every_placeholder_per_surface() {
        // The golden in `src/radio_answer.rs` pins the TEMPLATE; only this
        // pins the finished text, which is what actually reaches the model.
        const SHIPPED_TEMPLATE: &str =
            include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/../../templates/builtin/roles/radio-host.md"));
        assert!(SHIPPED_TEMPLATE.contains("{{surface_instructions}}"), "the template must still carry the placeholder");
        for (surface, needle) in
            // (#2050) The CLI needle is the BARE-ID clause, not just the
            // canonical form: the instruction already named the canonical
            // form and the seat still wrote a bare id, so what this pins
            // is the sentence that closes that gap.
            [(RadioSurface::Cli, "never the id on its own"), (RadioSurface::Panel, "/mission launch <id>")]
        {
            let prompt = substitute_persona(SHIPPED_TEMPLATE, 40, surface);
            assert!(!prompt.contains("{{"), "no placeholder may reach the model ({surface:?}): {prompt}");
            assert!(prompt.contains(needle), "the {surface:?} instruction must be substituted in: {prompt}");
            assert!(prompt.contains("40"), "the humor value must still substitute: {prompt}");
        }
        assert_ne!(
            substitute_persona(SHIPPED_TEMPLATE, 40, RadioSurface::Cli),
            substitute_persona(SHIPPED_TEMPLATE, 40, RadioSurface::Panel),
            "the two surfaces must not produce the same system prompt"
        );
    }

    /// The seat's per-call budget honors the operator's knob and otherwise
    /// gives a reasoning model room: the single-shot default of 4096 is what
    /// produced the empty answer.
    #[test]
    #[serial_test::serial]
    fn answer_token_cap_honors_the_config_knob_and_defaults_above_4096() {
        let _g = EnvGuard::set("DARKMUX_RUNTIME_MAX_TOKENS_PER_CALL", "12000");
        assert_eq!(answer_token_cap(), 12000);
        drop(_g);
        let _g = EnvGuard::set("DARKMUX_RUNTIME_MAX_TOKENS_PER_CALL", "");
        assert!(answer_token_cap() > 4096, "{}", answer_token_cap());
        assert_eq!(answer_token_cap(), RADIO_ANSWER_TOKEN_CAP);
    }

    /// The machine that runs the model owns its limit: a sender lowers the
    /// budget, never raises it past the receiver's own.
    #[test]
    #[serial_test::serial]
    fn a_peer_runs_under_the_smaller_of_the_requested_and_its_own_cap() {
        let _g = EnvGuard::set("DARKMUX_RUNTIME_MAX_TOKENS_PER_CALL", "12000");
        assert_eq!(peer_token_cap(50_000), 12_000, "the receiver's knob bounds a larger ask");
        assert_eq!(peer_token_cap(3_000), 3_000, "a smaller ask is honored");
        drop(_g);
        let _g = EnvGuard::set("DARKMUX_RUNTIME_MAX_TOKENS_PER_CALL", "");
        assert_eq!(peer_token_cap(50_000), RADIO_ANSWER_TOKEN_CAP, "unset, the radio default bounds it");
    }
}
