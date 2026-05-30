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

    // The whole load → register → save cycle runs under an exclusive
    // `flock(2)` on the registry (#496) so concurrent `dm lab register`
    // invocations serialize instead of clobbering last-write-wins.
    //
    // `register` returns the resolved registry key + the inserted entry;
    // we clone the entry out of the locked closure (the post-review #498
    // fix — re-scanning the BTreeMap was fragile when multiple entries
    // shared content but had different keys).
    let (registered_name, entry) = LabRegistry::with_locked(&reg_path, |registry| {
        let (name, entry) = registry.register(path, name_override, force)?;
        Ok((name, entry.clone()))
    })
    .with_context(|| format!("registering into {}", reg_path.display()))?;

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

    // load → unregister → save under the registry flock (#496).
    let removed = LabRegistry::with_locked(&reg_path, |registry| registry.unregister(name))?;

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
