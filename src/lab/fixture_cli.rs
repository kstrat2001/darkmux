//! CLI verb logic for `dm lab register / unregister / fixtures`.
//!
//! Phase 4 of the lab-reproducibility cluster (#487, #491). Thin
//! wrappers around `LabRegistry`'s in-memory API that load + save
//! the registry file as a unit. Verbs return operator-formatted
//! result strings; main.rs prints them.

use crate::lab::paths::{self, ResolveScope};
use crate::lab::registry::{default_registry_path, LabRegistry};
use anyhow::{Context, Result};
use std::path::Path;

/// `dm lab register <path>` — read `.fixture.json` from `path`,
/// compute its content hash, add to registry, persist.
pub fn cmd_register(
    path: &Path,
    name_override: Option<String>,
    force: bool,
) -> Result<String> {
    let paths = paths::resolve(ResolveScope::Auto);
    paths::ensure(&paths)?;
    let reg_path = default_registry_path(&paths);

    let mut registry = LabRegistry::load(&reg_path)
        .with_context(|| format!("loading {}", reg_path.display()))?;
    let entry = registry
        .register(path, name_override, force)?
        .clone();
    let registered_name = registry
        .fixtures
        .iter()
        .find(|(_, f)| f.content_hash == entry.content_hash && f.path == entry.path)
        .map(|(n, _)| n.clone())
        .unwrap_or_else(|| "<unknown>".to_string());
    registry.save(&reg_path)?;

    Ok(format!(
        "Registered fixture `{}`\n  path:   {}\n  hash:   {}\n  hashed: {}\n  version: {}{}",
        registered_name,
        entry.path.display(),
        entry.content_hash,
        entry.hashed_at,
        entry.manifest_version,
        entry
            .satisfies
            .as_ref()
            .map(|s| format!("\n  satisfies: {s}"))
            .unwrap_or_default(),
    ))
}

/// `dm lab unregister <name>` — remove pointer from registry.
/// NEVER touches the underlying dir (operator-sovereignty).
pub fn cmd_unregister(name: &str) -> Result<String> {
    let paths = paths::resolve(ResolveScope::Auto);
    paths::ensure(&paths)?;
    let reg_path = default_registry_path(&paths);

    let mut registry = LabRegistry::load(&reg_path)?;
    let removed = registry.unregister(name)?;
    registry.save(&reg_path)?;

    Ok(format!(
        "Unregistered `{name}` (was → {})\n  Note: the directory itself was NOT touched.",
        removed.path.display()
    ))
}

/// `dm lab fixtures` — show registered fixtures table.
pub fn cmd_list() -> Result<String> {
    let paths = paths::resolve(ResolveScope::Auto);
    let reg_path = default_registry_path(&paths);

    if !reg_path.exists() {
        return Ok(format!(
            "No registry at {}.\n  To get started:\n    `dm lab register <path-to-fixture>`",
            reg_path.display()
        ));
    }

    let registry = LabRegistry::load(&reg_path)?;
    if registry.fixtures.is_empty() {
        return Ok(format!(
            "Registry at {} has no fixtures registered.\n  Add one: `dm lab register <path-to-fixture>`",
            reg_path.display()
        ));
    }

    let mut out = format!(
        "Lab registry: {} ({} fixture{})\n",
        reg_path.display(),
        registry.fixtures.len(),
        if registry.fixtures.len() == 1 { "" } else { "s" }
    );
    for (name, entry) in &registry.fixtures {
        out.push_str(&format!(
            "\n  {name}\n    path:       {}\n    version:    {}\n    satisfies:  {}\n    hash:       {}\n    hashed_at:  {}\n",
            entry.path.display(),
            entry.manifest_version,
            entry.satisfies.as_deref().unwrap_or("(none)"),
            entry.content_hash,
            entry.hashed_at,
        ));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    // CLI-level integration tests live in tests/cli.rs (they spawn
    // the binary). Unit-test coverage for the underlying LabRegistry
    // operations lives in src/lab/registry.rs. This module is thin
    // wiring; the wiring is exercised via the CLI tests.
}
