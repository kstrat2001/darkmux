//! Verb spellings a 4.0 break retired, each with the command that replaced it.
//! clap alone answers a retired spelling with "unrecognized subcommand"; this
//! runs before it so the operator is told where the verb went. No aliases:
//! the retired spelling never runs.

/// One retired spelling (the leading words after `darkmux`) and its
/// replacement, as a command.
struct Retired {
    spelling: &'static [&'static str],
    replacement: &'static str,
}

const RETIRED: &[Retired] = &[
    Retired { spelling: &["lab", "run", "list"], replacement: "darkmux run list --kind lab" },
    Retired { spelling: &["lab", "run", "inspect"], replacement: "darkmux run inspect <run>" },
    Retired { spelling: &["lab", "run", "stats"], replacement: "darkmux run stats <run>..." },
    Retired { spelling: &["lab", "run", "compare"], replacement: "darkmux run compare <a> <b>" },
];

/// The refusal for `args` (the full argv, program name first) when it opens
/// with a retired spelling.
pub(crate) fn refusal(args: &[String]) -> Option<String> {
    let words = args.get(1..)?;
    RETIRED.iter().find(|r| words.iter().map(String::as_str).take(r.spelling.len()).eq(r.spelling.iter().copied())).map(|r| {
        format!(
            "`darkmux {}` was retired in 4.0: recorded runs are read through `darkmux run`. Use `{}`. \
             `darkmux lab run <workload>` still launches a lab run.",
            r.spelling.join(" "),
            r.replacement
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(words: &[&str]) -> Vec<String> {
        std::iter::once("darkmux").chain(words.iter().copied()).map(String::from).collect()
    }

    #[test]
    fn every_retired_spelling_names_its_replacement() {
        for r in RETIRED {
            let msg = refusal(&argv(r.spelling)).unwrap_or_else(|| panic!("{:?} not refused", r.spelling));
            assert!(msg.contains(r.replacement), "{msg}");
        }
    }

    #[test]
    fn the_launcher_and_the_new_verbs_are_not_refused() {
        assert_eq!(refusal(&argv(&["lab", "run", "quick-q"])), None);
        assert_eq!(refusal(&argv(&["run", "inspect", "x"])), None);
        assert_eq!(refusal(&argv(&["lab", "run"])), None);
        assert_eq!(refusal(&argv(&[])), None);
    }
}
