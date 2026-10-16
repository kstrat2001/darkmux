//! The LM Studio backend of the [`Engine`] facade: each method is one call
//! to today's `crate::lms` function or [`LmsHost`], unchanged.

use super::{Engine, ModelMeta};
use crate::model_ledger::ModelLedger;
use crate::gestalt_host::LmsHost;
use crate::lms;
use anyhow::{bail, Context, Result};
use darkmux_gestalt::ModelHost;
use darkmux_types::LoadedModel;
use std::process::Command;

/// LM Studio, reached through the `lms` CLI.
#[derive(Debug, Clone, Copy, Default)]
pub struct LmStudio;

impl Engine for LmStudio {
    fn loaded(&self) -> Result<Vec<LoadedModel>> {
        lms::list_loaded()
    }

    fn catalog(&self) -> Result<Vec<ModelMeta>> {
        lms::list_available()
    }

    fn unload(&self, identifier: &str) -> Result<()> {
        lms::unload(identifier)
    }

    fn host(&self) -> Option<Box<dyn ModelHost>> {
        Some(Box::new(LmsHost::new()))
    }

    fn resident_ids(&self) -> Result<Vec<String>> {
        // (#1863, named+tested #2534) This spawn bypasses this crate's
        // `run_bounded` chokepoint entirely (moved verbatim from dispatch's
        // `probe_loaded_model_list`, #316 stage 3.6), so it needs its own cwd
        // pin. A daemon/dispatch process started from a git worktree that
        // later gets deleted (routine in this project's own workflow)
        // otherwise inherits a cwd that no longer exists, and spawning any
        // child from it fails outright.
        // `crate::process::pin_cwd` is the one place this rule is written; see
        // its doc comment for why `/`.
        //
        // (#1939 sibling) Resolved through `darkmux_types::config_access::lms_bin()`
        // — the one `env(DARKMUX_LMS_BIN) > config.lms_bin > "lms"` precedence
        // home — rather than a literal, so an operator's override is honored on
        // this dispatch-path probe the same as everywhere else.
        let mut cmd = Command::new(darkmux_types::config_access::lms_bin());
        crate::process::pin_cwd(&mut cmd);
        let output = cmd
            .args(["ps", "--json"])
            .output()
            .context("running `lms ps --json` to enumerate loaded models")?;
        if !output.status.success() {
            bail!(
                "`lms ps --json` failed (exit {})",
                output.status.code().unwrap_or(-1)
            );
        }
        let body: serde_json::Value = serde_json::from_slice(&output.stdout)
            .context("parsing `lms ps --json` output")?;
        let mut ids = Vec::new();
        if let Some(arr) = body.as_array() {
            for entry in arr {
                // Both forms a profile id could match against.
                if let Some(key) = entry["modelKey"].as_str() {
                    ids.push(key.to_string());
                }
                if let Some(ident) = entry["identifier"].as_str() {
                    ids.push(ident.to_string());
                }
            }
        }
        Ok(ids)
    }

    fn version(&self) -> Option<String> {
        // (#1939) Resolved through the one `env > config.lms_bin > "lms"`
        // precedence home, not a literal: an operator who set `lms_bin` to a
        // non-default path otherwise gets a version that describes a
        // DIFFERENT binary than the one that served the run. `--version`
        // emits one plain line (`CLI commit: efce996`); bare `lms version`
        // emits a multi-line ANSI-art banner.
        let mut cmd = Command::new(darkmux_types::config_access::lms_bin());
        crate::process::pin_cwd(&mut cmd);
        cmd.arg("--version")
            .output()
            .ok()
            .filter(|o| o.status.success())
            .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
    }

    fn ledger(&self) -> ModelLedger {
        crate::model_ledger::gather_with_bin(&lms::lms_bin())
    }

    fn binary(&self) -> Option<(String, darkmux_types::config_access::Source)> {
        Some(darkmux_types::config_access::lms_bin_with_source())
    }
}

#[cfg(test)]
mod tests {
    /// A fake `lms` answering each verb the backend delegates: `ps --json`
    /// with one resident, `ls --json` with one catalog entry, `unload x`
    /// succeeding and `unload` of anything else failing as not resident, and
    /// `--version` printing one line.
    const FAKE_LMS: &str = r#"#!/bin/sh
case "$1" in
  ps) echo '[{"identifier":"darkmux:m","modelKey":"m","status":"idle","sizeBytes":1000000000,"contextLength":4096}]' ;;
  ls) echo '[{"modelKey":"m","displayName":"M","publisher":"p","sizeBytes":1000000000,"type":"llm"}]' ;;
  unload) [ "$2" = "x" ] && exit 0; echo "Model Not Found: $2" >&2; exit 1 ;;
  --version) echo 'CLI commit: abc1234' ;;
  *) exit 1 ;;
esac
"#;

    /// Each facade method reaches the `lms` verb it names and returns that
    /// verb's answer, and LM Studio declares a lifecycle.
    #[test]
    #[serial_test::serial]
    fn each_method_delegates_to_its_lms_verb() {
        let tmp = tempfile::tempdir().unwrap();
        let bin = tmp.path().join("lms");
        std::fs::write(&bin, FAKE_LMS).unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
        let prev = std::env::var_os("DARKMUX_LMS_BIN");
        // SAFETY: #[serial]; restored below.
        unsafe { std::env::set_var("DARKMUX_LMS_BIN", &bin) };

        let engine = crate::engine::local();
        let loaded = engine
            .loaded()
            .map(|l| l.into_iter().map(|m| m.identifier).collect::<Vec<_>>());
        let catalog = engine
            .catalog()
            .map(|c| c.into_iter().map(|m| m.model_key).collect::<Vec<_>>());
        let unloaded = engine.unload("x");
        let gone = engine.unload("y");
        let ids = engine.resident_ids();
        let version = engine.version();
        let ledger: Vec<String> = engine.ledger().models.into_iter().map(|m| m.identifier).collect();
        let binary = engine.binary();

        unsafe {
            match prev {
                Some(v) => std::env::set_var("DARKMUX_LMS_BIN", v),
                None => std::env::remove_var("DARKMUX_LMS_BIN"),
            }
        }
        assert_eq!(loaded.unwrap(), vec!["darkmux:m".to_string()]);
        assert_eq!(catalog.unwrap(), vec!["m".to_string()]);
        assert!(unloaded.is_ok(), "{unloaded:?}");
        assert!(
            crate::engine::is_not_resident(&gone.unwrap_err()),
            "an absent instance is the typed not-resident error"
        );
        assert!(engine.host().is_some(), "LM Studio has a load lifecycle");
        assert_eq!(ids.unwrap(), vec!["m".to_string(), "darkmux:m".to_string()], "model key, then identifier");
        assert_eq!(version.as_deref(), Some("CLI commit: abc1234\n"), "the version line, unparsed");
        assert_eq!(ledger, vec!["darkmux:m".to_string()], "the ledger reads the same `lms ps`");
        assert_eq!(
            binary,
            Some((bin.to_string_lossy().into_owned(), darkmux_types::config_access::Source::Env)),
            "the binary the engine is reached through, and that the env named it"
        );
    }
}
