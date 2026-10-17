//! (#3264) A reply that still carries the model's raw channel markup.
//!
//! Some model families address each message to a channel: Muse Glimmer
//! (`to=self` for reasoning, `to=user` for the answer) and gpt-oss's Harmony
//! (`<|channel|>analysis`, `<|channel|>final`). An engine with a parser for
//! the format returns the answer in `content` and the reasoning in
//! `reasoning_content`; one without passes the markup through, so the reply
//! opens with the model's reasoning. darkmux leaves parsing to the engine and
//! never strips the markup: it says the engine did not parse the format and
//! names engines that do. The reply is delivered as the model wrote it.

/// The channel format a reply's markup belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    MuseGlimmer,
    Harmony,
}

/// A reply that opens with unparsed channel markup.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Leak {
    pub format: Format,
    /// The marker as it appeared, for the warning.
    pub marker: String,
}

/// How far into a reply a Muse Glimmer recipient may run before its
/// `<|message|>`: recipients are short names (`self`, `user`, a tool name).
const RECIPIENT_MAX: usize = 64;

const MESSAGE: &str = "<|message|>";
const CHANNEL: &str = "<|channel|>";
const START: &str = "<|start|>assistant";

/// The leak a reply shows, if it OPENS with raw channel markup. Only the
/// opening counts: an unparsed reply starts with its first channel, while a
/// reply that mentions the markup later (a review quoting a parser, an
/// answer explaining the format) is content.
pub fn leaked(reply: &str) -> Option<Leak> {
    let text = reply.trim_start();
    let text = text.strip_prefix(START).map(str::trim_start).unwrap_or(text);
    if text.starts_with(CHANNEL) {
        return Some(Leak { format: Format::Harmony, marker: CHANNEL.to_string() });
    }
    let recipient = text.strip_prefix("to=")?;
    let end = recipient.find(MESSAGE)?;
    let name = &recipient[..end];
    let is_name = !name.is_empty()
        && end <= RECIPIENT_MAX
        && name.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.' | ':'));
    is_name.then(|| Leak { format: Format::MuseGlimmer, marker: format!("to={name}{MESSAGE}") })
}

/// The operator-facing line for a leak from `model`.
pub fn warning(leak: &Leak, model: &str) -> String {
    let (family, engines) = match leak.format {
        Format::MuseGlimmer => (
            "Muse Glimmer",
            "llama.cpp's llama-server b10353 or later, or vLLM with its muse_glimmer reasoning parser",
        ),
        Format::Harmony => ("Harmony (gpt-oss)", "llama.cpp's llama-server, or vLLM"),
    };
    format!(
        "darkmux dispatch: the reply from `{model}` opens with raw {family} channel markup (`{}`): the engine \
         serving it did not parse this model's format, so the model's reasoning is in the reply, printed as \
         the model wrote it. Engines that parse it: {engines}. (#3264)",
        leak.marker
    )
}

#[cfg(test)]
#[path = "channel_markup_tests.rs"]
mod tests;
