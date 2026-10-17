//! The local engine when `default_engine` names no usable engine (#316,
//! ENGINES.md section 9.1). Contract 9 forbids reading such a value as LM
//! Studio, and [`super::local`] cannot fail, so it returns this: every
//! question is answered with the refusal, which every entry point's preflight
//! and `darkmux doctor`'s `default engine` row also give. It is an error
//! carrier, not an engine, so it is held to no capability declaration: its
//! questions fail with the refusal rather than [`super::NotReported`].

use super::{Capabilities, CatalogFacts, Engine, Loading, ModelMeta, Residency};
use crate::model_ledger::ModelLedger;
use anyhow::{anyhow, Result};
use darkmux_gestalt::ModelHost;
use darkmux_types::LoadedModel;

/// `reason` is the operator line: where the value was set, what is wrong
/// with it and the valid values.
pub(crate) struct Unresolved {
    /// The setting's value, as written.
    pub(crate) name: String,
    pub(crate) reason: String,
}

impl Unresolved {
    fn refused<T>(&self) -> Result<T> {
        Err(anyhow!(
            "the default engine cannot be used: {}",
            self.reason
        ))
    }
}

impl Engine for Unresolved {
    fn capabilities(&self) -> Capabilities {
        Capabilities {
            residency: Residency::NotReported,
            loading: Loading::Unknown,
            unload: false,
            ownership_mark: false,
            catalog: CatalogFacts {
                size: false,
                context_limit: false,
                tool_training: false,
            },
        }
    }

    fn name(&self) -> String {
        self.name.clone()
    }

    fn usable(&self) -> bool {
        false
    }

    fn loaded(&self) -> Result<Vec<LoadedModel>> {
        self.refused()
    }

    fn catalog(&self) -> Result<Vec<ModelMeta>> {
        self.refused()
    }

    fn unload(&self, _identifier: &str) -> Result<()> {
        self.refused()
    }

    fn host(&self) -> Option<Box<dyn ModelHost>> {
        None
    }

    fn resident_ids(&self) -> Result<Vec<String>> {
        self.refused()
    }

    fn version(&self) -> Option<String> {
        None
    }

    fn ledger(&self) -> ModelLedger {
        crate::model_ledger::gather_residents_unknown(&format!(
            "the default engine cannot be used: {}",
            self.reason
        ))
    }

    fn binary(&self) -> Option<(String, darkmux_types::config_access::Source)> {
        None
    }
}
