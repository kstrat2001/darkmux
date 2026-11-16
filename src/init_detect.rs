//! (#3569) `darkmux init`'s engine detection and `--add-engine`.
//!
//! Every run reports the engines darkmux starts servers for (Tensorfold,
//! rec's Flint) that are installed here (`darkmux_profiles::engine::detect`),
//! each usable or not and why, and says plainly when none is usable; LM
//! Studio and a server are what the default-engine step reports. darkmux
//! recommends none of them.
//!
//! `--add-engine <tensorfold|flint>` adds a detected engine to
//! `profiles.json` (`darkmux_profiles::engine::add`): an endpoint and a
//! profile running its model. It writes only after the operator confirms
//! on the terminal; with no terminal, or on a dry run, it shows what it
//! would add and writes nothing.

use anyhow::{anyhow, Result};
use darkmux_profiles::engine::add::{self, AddRequest, Planned};
use darkmux_profiles::engine::detect::{Detected, DetectedEngine, DetectedKind, DetectedState};
use std::io::IsTerminal;
use std::path::Path;

/// `--add-engine`'s value as the engine it names. Every engine detection
/// knows parses (LM Studio is then refused by the add, naming why); any
/// other word is refused naming the engines darkmux adds.
pub fn parse_engine(word: &str) -> Result<DetectedKind> {
    serde_json::from_value(serde_json::Value::from(word.trim().to_ascii_lowercase()))
        .ok()
        .filter(|k| *k != DetectedKind::Url)
        .ok_or_else(|| anyhow!("--add-engine: `{word}` is not an engine darkmux adds; it adds `tensorfold` or `flint`"))
}

/// What `--add-engine` did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AddOutcome {
    /// Written to profiles.json.
    Added(Planned),
    /// Planned and shown; nothing was written (a dry run, no terminal, or
    /// the operator said no). `why` says which.
    NotWritten {
        planned: Planned,
        why: &'static str,
    },
    Refused(String),
}

/// The report lines for the engines found.
pub fn detected_lines(d: &Detected) -> Vec<String> {
    if d.detected.is_empty() {
        return vec!["engines darkmux starts servers for: none installed (it looks for Tensorfold and rec's Flint).".to_string()];
    }
    let head = if d.usable {
        "engines darkmux starts servers for:"
    } else {
        "engines darkmux starts servers for: none of these is usable."
    };
    std::iter::once(head.to_string())
        .chain(d.detected.iter().map(engine_line))
        .collect()
}

fn engine_line(e: &DetectedEngine) -> String {
    let count = match e.models.len() {
        1 => format!("`{}`", e.models[0].id),
        n => format!("{n} models"),
    };
    match e.state {
        DetectedState::Usable if e.can_add => {
            format!(
                "  {}: usable ({count}), not configured; {} adds it after you confirm",
                e.named(),
                e.add_command()
            )
        }
        DetectedState::Usable => format!("  {}: usable ({count}){}", e.named(), configured_as(e)),
        DetectedState::Unusable => format!(
            "  {}: not usable: {}",
            e.named(),
            e.reason.as_deref().unwrap_or_default()
        ),
        DetectedState::Unknown => format!(
            "  {}: unknown: {}",
            e.named(),
            e.reason.as_deref().unwrap_or_default()
        ),
    }
}

fn configured_as(e: &DetectedEngine) -> String {
    if e.endpoints.is_empty() {
        String::new()
    } else {
        format!(
            ", endpoint {}",
            e.endpoints
                .iter()
                .map(|x| format!("`{x}`"))
                .collect::<Vec<_>>()
                .join(", ")
        )
    }
}

/// The report line for an add.
pub fn add_line(outcome: &AddOutcome, path: &Path) -> String {
    match outcome {
        AddOutcome::Added(p) => format!("add engine: added {} to {}", p.summary(), path.display()),
        AddOutcome::NotWritten { planned, why } => {
            format!(
                "add engine: would add {} to {}; nothing written ({why})",
                planned.summary(),
                path.display()
            )
        }
        AddOutcome::Refused(why) => format!("add engine: not added. {why}"),
    }
}

/// Asks the operator to confirm an add. A trait so tests decide.
pub trait Confirm {
    /// `Some(true)` confirmed, `Some(false)` declined, `None` nobody to ask.
    fn confirm(&mut self, question: &str) -> Option<bool>;
}

/// Asks on the terminal; with none, asks nobody.
pub struct TerminalConfirm;

impl Confirm for TerminalConfirm {
    fn confirm(&mut self, question: &str) -> Option<bool> {
        let interactive = std::io::stdin().is_terminal() && std::io::stderr().is_terminal();
        ask_yes_no(
            interactive,
            question,
            &mut std::io::stdin().lock(),
            &mut std::io::stderr(),
        )
    }
}

/// `question` and `[y/N]` on `out`, one line from `input`: `y` or `yes`
/// confirms, anything else declines. Nobody to ask is `None`.
pub fn ask_yes_no(
    interactive: bool,
    question: &str,
    input: &mut dyn std::io::BufRead,
    out: &mut dyn std::io::Write,
) -> Option<bool> {
    if !interactive {
        return None;
    }
    let _ = write!(out, "{question} [y/N] ");
    let _ = out.flush();
    let mut line = String::new();
    input.read_line(&mut line).ok()?;
    Some(matches!(
        line.trim().to_ascii_lowercase().as_str(),
        "y" | "yes"
    ))
}

/// Plan the add from what was found, ask, and write when confirmed.
/// `text` is the registry at `path` as read; a dry run asks nobody.
pub fn add_engine(
    req: &AddRequest,
    found: &Detected,
    path: &Path,
    text: &str,
    dry_run: bool,
    confirm: &mut dyn Confirm,
) -> AddOutcome {
    let planned = match add::plan(text, path, &found.detected, req, &port_free) {
        Ok(p) => p,
        Err(r) => return AddOutcome::Refused(r.to_string()),
    };
    if dry_run {
        return AddOutcome::NotWritten {
            planned,
            why: "a dry run",
        };
    }
    let question = format!(
        "darkmux init: add {} to {}?",
        planned.summary(),
        path.display()
    );
    match confirm.confirm(&question) {
        None => AddOutcome::NotWritten {
            planned,
            why: "no terminal to confirm on; run it on one",
        },
        Some(false) => AddOutcome::NotWritten {
            planned,
            why: "not confirmed",
        },
        Some(true) => match add::write(path, text, &planned) {
            Ok(()) => AddOutcome::Added(planned),
            Err(r) => AddOutcome::Refused(r.to_string()),
        },
    }
}

/// Nothing listens on `port` (a host that cannot say counts as busy).
fn port_free(port: u16) -> bool {
    darkmux_platform::current()
        .tcp_listeners(port)
        .is_some_and(|pids| pids.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;
    use darkmux_profiles::engine::detect::{judge_flint, FlintFacts};
    use darkmux_profiles::engine::flint::Pulled;

    fn flint(endpoints: Vec<String>) -> DetectedEngine {
        let pulled = vec![Pulled {
            revision: "flint-int4".into(),
            max_context: Some(262_144),
        }];
        judge_flint(
            FlintFacts {
                version: Ok("0.1.0".into()),
                runtime: Ok(()),
                pulled: Ok(pulled),
            },
            endpoints,
        )
    }

    struct Says(Option<bool>, Vec<String>);

    impl Confirm for Says {
        fn confirm(&mut self, question: &str) -> Option<bool> {
            self.1.push(question.to_string());
            self.0
        }
    }

    const REGISTRY: &str = "{\n  \"profiles\": {\n    \"worker\": { \"models\": [ { \"id\": \"qwen\", \"n_ctx\": 8192 } ] }\n  }\n}\n";

    /// Only the engines darkmux adds parse, and a wrong word names them.
    #[test]
    fn add_engine_takes_the_engine_words() {
        assert_eq!(parse_engine("flint").unwrap(), DetectedKind::Flint);
        assert_eq!(
            parse_engine(" Tensorfold ").unwrap(),
            DetectedKind::Tensorfold
        );
        assert_eq!(
            parse_engine("lmstudio").unwrap(),
            DetectedKind::Lmstudio,
            "refused later, naming why"
        );
        for bad in ["url", "rec", ""] {
            assert!(
                parse_engine(bad)
                    .unwrap_err()
                    .to_string()
                    .contains("`tensorfold` or `flint`"),
                "{bad}"
            );
        }
    }

    /// The report says when nothing is installed, when nothing found is
    /// usable, and names the command for an engine that can be added.
    #[test]
    fn the_report_says_when_nothing_is_usable_and_how_to_add_one() {
        assert!(detected_lines(&Detected::of(Vec::new()))[0].contains("none installed"));
        let unusable = judge_flint(
            FlintFacts {
                version: Ok("0.2.0".into()),
                runtime: Ok(()),
                pulled: Ok(Vec::new()),
            },
            Vec::new(),
        );
        let lines = detected_lines(&Detected::of(vec![unusable]));
        assert_eq!(
            lines[0],
            "engines darkmux starts servers for: none of these is usable."
        );
        assert!(
            lines[1].contains("rec's Flint 0.2.0: not usable: rec 0.2.0 is installed"),
            "{lines:?}"
        );
        let lines = detected_lines(&Detected::of(vec![flint(Vec::new())]));
        assert_eq!(lines[0], "engines darkmux starts servers for:");
        assert!(lines[1].contains("rec's Flint 0.1.0: usable (`flint-int4`), not configured; `darkmux init --add-engine flint`"), "{lines:?}");
        let lines = detected_lines(&Detected::of(vec![flint(vec!["fl".into()])]));
        assert!(
            lines[1].ends_with("usable (`flint-int4`), endpoint `fl`"),
            "{lines:?}"
        );
    }

    /// The add writes only after a yes: no terminal, a no, or a dry run
    /// leaves the file as it was, and a yes writes the planned registry.
    #[test]
    fn the_add_writes_only_after_a_yes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("profiles.json");
        std::fs::write(&path, REGISTRY).unwrap();
        let found = Detected::of(vec![flint(Vec::new())]);
        let req = AddRequest {
            engine: DetectedKind::Flint,
            model: None,
            n_ctx: None,
        };
        for (answer, dry_run, why) in [
            (None, false, "no terminal"),
            (Some(false), false, "not confirmed"),
            (Some(true), true, "a dry run"),
        ] {
            let mut says = Says(answer, Vec::new());
            match add_engine(&req, &found, &path, REGISTRY, dry_run, &mut says) {
                AddOutcome::NotWritten { why: w, .. } => assert!(w.contains(why), "{w}"),
                other => panic!("{why}: {other:?}"),
            }
            assert_eq!(
                std::fs::read_to_string(&path).unwrap(),
                REGISTRY,
                "{why}: untouched"
            );
            assert_eq!(says.1.is_empty(), dry_run, "a dry run asks nobody");
        }
        let mut yes = Says(Some(true), Vec::new());
        let added = add_engine(&req, &found, &path, REGISTRY, false, &mut yes);
        assert!(
            yes.1[0].contains("add endpoint `flint`") && yes.1[0].ends_with("profiles.json?"),
            "{:?}",
            yes.1
        );
        let AddOutcome::Added(planned) = added else {
            panic!("{added:?}")
        };
        assert_eq!(std::fs::read_to_string(&path).unwrap(), planned.text);
        assert!(add_line(&AddOutcome::Added(planned), &path)
            .starts_with("add engine: added endpoint `flint`"));
    }

    /// A refused add says why and writes nothing.
    #[test]
    fn a_refused_add_says_why_and_writes_nothing() {
        let found = Detected::of(vec![flint(vec!["fl".into()])]);
        let req = AddRequest {
            engine: DetectedKind::Flint,
            model: None,
            n_ctx: None,
        };
        let mut says = Says(Some(true), Vec::new());
        let out = add_engine(
            &req,
            &found,
            Path::new("/nowhere/profiles.json"),
            REGISTRY,
            false,
            &mut says,
        );
        assert!(
            matches!(&out, AddOutcome::Refused(m) if m.contains("configured already")),
            "{out:?}"
        );
        assert!(says.1.is_empty(), "nothing to confirm");
    }

    /// The y/N prompt: `y` or `yes` confirms, anything else declines, and
    /// nobody to ask is no answer.
    #[test]
    fn the_prompt_confirms_only_on_yes() {
        let ask = |interactive: bool, line: &str| {
            let mut out = Vec::new();
            ask_yes_no(interactive, "add?", &mut line.as_bytes(), &mut out)
        };
        assert_eq!(ask(true, "y\n"), Some(true));
        assert_eq!(ask(true, "YES\n"), Some(true));
        assert_eq!(ask(true, "\n"), Some(false));
        assert_eq!(ask(true, "n\n"), Some(false));
        assert_eq!(ask(false, "y\n"), None);
    }
}
