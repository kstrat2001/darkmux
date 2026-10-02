//! darkmux-profiles — profile registry + LMStudio state helpers.
//!
//! Extracted from the binary in #463 (PR2). Holds the profile loader/lookup
//! (`profiles`), the `darkmux:` ownership contract (`ownership`: the
//! namespace helpers and the managed-resident eject), and the `lms` CLI
//! wrapper. `gestalt_host` (#1274 packet 2b) adds the gestalt port adapters
//! (`LmsHost`/`MacProbe`/`ArchFactsReader`), which pull in `darkmux-gestalt`
//! for the port traits — no cycle (gestalt depends only on darkmux-types).
//! `model_ledger` (#1286) composes those adapters into the potential-vs-
//! current memory ledger — ONE implementation consumed by both the
//! `darkmux machine resources` CLI verb and the serve daemon's `/machine/resources`.
//!
//! (2.0, #1405: the `runtime` module — the legacy `openclaw` shell-out
//! runtime's config-file patcher — was removed along with that runtime.)

pub mod envelope;
pub mod gestalt_host;
pub mod lms;
pub mod model_ledger;
pub mod ownership;
pub mod profiles;

/// (#2902 step 5) THE preflight for an entry point that starts work: every
/// registered `config.json` enum setting its scope consumes and `config.json`'s
/// unknown keys (`darkmux_types::config_enum::preflight`), plus, for a scope
/// that dispatches, the profile registry: every endpoint budget (an
/// unregistered `policy`, `config_enum::bad_endpoint_budget_policies`, and
/// `limits` that cannot be used as written,
/// `config_enum::invalid_endpoint_limits`) and every key its schema does not
/// know (`darkmux_types::user_files`). `profiles.json` is a file
/// `darkmux-types` cannot locate on its own, so this crate (the registry's
/// loader) adds that pass. Every endpoint is checked, not only the ones this
/// run would call: bad config is bad config (#2947). A registry that cannot
/// be loaded adds nothing here; the entry point reports that itself when it
/// resolves a profile.
pub fn preflight(
    scope: darkmux_types::config_enum::Scope,
) -> Result<(), darkmux_types::config_enum::PreflightRefusal> {
    preflight_with(scope, None)
}

/// [`preflight`] against the registry the command itself uses:
/// `profiles_file` is its `--profiles-file` (or equivalent), `None` for the
/// default search (`DARKMUX_PROFILES`, then the default locations). A
/// registry that cannot be loaded adds nothing here.
pub fn preflight_with(
    scope: darkmux_types::config_enum::Scope,
    profiles_file: Option<&str>,
) -> Result<(), darkmux_types::config_enum::PreflightRefusal> {
    use darkmux_types::config_enum::{self, PreflightRefusal};
    use darkmux_types::user_files::UserFileKind;
    let mut refusal = config_enum::preflight(scope).err().unwrap_or_else(|| PreflightRefusal::none(scope));
    if UserFileKind::Profiles.scopes().contains(&scope) {
        if let Ok(loaded) = profiles::load_registry_quiet(profiles_file) {
            refusal.bad.extend(config_enum::bad_endpoint_budget_policies(&loaded.registry));
            refusal.invalid.extend(config_enum::invalid_endpoint_limits(&loaded.registry));
            refusal.files.extend(profiles::user_file_problem(&loaded.path));
        }
    }
    refusal.into_result()
}

#[cfg(test)]
mod user_file_tests {
    use darkmux_types::config_enum::Scope;
    use darkmux_types::user_files::{open_objects, Problem};

    const CLEAN: &str = r#"{"profiles": {"p": {"models": [{"id": "m", "n_ctx": 4096}]}},
        "endpoints": {"azure": {"url": "https://h.example/v1", "limits": {"policy": "warn", "window": {"period": "1d", "tokens": 5}}}}}"#;

    fn write(text: &str) -> (tempfile::TempDir, String) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("profiles.json");
        std::fs::write(&path, text).unwrap();
        (dir, path.to_string_lossy().into_owned())
    }

    fn with_key(pointer: &str, key: &str) -> String {
        let mut doc: serde_json::Value = serde_json::from_str(CLEAN).unwrap();
        doc.pointer_mut(pointer).unwrap().as_object_mut().unwrap().insert(key.into(), serde_json::json!(1));
        doc.to_string()
    }

    /// `(document, the unknown key's path, the key the refusal must suggest)`.
    fn probes() -> Vec<(String, &'static str, &'static str)> {
        vec![
            (with_key("", "profile"), "profile", "profiles"),
            (with_key("/profiles/p/models/0", "n_ctxx"), "profiles.p.models[0].n_ctxx", "profiles.p.models[0].n_ctx"),
            (with_key("/endpoints/azure/limits/window", "tokns"), "endpoints.azure.limits.window.tokns", "endpoints.azure.limits.window.tokens"),
        ]
    }

    #[test]
    fn known_keys_load_and_pass_the_preflight() {
        let (_d, path) = write(CLEAN);
        crate::profiles::load_registry_quiet(Some(&path)).unwrap();
        assert_eq!(crate::preflight_with(Scope::Dispatch, Some(&path)), Ok(()));
    }

    #[test]
    fn an_unknown_key_is_refused_by_every_dispatching_preflight_naming_the_closest() {
        for (doc, key, closest) in probes() {
            let (_d, path) = write(&doc);
            // Loading never crashes: the registry still loads.
            crate::profiles::load_registry_quiet(Some(&path)).unwrap();
            for scope in [Scope::Dispatch, Scope::MissionLaunch, Scope::LabRun] {
                let refusal = crate::preflight_with(scope, Some(&path)).expect_err(&doc);
                let msg = refusal.to_string();
                assert!(msg.contains(&format!("unknown key `{key}`: did you mean `{closest}`?")), "{msg}");
                assert!(msg.contains(&path), "the refusal names the file: {msg}");
            }
            assert_eq!(crate::preflight_with(Scope::FleetSubmission, Some(&path)), Ok(()), "fleet submission reads no registry");
        }
    }

    /// A profile model's inline `endpoint` object is refused by every
    /// dispatching preflight, naming the exact rewrite (the `endpoints` id to
    /// move it to). The same document with the endpoint named by id passes.
    #[test]
    fn an_inline_endpoint_object_is_refused_naming_the_rewrite() {
        let inline = r#"{"profiles": {"p": {"models": [{"id": "gpt", "endpoint": {"url": "https://api.example/v1"}}]}}}"#;
        let (_d, path) = write(inline);
        for scope in [Scope::Dispatch, Scope::MissionLaunch, Scope::LabRun] {
            let msg = crate::preflight_with(scope, Some(&path)).expect_err(inline).to_string();
            assert!(msg.contains("profiles.p.models[0].endpoint"), "names the model's endpoint: {msg}");
            assert!(
                msg.contains("removed in 4.0")
                    && msg.contains("endpoints.\"api.example\"")
                    && msg.contains("\"endpoint\": \"api.example\""),
                "names the rewrite: {msg}"
            );
        }
        let named = r#"{"profiles": {"p": {"models": [{"id": "gpt", "endpoint": "api"}]}},
                        "endpoints": {"api": {"url": "https://api.example/v1"}}}"#;
        let (_d, path) = write(named);
        assert_eq!(crate::preflight_with(Scope::Dispatch, Some(&path)), Ok(()));
    }

    #[test]
    fn the_file_check_reports_every_unknown_key() {
        let (_d, path) = write(&with_key("/profiles/p", "modles"));
        let p = crate::profiles::user_file_problem(std::path::Path::new(&path)).unwrap();
        let Problem::Keys(keys) = p.problem else { panic!("{p:?}") };
        assert_eq!(keys.iter().map(|k| k.path.as_str()).collect::<Vec<_>>(), ["profiles.p.modles"]);
    }

    #[test]
    fn no_registry_object_accepts_keys_it_does_not_name() {
        assert_eq!(open_objects::<darkmux_types::ProfileRegistry>(), Vec::<String>::new());
    }
}

#[cfg(test)]
mod wrong_type_tests {
    /// A mistyped profile entry is quarantined (#1282), loudly, and the
    /// rest of the registry keeps working: the gate leaves it to that and
    /// refuses nothing over it.
    #[test]
    fn a_mistyped_profile_is_quarantined_not_refused() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("profiles.json");
        std::fs::write(
            &path,
            r#"{"profiles": {"good": {"models": [{"id": "m", "n_ctx": 1}]}, "bad": {"models": [{"id": "m", "n_ctx": "big"}]}}}"#,
        )
        .unwrap();
        let loaded = crate::profiles::load_registry_quiet(Some(path.to_str().unwrap())).unwrap();
        assert_eq!(loaded.registry.quarantined.len(), 1, "the bad entry is quarantined by name");
        assert!(loaded.registry.profiles.contains_key("good"));
        assert_eq!(crate::profiles::user_file_problem(&path), None);
        // A missing required key is quarantined the same way.
        std::fs::write(&path, r#"{"profiles": {"good": {"models": [{"id": "m", "n_ctx": 1}]}, "bad": {}}}"#).unwrap();
        assert_eq!(crate::profiles::load_registry_quiet(Some(path.to_str().unwrap())).unwrap().registry.quarantined.len(), 1);
        assert_eq!(crate::profiles::user_file_problem(&path), None);
        let with_typo = r#"{"profiles": {"bad": {"models": [{"id": "m", "n_ctx": "big", "n_ctxx": 1}]}}}"#;
        std::fs::write(&path, with_typo).unwrap();
        let msg = crate::profiles::user_file_problem(&path).unwrap().to_string();
        assert!(msg.contains("unknown key `profiles.bad.models[0].n_ctxx`") && !msg.contains("must be"), "{msg}");
    }
}

#[cfg(test)]
mod unloadable_registry_tests {
    use darkmux_types::user_files::Problem;

    /// A registry whose typed load fails on a bare-string `internal.utility`
    /// still has that shape named by the file check, with the object to
    /// write, beside every other refused shape in the same file.
    #[test]
    fn the_file_check_names_every_refusal_in_a_file_that_does_not_load() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("profiles.json");
        std::fs::write(
            &path,
            serde_json::json!({
                "profiles": {"p": {"models": [
                    {"id": "m", "n_ctx": 1, "role": "primary"},
                    {"id": "g", "endpoint": {"url": "https://api.example/v1"}}
                ]}},
                "internal": {"utility": "util-4b"}
            })
            .to_string(),
        )
        .unwrap();
        assert!(crate::profiles::load_registry_quiet(path.to_str()).is_err(), "the fixture must not load");
        let p = crate::profiles::user_file_problem(&path).unwrap();
        let Problem::Keys(keys) = &p.problem else { panic!("{p:?}") };
        let msgs: Vec<String> = keys.iter().map(ToString::to_string).collect();
        for (at, says) in [
            ("internal.utility", r#""utility": { "id": "util-4b", "n_ctx": "#),
            ("profiles.p.models[0].role", "unknown key"),
            ("profiles.p.models[1].endpoint", "inline endpoint object was removed"),
        ] {
            assert!(msgs.iter().any(|m| m.contains(at) && m.contains(says)), "{at}: {msgs:#?}");
        }
    }

    /// `registry_path` is the file `load_registry` would read: an explicit
    /// path even when absent, `None` only when nothing names a file.
    #[test]
    fn registry_path_is_the_file_the_loader_would_read() {
        assert_eq!(
            crate::profiles::registry_path(Some("/nonexistent/x/profiles.json")),
            Some(std::path::PathBuf::from("/nonexistent/x/profiles.json"))
        );
    }
}
