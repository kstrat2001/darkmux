//! Canonical run-artifact directory names. Single source of truth so the
//! clone, the content hash, and the workspace-delta view can never drift
//! (the drift across 4 hand-rolled copies was the recurring root cause of
//! lab-run contamination).

/// (a) Run/build droppings: excluded from BOTH the COW clone AND the
/// content hash. The in-sandbox tests never need these.
/// NOTE: `.darkmux-agent` is DEFENSIVE — no darkmux code writes it (verified
/// by tree-wide grep); it's a leftover from the removed external-runtime
/// shell-out path (#1405) or a pre-#487 dropping. Excluding a never-present
/// name is a no-op, so it's safe to list.
pub(crate) const RUN_ARTIFACT_DIRS: &[&str] = &[
    ".darkmux-runtime",
    ".darkmux-agent",
    "coverage",
    ".coverage",
    "target",
    "__pycache__",
    ".git",
];

/// (b) Heavy-but-needed: the clone KEEPS it (in-sandbox tests need deps),
/// the hash DROPS it (it must never perturb content-equality).
pub(crate) const HASH_ONLY_EXCLUDES: &[&str] = &["node_modules"];
