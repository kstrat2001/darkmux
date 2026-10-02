//! Verb spellings darkmux retired, each with the line that names its
//! replacement. This is the ONE table of them: no aliases, the retired
//! spelling never runs. [`refusal`] runs on the raw command line BEFORE clap,
//! because a retired spelling can still parse (`lab run list` reads as the
//! launcher for a workload named `list`), so clap's own rejection cannot be
//! the trigger. An entry must therefore name a spelling that has no live
//! meaning.

/// One retired spelling: the leading words of the command line, and the flag
/// that is the retired part when the verb itself survives.
struct RetiredVerb {
    words: &'static [&'static str],
    flag: Option<&'static str>,
    remedy: &'static str,
}

/// The remedy for a retired `lab run <verb>` read spelling: where the read
/// went, and how a workload that shares the verb's name still launches.
macro_rules! run_read_remedy {
    ($verb:literal, $use:literal) => {
        concat!(
            "Recorded runs are read through `darkmux run`. Use `",
            $use,
            "`. `darkmux lab run <workload>` still launches a lab run; for a workload named `",
            $verb,
            "`, use `darkmux lab run -- ",
            $verb,
            "`."
        )
    };
}

/// The remedy for a retired `--session-id` flag on a `flow` verb: its
/// replacement takes the `exec-...` id of the role execution the record is
/// about (what `darkmux dispatch` prints), not the session id the old flag took.
const EXECUTION_ID_REMEDY: &str = "Use `--execution <id>`, which takes the `exec-...` id of the role \
                                   execution (`darkmux dispatch` prints it), not a session id.";

const RETIRED: &[RetiredVerb] = &[
    RetiredVerb {
        words: &["lab", "run", "list"],
        flag: None,
        remedy: run_read_remedy!("list", "darkmux run list --kind lab"),
    },
    RetiredVerb {
        words: &["lab", "run", "inspect"],
        flag: None,
        remedy: run_read_remedy!("inspect", "darkmux run inspect <run>"),
    },
    RetiredVerb {
        words: &["lab", "run", "stats"],
        flag: None,
        remedy: run_read_remedy!("stats", "darkmux run stats <run>..."),
    },
    RetiredVerb {
        words: &["lab", "run", "compare"],
        flag: None,
        remedy: run_read_remedy!("compare", "darkmux run compare <a> <b>"),
    },
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
        words: &["finding", "list"],
        flag: Some("--dispatch"),
        remedy: "Findings are keyed by the role execution that filed them: use \
                 `darkmux finding list --execution <execution-id>`.",
    },
    RetiredVerb {
        words: &["dispatch"],
        flag: Some("--phase-id"),
        remedy: "A dispatch no longer attaches to another mission's phase. Run the work as a \
                 step of a mission config (`darkmux mission launch <config>`), or dispatch \
                 without `--phase-id`.",
    },
    RetiredVerb {
        words: &["dispatch"],
        flag: Some("--session-id"),
        remedy: "`session` is an internal word, and the flag names the dispatch, not a session: \
                 use `darkmux dispatch <role> \"<message>\" --name <name>`.",
    },
    RetiredVerb {
        words: &["flow", "note"],
        flag: Some("--session-id"),
        remedy: EXECUTION_ID_REMEDY,
    },
    RetiredVerb {
        words: &["flow", "catch"],
        flag: Some("--session-id"),
        remedy: EXECUTION_ID_REMEDY,
    },
    RetiredVerb {
        words: &["flow", "record"],
        flag: Some("--session-id"),
        remedy: EXECUTION_ID_REMEDY,
    },
    RetiredVerb {
        words: &["flow", "tier-decision"],
        flag: Some("--session-id"),
        remedy: EXECUTION_ID_REMEDY,
    },
    RetiredVerb {
        words: &["flow", "tail"],
        flag: Some("--session"),
        remedy: "Use `darkmux flow tail --execution <id>`, which takes the `exec-...` id of a role \
                 execution (`darkmux dispatch` prints it), not a session id.",
    },
    RetiredVerb {
        words: &["memory", "correction", "list"],
        flag: Some("--session"),
        remedy: "Use `darkmux memory correction list --execution <id>`, which takes the `exec-...` id \
                 of a role execution (`darkmux dispatch` prints it), not a session id.",
    },
    RetiredVerb {
        words: &["lab", "run"],
        flag: Some("--runs"),
        remedy: "`run` names the umbrella over mission, dispatch and lab runs, not a repeat count: \
                 use `darkmux lab run <workload> --repeat N` (`-n N` still works).",
    },
    RetiredVerb {
        words: &["lab", "tune"],
        flag: Some("--runs"),
        remedy: "`run` names the umbrella over mission, dispatch and lab runs, not a repeat count: \
                 use `darkmux lab tune <workload> --repeat N` (`-n N` still works).",
    },
    RetiredVerb {
        words: &["machine", "list"],
        flag: Some("--deep"),
        remedy: "`darkmux machine list` prints each machine's card by default (hardware, loaded \
                 models, profiles, seats, thermal state), so there is nothing to ask for: drop the flag.",
    },
    RetiredVerb {
        words: &["mission", "status"],
        flag: Some("--missions"),
        remedy: "The flag hides machine-minted runs and shows the missions you named: use \
                 `darkmux mission status --named`.",
    },
];

/// Spellings removed in 5.0, kept apart from [`RETIRED`] so the refusal names
/// the release that removed each.
const RETIRED_5_0: &[RetiredVerb] = &[RetiredVerb {
    words: &["lab", "eval"],
    flag: None,
    remedy: "The role-eval harness and its corpus are gone (#3036). To measure a model on a task, \
             write a workload and run it with `darkmux lab run <workload>`; to review a diff, \
             launch the `review` mission with `darkmux mission launch review`.",
}];

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
/// names a retired spelling, or `None` when it names none.
pub(crate) fn refusal(args: &[String]) -> Option<String> {
    [("4.0", RETIRED), ("5.0", RETIRED_5_0)].into_iter().find_map(|(release, table)| {
        table
            .iter()
            .find(|r| r.matches(args))
            .map(|r| format!("`{}` was removed in {release}. {}", r.spelling(), r.remedy))
    })
}

/// Every retired spelling, as the line a user would have typed, for tests that
/// assert a model-facing surface never offers one (F12).
#[cfg(test)]
pub(crate) fn retired_spellings() -> Vec<String> {
    RETIRED.iter().chain(RETIRED_5_0).map(RetiredVerb::spelling).collect()
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

    /// (#3036) `lab eval` is refused whole, with any flags or positional it
    /// used to take, naming the 5.0 release and both replacements.
    #[test]
    fn lab_eval_is_refused_whole_naming_its_replacements() {
        for a in [
            args(&["lab", "eval"]),
            args(&["lab", "eval", "pr-reviewer", "--mode", "agentic", "--workdirs", "w"]),
            args(&["lab", "eval", "--profile", "p"]),
        ] {
            let msg = refusal(&a).unwrap_or_else(|| panic!("{a:?} was not refused"));
            // drift-guard:allow lab eval — asserts the refusal names the retired verb
            assert!(msg.contains("`darkmux lab eval` was removed in 5.0"), "{msg}");
            assert!(msg.contains("darkmux lab run <workload>"), "{msg}");
            assert!(msg.contains("darkmux mission launch review"), "{msg}");
        }
    }

    #[test]
    fn the_finding_list_dispatch_flag_names_execution() {
        let msg = refusal(&args(&["finding", "list", "--dispatch", "k"])).unwrap();
        // drift-guard:allow finding list --dispatch — asserts the refusal names the retired flag
        assert!(msg.contains("`darkmux finding list --dispatch` was removed"), "{msg}");
        assert!(msg.contains("--execution"), "{msg}");
        assert!(refusal(&args(&["finding", "list", "--execution", "k"])).is_none());
    }

    /// `(argv, the spelling the refusal names, the replacement it must name)`.
    const RETIRED_FLAGS: &[(&[&str], &str, &str)] = &[
        (&["dispatch", "coder", "hi", "--session-id", "x"], "darkmux dispatch --session-id", "--name"),  // drift-guard:allow retired flag: asserts the refusal names it
        (&["flow", "note", "--text", "t", "--session-id=s"], "darkmux flow note --session-id", "--execution"),  // drift-guard:allow retired flag: asserts the refusal names it
        (&["flow", "catch", "--text", "t", "--session-id", "s"], "darkmux flow catch --session-id", "--execution"),  // drift-guard:allow retired flag: asserts the refusal names it
        (&["flow", "record", "--session-id", "s"], "darkmux flow record --session-id", "--execution"),  // drift-guard:allow retired flag: asserts the refusal names it
        (&["flow", "tier-decision", "--session-id", "s"], "darkmux flow tier-decision --session-id", "--execution"),  // drift-guard:allow retired flag: asserts the refusal names it
        (&["flow", "tail", "--session", "s"], "darkmux flow tail --session", "--execution"),  // drift-guard:allow retired flag: asserts the refusal names it
        (&["memory", "correction", "list", "--session", "s"], "darkmux memory correction list --session", "--execution"),  // drift-guard:allow retired flag: asserts the refusal names it
        (&["lab", "run", "quick-q", "--runs", "3"], "darkmux lab run --runs", "--repeat"),  // drift-guard:allow retired flag: asserts the refusal names it
        (&["lab", "tune", "quick-q", "--runs=3"], "darkmux lab tune --runs", "--repeat"),  // drift-guard:allow retired flag: asserts the refusal names it
        (&["mission", "status", "--missions"], "darkmux mission status --missions", "--named"),
        (&["machine", "list", "--deep"], "darkmux machine list --deep", "by default"),  // drift-guard:allow retired flag: asserts the refusal names it
    ];

    #[test]
    fn each_retired_flag_is_refused_naming_its_replacement() {
        for (argv, spelling, replacement) in RETIRED_FLAGS {
            let msg = refusal(&args(argv)).unwrap_or_else(|| panic!("{argv:?} was not refused"));
            assert!(msg.contains(&format!("`{spelling}` was removed")), "{msg}");
            assert!(msg.contains(replacement), "{msg}");
        }
    }

    /// The replacements are live spellings that parse, and a retired flag
    /// after the `--` separator is message text, never a flag.
    #[test]
    fn the_replacement_spellings_parse_and_are_not_refused() {
        use clap::Parser;
        for argv in [
            &["dispatch", "coder", "hi", "--name", "x"][..],
            &["flow", "note", "--text", "t", "--execution", "exec-1-2-3"],
            &["flow", "catch", "--text", "t", "--execution", "exec-1-2-3"],
            &["flow", "tail", "--execution", "exec-1-2-3"],
            &["memory", "correction", "list", "--execution", "exec-1-2-3"],
            &["lab", "run", "quick-q", "--repeat", "3"],
            &["lab", "run", "quick-q", "-n", "3"],
            &["lab", "tune", "quick-q", "--repeat", "3"],
            &["mission", "status", "--named"],
        ] {
            assert!(refusal(&args(argv)).is_none(), "{argv:?}");
            let mut full = vec!["darkmux"];
            full.extend_from_slice(argv);
            if let Err(e) = crate::cli::Cli::try_parse_from(&full) {
                panic!("{full:?} did not parse: {e}");
            }
        }
        assert!(refusal(&args(&["dispatch", "coder", "--", "--session-id"])).is_none());
    }

    #[test]
    fn a_live_verb_or_an_unrelated_typo_is_not_refused() {
        for a in [
            args(&["dispatch", "coder", "m", "--bogus"]),
            args(&["mission", "frobnicate"]),
            args(&["lab", "run", "quick-q"]),
            args(&["lab", "run"]),
            args(&["run", "inspect", "x"]),
            // The `--` escape still reaches the launcher, for a workload named `list`.
            args(&["lab", "run", "--", "list"]),
            args(&["mission"]),
            args(&["missions", "dispatch"]),
            args(&[]),
        ] {
            assert!(refusal(&a).is_none(), "{a:?}");
        }
    }

    #[test]
    fn a_retired_run_read_spelling_names_its_replacement_and_the_escape() {
        for (verb, want) in [
            ("list", "darkmux run list --kind lab"),
            ("inspect", "darkmux run inspect <run>"),
            ("stats", "darkmux run stats <run>..."),
            ("compare", "darkmux run compare <a> <b>"),
        ] {
            let msg = refusal(&args(&["lab", "run", verb])).unwrap();
            assert!(msg.contains(&format!("`darkmux lab run {verb}` was removed")), "{msg}");
            assert!(msg.contains(want), "{msg}");
            assert!(msg.contains(&format!("darkmux lab run -- {verb}")), "{msg}");
        }
    }

    /// The escape the refusal names really reaches the launcher: clap reads
    /// the word after `--` as the workload, not as a retired verb.
    #[test]
    fn the_escape_the_refusal_names_parses_as_the_launcher() {
        use clap::Parser;
        let argv = ["darkmux", "lab", "run", "--", "list"];
        let cli = crate::cli::Cli::try_parse_from(argv).ok().unwrap();
        match cli.command {
            crate::cli::Cmd::Lab { sub: crate::cli::LabCmd::Run { workload, .. } } => assert_eq!(workload, "list"),
            _ => panic!("`lab run -- list` did not parse as the launcher"),
        }
    }
}
