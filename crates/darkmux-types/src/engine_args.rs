//! (#3453) The `engine_args` a model on a Flint endpoint may carry: flags of
//! rec's `rec-mlx serve flint`, from the fixed set rec 0.1.0 accepts. rec has
//! no `capabilities` command to ask (Tensorfold's flags are checked against
//! the installed binary by `darkmux doctor`), so this set is checked at load,
//! by every dispatching preflight and by doctor. A refusal never echoes a
//! flag's value: it may be a secret.

/// What one flag takes.
#[derive(Debug, Clone, Copy)]
enum Takes {
    /// A switch: no value.
    Nothing,
    /// One of the listed values.
    OneOf(&'static [&'static str]),
    /// A number (`0.6`).
    Number,
    /// A whole number (`20`).
    WholeNumber,
    /// Any value (`--metrics`' JSONL path).
    Any,
}

/// The flags of `rec-mlx serve flint` (rec 0.1.0) an operator may pass.
const FLINT_FLAGS: &[(&str, Takes)] = &[
    ("--grammar", Takes::OneOf(&["none", "flint"])),
    ("--enforce-tool-schemas", Takes::Nothing),
    ("--metrics", Takes::Any),
    ("--temperature", Takes::Number),
    ("--top-p", Takes::Number),
    ("--top-k", Takes::WholeNumber),
    ("--max-tokens", Takes::WholeNumber),
    ("--fast-sync", Takes::Nothing),
    ("--no-fast-sync", Takes::Nothing),
];

/// The flags darkmux sets itself on a Flint server, and why each is not the
/// operator's to pass.
fn reserved(flag: &str) -> Option<&'static str> {
    Some(match flag {
        "--model-dir" | "--manifest" => "darkmux sets it from rec's own install, for the model the profile's `id` names",
        "--context-limit" => "darkmux sets it from the model's `n_ctx`",
        "--port" => "darkmux sets it from the endpoint's `port`",
        "--api-key-file" => "darkmux serves Flint on loopback with no key, as it does Tensorfold",
        _ => return None,
    })
}

impl Takes {
    /// `value` is one this flag takes, or the refusal (never naming `value`).
    fn check(self, flag: &str, value: &str) -> Result<(), String> {
        let ok = match self {
            Takes::Nothing => return Err(format!("`engine_args` gives `{flag}` a value, and `{flag}` takes no value")),
            Takes::OneOf(allowed) => allowed.contains(&value),
            Takes::Number => value.parse::<f64>().is_ok_and(f64::is_finite),
            Takes::WholeNumber => value.parse::<i64>().is_ok(),
            Takes::Any => true,
        };
        if ok {
            return Ok(());
        }
        Err(match self {
            Takes::OneOf(allowed) => format!("`engine_args`: `{flag}` takes one of {}", allowed.join(", ")),
            Takes::WholeNumber => format!("`engine_args`: `{flag}` takes a whole number"),
            _ => format!("`engine_args`: `{flag}` takes a number"),
        })
    }
}

/// Why `args` (a Flint model's `engine_args`) cannot be used as written, or
/// `None`.
pub(crate) fn flint_problem(args: &[String]) -> Option<String> {
    flint_check(args).err()
}

fn flint_check(args: &[String]) -> Result<(), String> {
    let mut reader = Reader::default();
    for arg in args {
        if let Some(why) = crate::malformed_engine_arg(arg) {
            return Err(why);
        }
        if arg.starts_with("--") {
            reader.flag(arg)?;
        } else {
            reader.value(arg)?;
        }
    }
    match reader.pending {
        Some((flag, _)) => Err(missing_value(flag)),
        None => Ok(()),
    }
}

/// Where a left-to-right read of `engine_args` stands.
#[derive(Default)]
struct Reader {
    /// The flag whose value the next token must be.
    pending: Option<(&'static str, Takes)>,
    /// The switch just read, which a following value would wrongly go to.
    last_switch: Option<&'static str>,
}

impl Reader {
    /// A token that is not a flag: the value of the flag before it.
    fn value(&mut self, arg: &str) -> Result<(), String> {
        match (self.pending.take(), self.last_switch.take()) {
            (Some((flag, takes)), _) => takes.check(flag, arg),
            (None, Some(flag)) => Takes::Nothing.check(flag, arg),
            (None, None) => Err("`engine_args` holds a value with no flag before it; remove it".to_string()),
        }
    }

    /// A `--flag` or `--flag=value` token.
    fn flag(&mut self, arg: &str) -> Result<(), String> {
        if let Some((flag, _)) = self.pending {
            return Err(missing_value(flag));
        }
        let (flag, inline) = match arg.split_once('=') {
            Some((f, v)) => (f, Some(v)),
            None => (arg, None),
        };
        let (known, takes) = lookup(flag)?;
        self.last_switch = None;
        match (takes, inline) {
            (Takes::Nothing, None) => self.last_switch = Some(known),
            (_, Some(v)) => takes.check(known, v)?,
            (_, None) => self.pending = Some((known, takes)),
        }
        Ok(())
    }
}

/// `flag` in [`FLINT_FLAGS`], or why it is not the operator's to pass.
fn lookup(flag: &str) -> Result<(&'static str, Takes), String> {
    if let Some(why) = reserved(flag) {
        return Err(format!("`engine_args` names `{flag}`, which is not yours to pass: {why}. Remove it"));
    }
    FLINT_FLAGS.iter().find(|(f, _)| *f == flag).copied().ok_or_else(|| {
        let accepted: Vec<&str> = FLINT_FLAGS.iter().map(|(f, _)| *f).collect();
        format!(
            "`engine_args` names `{flag}`, which is not a flag `rec-mlx serve flint` accepts (rec 0.1.0): {}",
            accepted.join(", ")
        )
    })
}

fn missing_value(flag: &str) -> String {
    format!("`engine_args`: `{flag}` takes a value, and none follows it")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The usage line `rec-mlx serve flint --help` printed for rec 0.1.0's
    /// runtime on the laptop (2026-11-09, read-only).
    const REC_0_1_0_USAGE: &str = "usage: cli.py [-h] --model-dir MODEL_DIR --manifest MANIFEST
              [--context-limit CONTEXT_LIMIT] [--port PORT]
              [--grammar {none,flint}] [--enforce-tool-schemas]
              [--api-key-file API_KEY_FILE] [--metrics METRICS]
              [--temperature TEMPERATURE] [--top-p TOP_P] [--top-k TOP_K]
              [--max-tokens MAX_TOKENS] [--fast-sync | --no-fast-sync]
              {chat,serve} {flint}";

    /// Each `--flag` in the usage and whether a value follows it within its
    /// own `[...]` group (a bracket or `|` ends a group).
    fn usage_arity() -> Vec<(String, bool)> {
        let spaced = REC_0_1_0_USAGE.replace('[', " [ ").replace(']', " ] ").replace('|', " | ");
        let tokens: Vec<&str> = spaced.split_whitespace().collect();
        tokens
            .iter()
            .enumerate()
            .filter(|(_, t)| t.starts_with("--"))
            .map(|(i, t)| (t.to_string(), tokens.get(i + 1).is_some_and(|n| !n.starts_with('-') && !["[", "]", "|"].contains(n))))
            .collect()
    }

    /// (#3453 review) The flag table matches rec 0.1.0's own usage: every
    /// flag it lists is either one darkmux sets (reserved) or in the table,
    /// and each table flag takes a value exactly when the usage gives it one.
    /// A switch read as taking no value when rec wants one (`--metrics
    /// METRICS`) is caught here.
    #[test]
    fn the_flag_table_matches_rec_0_1_0s_usage_including_arity() {
        let usage = usage_arity();
        assert_eq!(usage.len(), 14, "the usage parses: {usage:?}");
        for (flag, takes_value) in &usage {
            if reserved(flag).is_some() {
                continue;
            }
            let (_, takes) = FLINT_FLAGS.iter().find(|(f, _)| f == flag).unwrap_or_else(|| panic!("{flag} is in the table"));
            assert_eq!(!matches!(takes, Takes::Nothing), *takes_value, "{flag}'s arity");
        }
        for (flag, _) in FLINT_FLAGS {
            assert!(usage.iter().any(|(f, _)| f == flag), "{flag} is one rec 0.1.0 lists");
        }
    }
}
