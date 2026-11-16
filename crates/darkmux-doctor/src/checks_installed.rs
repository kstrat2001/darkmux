//! (#3569) The `installed engines` row: an engine darkmux starts servers for
//! (Tensorfold, rec's Flint) that is installed, usable and declared by no
//! endpoint, with the command that adds it. Informational: it passes. Read-only: detection asks each
//! installed binary its version and models
//! (`darkmux_profiles::engine::detect`), and starts, loads and pulls
//! nothing. LM Studio and the default engine have rows of their own.

use crate::{Check, Status};
use darkmux_profiles::engine::detect::{self, DetectedEngine, DetectedState};

const NAME: &str = "installed engines";

pub(crate) fn check_installed_engines() -> Check {
    match darkmux_profiles::profiles::load_registry_quiet(None) {
        Ok(loaded) => installed_engines_row(&detect::detect_servers(Some(&loaded.registry))),
        // Without the registry, what is configured is not known; the
        // `profile registry` row reports why it does not load.
        Err(_) => row(
            Status::Pass,
            "not checked: the profile registry does not load".into(),
            None,
        ),
    }
}

/// The row, its text safe on a terminal: a model id or reason an engine
/// supplied may hold a control character, which is shown escaped.
fn row(status: Status, message: String, hint: Option<String>) -> Check {
    let shown = |t: &str| darkmux_types::user_files::escape_text(t);
    Check {
        name: NAME.into(),
        status,
        message: shown(&message),
        hint: hint.as_deref().map(shown),
    }
}

/// The row: a warning naming each engine that could be added, else what
/// was found and why it needs nothing.
pub(crate) fn installed_engines_row(found: &[DetectedEngine]) -> Check {
    let addable: Vec<&DetectedEngine> = found.iter().filter(|d| d.can_add).collect();
    // Informational, never a warning: an engine installed and left out of
    // profiles.json may be left out on purpose, and a row the operator
    // cannot silence would only teach them to skip doctor's warnings.
    if !addable.is_empty() {
        return row(
            Status::Pass,
            addable.iter().map(|d| format!("{}; {} adds it", waiting(d), d.add_command())).collect::<Vec<_>>().join("; "),
            Some(format!(
                "{} adds it to profiles.json after you confirm (an endpoint and a profile running the model)",
                addable.iter().map(|d| d.add_command()).collect::<Vec<_>>().join(", or ")
            )),
        );
    }
    if found.is_empty() {
        return row(
            Status::Pass,
            "no engine darkmux starts servers for (Tensorfold, rec's Flint) is installed".into(),
            None,
        );
    }
    row(
        Status::Pass,
        found.iter().map(standing).collect::<Vec<_>>().join("; "),
        None,
    )
}

/// An addable engine: what it holds, and that nothing declares it.
fn waiting(d: &DetectedEngine) -> String {
    let models = d
        .models
        .iter()
        .map(|m| format!("`{}`", m.id))
        .collect::<Vec<_>>()
        .join(", ");
    let token = d
        .engine
        .startable()
        .map_or("", darkmux_types::config_enum::ConfigEnum::token);
    format!(
        "{} is installed with {models}, and no endpoint declares `\"managed\": \"{token}\"`",
        d.named()
    )
}

/// An engine that needs nothing: configured, or not usable and why.
fn standing(d: &DetectedEngine) -> String {
    let endpoints = || {
        d.endpoints
            .iter()
            .map(|e| format!("`{e}`"))
            .collect::<Vec<_>>()
            .join(", ")
    };
    match (d.configured, d.state) {
        (true, _) => format!("{} is configured (endpoint {})", d.named(), endpoints()),
        (false, DetectedState::Usable) => format!("{} is installed", d.named()),
        (false, _) => format!(
            "{} is installed and cannot be used: {}",
            d.named(),
            d.reason.as_deref().unwrap_or("no reason given")
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use darkmux_profiles::engine::detect::{
        judge_flint, judge_tensorfold, FlintFacts, TensorfoldFacts,
    };
    use darkmux_profiles::engine::flint::Pulled;
    use darkmux_profiles::engine::tensorfold::Version;

    fn flint(pulled: &[(&str, Option<u32>)], endpoints: &[&str]) -> DetectedEngine {
        let pulled = pulled
            .iter()
            .map(|(r, c)| Pulled {
                revision: r.to_string(),
                max_context: *c,
            })
            .collect();
        judge_flint(
            FlintFacts {
                version: Ok("0.1.0".into()),
                runtime: Ok(()),
                pulled: Ok(pulled),
            },
            endpoints.iter().map(|e| e.to_string()).collect(),
        )
    }

    fn tensorfold(version: Version) -> DetectedEngine {
        judge_tensorfold(
            TensorfoldFacts {
                version: Ok(version),
                models: Ok(vec!["TensorFold/Qwen3.8-27B-MLX-4bit".into()]),
            },
            Vec::new(),
        )
    }

    /// An installed, usable engine no endpoint declares is a warning naming
    /// it, its models, and the command that adds it after confirming.
    #[test]
    fn an_installed_engine_no_endpoint_declares_is_named_with_the_command_that_adds_it() {
        let c = installed_engines_row(&[flint(&[("flint-int4", Some(262_144))], &[])]);
        assert_eq!(
            c.status,
            Status::Pass,
            "informational, never a warning: {c:?}"
        );
        assert!(
            c.message
                .contains("`darkmux init --add-engine flint` adds it"),
            "{}",
            c.message
        );
        assert!(
            c.message
                .contains("rec's Flint 0.1.0 is installed with `flint-int4`"),
            "{}",
            c.message
        );
        assert!(
            c.message.contains(r#"`"managed": "flint"`"#),
            "{}",
            c.message
        );
        let hint = c.hint.unwrap();
        assert!(
            hint.contains("`darkmux init --add-engine flint`")
                && hint.contains("after you confirm"),
            "{hint}"
        );
    }

    /// The command names the flags darkmux cannot fill in: the model when
    /// the engine holds several, the context when it states none.
    #[test]
    fn the_add_command_names_what_darkmux_cannot_fill_in() {
        assert_eq!(
            DetectedEngine::add_command(&flint(&[("flint-int4", Some(8192))], &[])),
            "`darkmux init --add-engine flint`"
        );
        assert_eq!(
            DetectedEngine::add_command(&flint(
                &[("flint-int4", Some(8192)), ("flint-int8", Some(8192))],
                &[]
            )),
            "`darkmux init --add-engine flint --engine-model <id>`"
        );
        let tf = tensorfold(Version {
            major: 1,
            minor: 2,
            patch: 5,
        });
        assert_eq!(
            DetectedEngine::add_command(&tf),
            "`darkmux init --add-engine tensorfold --n-ctx <tokens>`"
        );
        let c = installed_engines_row(&[tf]);
        assert!(c
            .hint
            .unwrap()
            .contains("--add-engine tensorfold --n-ctx <tokens>"));
    }

    /// Nothing to add passes: no engine installed, one configured, or one
    /// that cannot be used (which says why, and offers no command).
    #[test]
    fn nothing_to_add_passes_and_says_what_was_found() {
        let none = installed_engines_row(&[]);
        assert_eq!(none.status, Status::Pass);
        assert!(
            none.message
                .contains("no engine darkmux starts servers for"),
            "{}",
            none.message
        );
        let configured = installed_engines_row(&[flint(&[("flint-int4", None)], &["fl"])]);
        assert_eq!(configured.status, Status::Pass);
        assert!(
            configured.message.contains("is configured (endpoint `fl`)"),
            "{}",
            configured.message
        );
        let old = installed_engines_row(&[tensorfold(Version {
            major: 1,
            minor: 3,
            patch: 0,
        })]);
        assert_eq!((old.status, old.hint.is_none()), (Status::Pass, true));
        assert!(
            old.message
                .contains("cannot be used: Tensorfold 1.3.0 is installed"),
            "{}",
            old.message
        );
    }

    /// (#3569 review) A model id an engine lists with a control character
    /// is shown escaped, never written to the terminal as is.
    #[test]
    fn an_engines_control_characters_are_shown_escaped() {
        let tf = judge_tensorfold(
            TensorfoldFacts {
                version: Ok(Version {
                    major: 1,
                    minor: 2,
                    patch: 5,
                }),
                models: Ok(vec!["evil\u{1b}]0;pwned\u{7}".into()]),
            },
            Vec::new(),
        );
        let c = installed_engines_row(&[tf]);
        assert!(!c.message.chars().any(char::is_control), "{:?}", c.message);
        assert!(
            c.message.contains("evil\\u{1b}]0;pwned\\u{7}"),
            "{}",
            c.message
        );
    }
}
