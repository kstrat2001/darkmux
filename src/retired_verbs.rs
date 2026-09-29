//! Verbs darkmux removed, each with the line that names its replacement.
//!
//! A removed verb is not an alias and does not parse: clap rejects it as an
//! unknown subcommand or argument. When it does, [`refusal`] looks the
//! command line up here, so the operator reads what replaced the verb
//! instead of a bare "unrecognized subcommand". It is consulted ONLY after
//! clap has rejected the command line, so an entry can never shadow a verb
//! that still exists.

/// One removed spelling: the leading words of the command line, and the flag
/// that is the retired part when the verb itself survives.
struct RetiredVerb {
    words: &'static [&'static str],
    flag: Option<&'static str>,
    remedy: &'static str,
}

const RETIRED: &[RetiredVerb] = &[
    RetiredVerb {
        words: &["mission", "dispatch"],
        flag: None,
        remedy: "To run a role on another machine, name a profile on that machine: \
                 `darkmux dispatch <role> \"<message>\" --profile <profile>@<machine> [--timeout N] \
                 [--no-wait]`. Missions come only from mission configs: \
                 `darkmux mission launch <config>`.",
    },
    RetiredVerb {
        words: &["mission", "add-phase"],
        flag: None,
        remedy: "A mission's phases come from its mission config: write or edit the config \
                 (`darkmux mission config show <config>` prints one to start from), then \
                 `darkmux mission launch <config>`.",
    },
    RetiredVerb {
        words: &["mission", "start"],
        flag: None,
        remedy: "`darkmux mission launch <config>` starts the mission it creates.",
    },
    RetiredVerb {
        words: &["mission", "pause"],
        flag: None,
        remedy: "A mission runs from `darkmux mission launch <config>` and ends with \
                 `darkmux mission finalize <id>` or `darkmux mission abort <id>`.",
    },
    RetiredVerb {
        words: &["mission", "resume"],
        flag: None,
        remedy: "A mission runs from `darkmux mission launch <config>` and ends with \
                 `darkmux mission finalize <id>` or `darkmux mission abort <id>`.",
    },
    RetiredVerb {
        words: &["dispatch"],
        flag: Some("--phase-id"),
        remedy: "A dispatch no longer attaches to another mission's phase. Run the work as a \
                 step of a mission config (`darkmux mission launch <config>`), or dispatch \
                 without `--phase-id`.",
    },
];

impl RetiredVerb {
    fn matches(&self, args: &[String]) -> bool {
        let leads = self.words.len() <= args.len()
            && self.words.iter().zip(args).all(|(w, a)| w == a);
        if !leads {
            return false;
        }
        let Some(flag) = self.flag else { return true };
        let prefixed = format!("{flag}=");
        args[self.words.len()..]
            .iter()
            .take_while(|a| a.as_str() != "--")
            .any(|a| a == flag || a.starts_with(&prefixed))
    }

    fn spelling(&self) -> String {
        let mut s = format!("darkmux {}", self.words.join(" "));
        if let Some(flag) = self.flag {
            s.push(' ');
            s.push_str(flag);
        }
        s
    }
}

/// The refusal for a command line (`args` without the program name) that
/// names a removed verb, or `None` when it names none.
pub(crate) fn refusal(args: &[String]) -> Option<String> {
    RETIRED.iter().find(|r| r.matches(args)).map(|r| {
        format!("error: `{}` was removed in 4.0 (#2954). {}", r.spelling(), r.remedy)
    })
}

#[cfg(test)]
mod tests {
    use super::refusal;

    fn args(s: &[&str]) -> Vec<String> {
        s.iter().map(|a| a.to_string()).collect()
    }

    #[test]
    fn each_retired_spelling_is_named_with_its_remedy() {
        let msg = refusal(&args(&["mission", "dispatch", "m", "--role", "coder"])).unwrap();
        // drift-guard:allow mission dispatch — asserts the refusal names the retired verb
        assert!(msg.contains("`darkmux mission dispatch` was removed"), "{msg}");
        assert!(msg.contains("--profile <profile>@<machine>"), "{msg}");
        for verb in ["add-phase", "start", "pause", "resume"] {
            let msg = refusal(&args(&["mission", verb, "m"])).unwrap();
            assert!(msg.contains(&format!("`darkmux mission {verb}` was removed")), "{msg}");
            assert!(msg.contains("darkmux mission launch <config>"), "{msg}");
        }
    }

    #[test]
    fn the_phase_id_flag_is_found_in_either_spelling_before_the_separator() {
        for a in [
            args(&["dispatch", "coder", "m", "--phase-id", "p"]),
            args(&["dispatch", "coder", "--phase-id=p", "m"]),
        ] {
            let msg = refusal(&a).unwrap();
            // drift-guard:allow dispatch --phase-id — asserts the refusal names the retired flag
            assert!(msg.contains("`darkmux dispatch --phase-id` was removed"), "{msg}");
        }
        // After `--` the text is the message, never a flag.
        assert!(refusal(&args(&["dispatch", "coder", "--", "--phase-id", "p"])).is_none());
    }

    #[test]
    fn a_live_verb_or_an_unrelated_typo_is_not_refused() {
        for a in [
            args(&["dispatch", "coder", "m", "--bogus"]),
            args(&["mission", "frobnicate"]),
            args(&["mission"]),
            args(&["missions", "dispatch"]),
            args(&[]),
        ] {
            assert!(refusal(&a).is_none(), "{a:?}");
        }
    }
}
