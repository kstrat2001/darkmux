//! The LM Studio backend of the [`Engine`] facade: each method is one call
//! to today's `crate::lms` function or [`LmsHost`], unchanged.

use super::{Engine, ModelMeta};
use crate::gestalt_host::LmsHost;
use crate::lms;
use anyhow::Result;
use darkmux_gestalt::ModelHost;
use darkmux_types::LoadedModel;

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
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A fake `lms` answering each verb the backend delegates: `ps --json`
    /// with one resident, `ls --json` with one catalog entry, `unload x`
    /// succeeding and `unload` of anything else failing as not resident.
    const FAKE_LMS: &str = r#"#!/bin/sh
case "$1" in
  ps) echo '[{"identifier":"darkmux:m","modelKey":"m","status":"idle","sizeBytes":1000000000,"contextLength":4096}]' ;;
  ls) echo '[{"modelKey":"m","displayName":"M","publisher":"p","sizeBytes":1000000000,"type":"llm"}]' ;;
  unload) [ "$2" = "x" ] && exit 0; echo "Model Not Found: $2" >&2; exit 1 ;;
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
            lms::is_not_resident(&gone.unwrap_err()),
            "an absent instance is the typed not-resident error"
        );
        assert!(engine.host().is_some(), "LM Studio has a load lifecycle");
    }
}
