//! Lab fixture registry — thin name → path map persisted as JSON.
//!
//! Phase 2 of the lab-reproducibility cluster (#487, #489). Operators
//! register their fixture directories (anywhere on disk) by name +
//! version. The runtime consults this registry when resolving a
//! workload's `requires_fixture` (Phase 3). Each entry records the
//! fixture's content hash at register time so drift can be detected.
//!
//! ## Why thin
//!
//! The registry is purely a lookup table — names point at paths.
//! All metadata (what version, what it satisfies, what files matter)
//! lives in the fixture dir's own `.fixture.json` manifest. The
//! registry never duplicates that — it just records the resolution
//! pointer + an integrity-check hash.

use crate::lab::fixture::FixtureManifest;
use crate::lab::paths::DarkmuxPaths;
use crate::lab::sandbox_hash::hash_sandbox_dir;
use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// Canonical location of the lab registry file inside a resolved
/// darkmux home. Phase 4 CLI verbs read/write this path. Operators
/// who want a custom location can hand-edit + move; the resolver
/// always honors the canonical name under `{root}`.
pub(crate) fn default_registry_path(paths: &DarkmuxPaths) -> PathBuf {
    paths.root.join("lab-registry.json")
}

/// Sidecar lock-file path for a registry at `path` (`<path>.lock`).
/// `LabRegistry::with_locked` `flock(2)`s this rather than the registry
/// itself so the load step never confuses an empty lock-created file
/// with an absent registry (#496).
fn registry_lock_path(path: &Path) -> PathBuf {
    let mut s = path.as_os_str().to_owned();
    s.push(".lock");
    PathBuf::from(s)
}

/// One registered fixture's entry. Fields are public-API surface for
/// Phase 3 (resolver) + Phase 4 (CLI verbs) — they're populated now
/// even though no consumer reads them in Phase 2 (#489), hence the
/// dead-code lint.
#[allow(dead_code)]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct RegisteredFixture {
    /// Absolute path to the fixture directory.
    pub path: PathBuf,
    /// Content hash recorded at register time. Use this to detect
    /// drift via `dm lab doctor` (Phase 4) or pre-dispatch check.
    pub content_hash: String,
    /// When the hash was last recorded (ISO 8601 UTC).
    pub hashed_at: String,
    /// `version` field from the fixture's `.fixture.json` at register
    /// time. Used for satisfies-resolution + doctor warnings if the
    /// on-disk manifest version drifts.
    pub manifest_version: String,
    /// `satisfies` field from the fixture's `.fixture.json` at
    /// register time. Optional. Phase 3's resolver uses this to
    /// match against a workload's `requires_fixture`.
    #[serde(default)]
    pub satisfies: Option<String>,
}

/// The registry file's top-level shape. Phase 2 (#489) ships the
/// structure + serialization; Phase 3 wires the resolver; Phase 4
/// adds the CLI verbs (`dm lab register/list/doctor`). Many fields +
/// methods are dead-code-lint-suppressed until those phases consume
/// them.
#[allow(dead_code)]
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub(crate) struct LabRegistry {
    #[serde(default)]
    pub fixtures: BTreeMap<String, RegisteredFixture>,
}

#[allow(dead_code)]
impl LabRegistry {
    /// Load the registry from `path`. Returns an empty registry if
    /// the file doesn't exist (first-time-operator-friendly).
    pub(crate) fn load(path: &Path) -> Result<Self> {
        if !path.exists() {
            return Ok(Self::default());
        }
        let raw = std::fs::read_to_string(path)
            .with_context(|| format!("reading {}", path.display()))?;
        let reg: LabRegistry = serde_json::from_str(&raw).with_context(|| {
            format!(
                "parsing {} as lab registry — the file appears corrupt. \
                 New writes are atomic (#543), so this is most likely a legacy \
                 torn write or a bad hand-edit. Back it up and remove it to rebuild \
                 from `scripts/lab-init.sh` (+ re-`darkmux lab register` any custom fixtures).",
                path.display()
            )
        })?;
        Ok(reg)
    }

    /// Run a read-modify-write transaction against the registry at
    /// `path` under an exclusive cross-process `flock(2)` (#496).
    ///
    /// Loads the current registry, runs `f`, and persists the result —
    /// all while holding the lock — so two concurrent `dm lab register`
    /// invocations serialize through the lock instead of clobbering each
    /// other last-write-wins. Mirrors `darkmux-flow`'s `AuditFileSink`
    /// flock pattern.
    ///
    /// The lock is taken on a sidecar `<path>.lock` file (see
    /// [`registry_lock_path`]) rather than the registry itself, so the
    /// [`Self::load`] step never has to tell an empty lock-created file
    /// apart from an absent registry (load treats absent as
    /// empty-default).
    ///
    /// `f`'s result is returned untouched on success; if `f` returns
    /// `Err` the registry is NOT saved (a rejected register leaves the
    /// on-disk registry unchanged). POSIX-only — on non-unix the lock
    /// degrades to a plain load → mutate → save (single-operator
    /// assumption; cross-process safety is a unix-only guarantee).
    #[cfg(unix)]
    pub(crate) fn with_locked<F, T>(path: &Path, f: F) -> Result<T>
    where
        F: FnOnce(&mut LabRegistry) -> Result<T>,
    {
        use std::os::unix::io::AsRawFd;

        let lock_path = registry_lock_path(path);
        if let Some(parent) = lock_path.parent() {
            if !parent.exists() {
                std::fs::create_dir_all(parent)
                    .with_context(|| format!("creating {}", parent.display()))?;
            }
        }
        let lock_file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&lock_path)
            .with_context(|| format!("opening registry lock {}", lock_path.display()))?;

        // Acquire exclusive cross-process lock; released on guard drop.
        let fd = lock_file.as_raw_fd();
        if unsafe { libc::flock(fd, libc::LOCK_EX) } != 0 {
            return Err(anyhow!(
                "flock(LOCK_EX) failed on registry lock {}: {}",
                lock_path.display(),
                std::io::Error::last_os_error()
            ));
        }
        struct FlockGuard(std::os::unix::io::RawFd);
        impl Drop for FlockGuard {
            fn drop(&mut self) {
                unsafe { libc::flock(self.0, libc::LOCK_UN) };
            }
        }
        let _guard = FlockGuard(fd);

        let mut registry = Self::load(path)?;
        let out = f(&mut registry)?;
        registry.save(path)?;
        Ok(out)
    }

    /// Non-unix fallback for [`Self::with_locked`] — plain
    /// load → mutate → save with no cross-process lock.
    #[cfg(not(unix))]
    pub(crate) fn with_locked<F, T>(path: &Path, f: F) -> Result<T>
    where
        F: FnOnce(&mut LabRegistry) -> Result<T>,
    {
        let mut registry = Self::load(path)?;
        let out = f(&mut registry)?;
        registry.save(path)?;
        Ok(out)
    }

    /// Save the registry to `path`. Creates parent dir if needed.
    /// Writes pretty-printed JSON for operator-readability (the
    /// registry is small enough that pretty cost is negligible).
    ///
    /// Direct callers do NOT get cross-process safety — concurrent
    /// writers race last-write-wins. Mutating verbs go through
    /// [`Self::with_locked`] instead, which serializes the whole
    /// load → mutate → save cycle under `flock(2)` (#496).
    pub(crate) fn save(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            if !parent.exists() {
                std::fs::create_dir_all(parent)
                    .with_context(|| format!("creating {}", parent.display()))?;
            }
        }
        let json = serde_json::to_string_pretty(self)
            .context("serializing lab registry")?;

        // Write to a sibling temp file, then atomic rename onto `path`
        // (#543). `flock(2)` (#496) serializes writers, but a bare
        // `fs::write` truncates-in-place: a crash mid-write leaves a torn
        // registry, and an unlocked reader (`lab run`/`fixtures`/`doctor`
        // all `load()` without the lock) can observe the half-written
        // file. temp-write + rename closes both: a crash leaves only an
        // orphan temp, and `rename(2)` is atomic on POSIX so a reader
        // always sees the complete old or complete new file — no reader
        // lock required. The temp name is process- AND call-unique (#898):
        // an atomic counter alongside the pid. `save()` is `pub(crate)`, so
        // two threads racing it must not tear each other's temp before the
        // rename — a `{pid}`-only name would. `flock(2)` covers today's call
        // graph, but the unique temp name is the construction-level guarantee
        // (and matches `darkmux-profiles`' `write_atomic_json`, #901).
        use std::sync::atomic::{AtomicU64, Ordering};
        static SAVE_SEQ: AtomicU64 = AtomicU64::new(0);
        let tmp = path.with_extension(format!(
            "json.tmp.{}.{}",
            std::process::id(),
            SAVE_SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::write(&tmp, json)
            .with_context(|| format!("writing {}", tmp.display()))?;
        std::fs::rename(&tmp, path).with_context(|| {
            // Best-effort cleanup so a failed rename doesn't leave the
            // temp behind; ignore the cleanup error and surface the
            // original rename failure.
            let _ = std::fs::remove_file(&tmp);
            format!("atomically replacing {}", path.display())
        })?;
        Ok(())
    }

    /// Register a fixture by reading its `.fixture.json`, computing
    /// its content hash, and adding it to the registry.
    ///
    /// `name_override` — if `Some`, use this as the registry key
    /// instead of the manifest's `name` field. Useful when an
    /// operator wants to keep multiple variants of the same fixture
    /// distinguishable.
    ///
    /// `force` — if `true`, replace any existing entry with the same
    /// name. If `false` and an entry exists, return an error.
    ///
    /// Returns the **resolved registry key** (post-name-override)
    /// alongside the entry, so callers don't have to recover it via
    /// scan-the-map (which is fragile when multiple entries share
    /// the same content).
    ///
    /// **In-memory only.** Caller MUST call [`Self::save`] to persist
    /// the change to disk. Phase 4 CLI verbs are responsible for the
    /// save step.
    pub(crate) fn register(
        &mut self,
        fixture_dir: &Path,
        name_override: Option<String>,
        force: bool,
    ) -> Result<(String, &RegisteredFixture)> {
        let manifest = FixtureManifest::load_from_dir(fixture_dir)
            .with_context(|| format!("loading manifest from {}", fixture_dir.display()))?;
        let name = name_override.unwrap_or_else(|| manifest.name.clone());

        if !force && self.fixtures.contains_key(&name) {
            return Err(anyhow!(
                "fixture `{}` already registered; pass --force to replace",
                name
            ));
        }

        let abs_path = fixture_dir
            .canonicalize()
            .with_context(|| format!("canonicalizing {}", fixture_dir.display()))?;
        let content_hash = hash_sandbox_dir(&abs_path)
            .with_context(|| format!("hashing {}", abs_path.display()))?;
        let hashed_at = chrono_like_utc_now();

        let entry = RegisteredFixture {
            path: abs_path,
            content_hash,
            hashed_at,
            manifest_version: manifest.version,
            satisfies: manifest.satisfies,
        };
        self.fixtures.insert(name.clone(), entry);
        Ok((
            name.clone(),
            self.fixtures.get(&name).expect("just inserted"),
        ))
    }

    /// Remove a fixture from the registry. Does NOT delete the
    /// underlying dir (operator-sovereignty: register is a pointer,
    /// unregister forgets the pointer, the dir stays untouched).
    ///
    /// **In-memory only.** Caller MUST call [`Self::save`] to persist
    /// the change to disk.
    pub(crate) fn unregister(&mut self, name: &str) -> Result<RegisteredFixture> {
        self.fixtures
            .remove(name)
            .ok_or_else(|| anyhow!("fixture `{}` not registered", name))
    }

    /// Look up a registered fixture by name. Returns `None` if not
    /// present.
    pub fn get(&self, name: &str) -> Option<&RegisteredFixture> {
        self.fixtures.get(name)
    }

    /// Find the first registered fixture that satisfies the given
    /// requirement (Phase 3's resolver entry point). Requirement
    /// format: `<definition-name>@<version>`, matched LITERALLY (exact
    /// `satisfies` string equality). Semver range operators (`>=`/`^`/…)
    /// are NOT supported here — the caller (`resolve_source_sandbox`)
    /// rejects them loudly before this point; full semver is tracked in
    /// #496.
    ///
    /// Returns `(name, fixture)` of the first match in sorted-by-name
    /// order. Phase 3 may add disambiguation if multiple match.
    pub(crate) fn find_satisfying(&self, requirement: &str) -> Option<(&String, &RegisteredFixture)> {
        self.fixtures
            .iter()
            .find(|(_, f)| f.satisfies.as_deref() == Some(requirement))
    }
}

/// ISO 8601 UTC timestamp without pulling in `chrono`. Format
/// matches the audit-log convention used elsewhere in the codebase
/// (e.g. flow.rs records).
fn chrono_like_utc_now() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    // Convert epoch seconds to YYYY-MM-DDTHH:MM:SSZ without chrono.
    // Operator-readability matters; precision beyond seconds is not
    // load-bearing for the registry's purpose.
    let (year, month, day, h, m, s) = epoch_to_utc_components(secs);
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z",
        year, month, day, h, m, s
    )
}

/// Decompose epoch seconds into (year, month, day, hour, minute, second).
/// Self-contained; no dep. Standard Gregorian calendar, UTC.
/// `total_days` is `u64` so this stays correct past year 2106 (where
/// `u32 days` overflows). Pre-1.0 we wouldn't care, but a silent
/// wrong-result at year-2106 is harder to debug than the trivial fix.
fn epoch_to_utc_components(secs: u64) -> (u32, u32, u32, u32, u32, u32) {
    let s = (secs % 60) as u32;
    let m = ((secs / 60) % 60) as u32;
    let h = ((secs / 3600) % 24) as u32;
    let total_days: u64 = secs / 86400;

    // Days since 1970-01-01.
    let (year, month, day) = days_to_civil_date(total_days);
    (year, month, day, h, m, s)
}

/// Howard Hinnant's days_from_civil inverse — convert days since
/// 1970-01-01 to (year, month, day). Public domain algorithm.
fn days_to_civil_date(days: u64) -> (u32, u32, u32) {
    let z = days as i64 + 719468;
    let era = if z >= 0 { z / 146097 } else { (z - 146096) / 146097 };
    let doe = (z - era * 146097) as u64; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365; // [0, 399]
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32; // [1, 31]
    let m = (if mp < 10 { mp + 3 } else { mp - 9 }) as u32; // [1, 12]
    let yr = (y + if m <= 2 { 1 } else { 0 }) as u32;
    (yr, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn write_fixture(dir: &Path, name: &str, satisfies: Option<&str>) {
        let manifest = if let Some(s) = satisfies {
            format!(r#"{{"name": "{}", "satisfies": "{}"}}"#, name, s)
        } else {
            format!(r#"{{"name": "{}"}}"#, name)
        };
        std::fs::write(dir.join(".fixture.json"), manifest).unwrap();
        // Add at least one file so the hash isn't empty (still
        // works either way, but more realistic).
        std::fs::write(dir.join("placeholder.txt"), name).unwrap();
    }

    #[test]
    fn empty_registry_loads_from_missing_file() {
        let tmp = TempDir::new().unwrap();
        let reg_path = tmp.path().join("nonexistent.json");
        let reg = LabRegistry::load(&reg_path).unwrap();
        assert!(reg.fixtures.is_empty());
    }

    #[test]
    fn save_then_load_roundtrips() {
        let tmp = TempDir::new().unwrap();
        let fixture_dir = tmp.path().join("fx");
        std::fs::create_dir_all(&fixture_dir).unwrap();
        write_fixture(&fixture_dir, "demo", Some("tiny-python-suite@1.0"));

        let reg_path = tmp.path().join("registry.json");
        let mut reg = LabRegistry::default();
        reg.register(&fixture_dir, None, false).unwrap();
        reg.save(&reg_path).unwrap();

        let loaded = LabRegistry::load(&reg_path).unwrap();
        assert!(loaded.fixtures.contains_key("demo"));
        assert_eq!(
            loaded.fixtures["demo"].satisfies.as_deref(),
            Some("tiny-python-suite@1.0")
        );
    }

    #[test]
    fn register_computes_hash_at_register_time() {
        let tmp = TempDir::new().unwrap();
        let fixture_dir = tmp.path().join("fx");
        std::fs::create_dir_all(&fixture_dir).unwrap();
        write_fixture(&fixture_dir, "demo", None);

        let mut reg = LabRegistry::default();
        let (name, entry) = reg.register(&fixture_dir, None, false).unwrap();
        assert_eq!(name, "demo");
        assert!(entry.content_hash.starts_with("blake3:"));
        assert!(!entry.hashed_at.is_empty());
    }

    /// Returned (name, entry) reflects the override when supplied.
    #[test]
    fn register_returns_resolved_name_with_override() {
        let tmp = TempDir::new().unwrap();
        let fixture_dir = tmp.path().join("fx");
        std::fs::create_dir_all(&fixture_dir).unwrap();
        write_fixture(&fixture_dir, "from-manifest", None);
        let mut reg = LabRegistry::default();
        let (name, _) = reg
            .register(&fixture_dir, Some("override-name".to_string()), false)
            .unwrap();
        assert_eq!(name, "override-name");
    }

    #[test]
    fn register_rejects_duplicate_without_force() {
        let tmp = TempDir::new().unwrap();
        let fixture_dir = tmp.path().join("fx");
        std::fs::create_dir_all(&fixture_dir).unwrap();
        write_fixture(&fixture_dir, "demo", None);

        let mut reg = LabRegistry::default();
        reg.register(&fixture_dir, None, false).unwrap();
        let err = reg.register(&fixture_dir, None, false).unwrap_err();
        assert!(err.to_string().contains("already registered"), "got: {err}");
    }

    #[test]
    fn register_replaces_with_force() {
        let tmp = TempDir::new().unwrap();
        let fixture_dir = tmp.path().join("fx");
        std::fs::create_dir_all(&fixture_dir).unwrap();
        write_fixture(&fixture_dir, "demo", None);

        let mut reg = LabRegistry::default();
        reg.register(&fixture_dir, None, false).unwrap();
        let first_hash = reg.fixtures["demo"].content_hash.clone();
        // Mutate the fixture so the second register produces a
        // different hash.
        std::fs::write(fixture_dir.join("new.txt"), "added").unwrap();
        reg.register(&fixture_dir, None, true).unwrap();
        let second_hash = reg.fixtures["demo"].content_hash.clone();
        assert_ne!(first_hash, second_hash);
    }

    #[test]
    fn name_override_uses_supplied_name() {
        let tmp = TempDir::new().unwrap();
        let fixture_dir = tmp.path().join("fx");
        std::fs::create_dir_all(&fixture_dir).unwrap();
        write_fixture(&fixture_dir, "demo", None);

        let mut reg = LabRegistry::default();
        reg.register(&fixture_dir, Some("custom-name".to_string()), false)
            .unwrap();
        assert!(reg.fixtures.contains_key("custom-name"));
        assert!(!reg.fixtures.contains_key("demo"));
    }

    #[test]
    fn unregister_removes_entry_but_not_dir() {
        let tmp = TempDir::new().unwrap();
        let fixture_dir = tmp.path().join("fx");
        std::fs::create_dir_all(&fixture_dir).unwrap();
        write_fixture(&fixture_dir, "demo", None);

        let mut reg = LabRegistry::default();
        reg.register(&fixture_dir, None, false).unwrap();
        let removed = reg.unregister("demo").unwrap();
        assert!(!reg.fixtures.contains_key("demo"));
        // The dir + manifest are STILL on disk (operator-sovereignty).
        assert!(removed.path.exists());
        assert!(removed.path.join(".fixture.json").exists());
    }

    #[test]
    fn unregister_missing_name_errors() {
        let mut reg = LabRegistry::default();
        let err = reg.unregister("never-registered").unwrap_err();
        assert!(err.to_string().contains("not registered"), "got: {err}");
    }

    #[test]
    fn find_satisfying_matches_first_in_sorted_order() {
        let tmp = TempDir::new().unwrap();
        let mut reg = LabRegistry::default();
        for name in &["zeta", "alpha", "middle"] {
            let dir = tmp.path().join(name);
            std::fs::create_dir_all(&dir).unwrap();
            write_fixture(&dir, name, Some("tiny-python-suite@1.0"));
            reg.register(&dir, None, false).unwrap();
        }
        // BTreeMap iteration is sorted; alpha should come first.
        let (matched, _) = reg.find_satisfying("tiny-python-suite@1.0").unwrap();
        assert_eq!(matched, "alpha");
    }

    #[test]
    fn find_satisfying_returns_none_when_no_match() {
        let tmp = TempDir::new().unwrap();
        let fixture_dir = tmp.path().join("fx");
        std::fs::create_dir_all(&fixture_dir).unwrap();
        write_fixture(&fixture_dir, "demo", Some("known@1.0"));
        let mut reg = LabRegistry::default();
        reg.register(&fixture_dir, None, false).unwrap();
        assert!(reg.find_satisfying("other@1.0").is_none());
    }

    #[test]
    fn epoch_to_utc_components_known_dates() {
        // 1970-01-01 00:00:00 UTC = epoch 0 (the canonical anchor).
        let (y, mo, d, h, mi, s) = epoch_to_utc_components(0);
        assert_eq!((y, mo, d, h, mi, s), (1970, 1, 1, 0, 0, 0));
        // 2000-01-01 00:00:00 UTC = epoch 946684800 (Y2K anchor).
        let (y, mo, d, h, mi, s) = epoch_to_utc_components(946684800);
        assert_eq!((y, mo, d, h, mi, s), (2000, 1, 1, 0, 0, 0));
        // 2024-02-29 00:00:00 UTC = epoch 1709164800 (leap day; tests
        // Hinnant's calendar handling around leap years).
        let (y, mo, d, h, mi, s) = epoch_to_utc_components(1709164800);
        assert_eq!((y, mo, d, h, mi, s), (2024, 2, 29, 0, 0, 0));
        // Time-of-day components: 2024-02-29 13:45:07 UTC =
        // 1709164800 + 13*3600 + 45*60 + 7 = 1709164800 + 49507.
        let (y, mo, d, h, mi, s) = epoch_to_utc_components(1709164800 + 49507);
        assert_eq!((y, mo, d, h, mi, s), (2024, 2, 29, 13, 45, 7));
    }

    #[test]
    fn save_creates_parent_dir() {
        let tmp = TempDir::new().unwrap();
        let reg_path = tmp.path().join("nested/deep/registry.json");
        let reg = LabRegistry::default();
        reg.save(&reg_path).unwrap();
        assert!(reg_path.exists());
    }

    /// #543: `save()` writes via a sibling temp + atomic rename. Assert
    /// (a) overwriting an existing registry leaves it fully parseable
    /// (never torn) and (b) no `*.json.tmp.*` sibling is left behind.
    #[test]
    fn save_atomic_overwrite_leaves_registry_intact_and_no_temp() {
        let tmp = TempDir::new().unwrap();
        let reg_path = tmp.path().join("lab-registry.json");

        // First save (creates the file), then overwrite with a second
        // save — the overwrite path is where a bare truncating write
        // could tear the file.
        LabRegistry::default().save(&reg_path).unwrap();
        let fixture_dir = tmp.path().join("fix");
        std::fs::create_dir_all(&fixture_dir).unwrap();
        write_fixture(&fixture_dir, "f1", None);
        let mut reg = LabRegistry::load(&reg_path).unwrap();
        reg.register(&fixture_dir, None, false).unwrap();
        reg.save(&reg_path).unwrap();

        // (a) The on-disk file is always a complete, parseable registry.
        let reloaded = LabRegistry::load(&reg_path).unwrap();
        assert!(reloaded.get("f1").is_some(), "registry survived overwrite");

        // (b) No temp file leaked into the directory.
        let leftover: Vec<_> = std::fs::read_dir(tmp.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| {
                e.file_name()
                    .to_string_lossy()
                    .contains("json.tmp.")
            })
            .collect();
        assert!(
            leftover.is_empty(),
            "save() left a temp file behind: {leftover:?}"
        );
    }

    /// (#898) `save()` is `pub(crate)`; two threads racing it must not tear
    /// each other's temp before the rename. The temp name is now process-
    /// AND call-unique, so concurrent saves to the same path never collide.
    /// Guards against a `{pid}`-only regression (which reliably collides
    /// across these rounds).
    #[test]
    fn save_concurrent_writers_to_same_path_dont_collide() {
        let dir = TempDir::new().unwrap();
        let reg_path = std::sync::Arc::new(dir.path().join("lab-registry.json"));
        for _ in 0..8 {
            let handles: Vec<_> = (0..16)
                .map(|_| {
                    let p = std::sync::Arc::clone(&reg_path);
                    std::thread::spawn(move || LabRegistry::default().save(&p))
                })
                .collect();
            for h in handles {
                h.join().unwrap().expect("each concurrent save succeeds");
            }
            // Always a complete, parseable registry — never torn.
            LabRegistry::load(&reg_path).expect("registry parseable after concurrent saves");
            // No temp leaked.
            let leftover: Vec<_> = std::fs::read_dir(dir.path())
                .unwrap()
                .filter_map(|e| e.ok())
                .filter(|e| e.file_name().to_string_lossy().contains("json.tmp."))
                .collect();
            assert!(leftover.is_empty(), "orphan temp after concurrent saves: {leftover:?}");
        }
    }

    /// (#496) `with_locked` persists a successful mutation and creates
    /// the registry file (and its sidecar lock) on first use.
    #[test]
    fn with_locked_persists_mutation() {
        let tmp = TempDir::new().unwrap();
        let fixture_dir = tmp.path().join("fx");
        std::fs::create_dir_all(&fixture_dir).unwrap();
        write_fixture(&fixture_dir, "demo", None);
        let reg_path = tmp.path().join("registry.json");

        let name = LabRegistry::with_locked(&reg_path, |reg| {
            let (name, _) = reg.register(&fixture_dir, None, false)?;
            Ok(name)
        })
        .unwrap();
        assert_eq!(name, "demo");

        let loaded = LabRegistry::load(&reg_path).unwrap();
        assert!(loaded.fixtures.contains_key("demo"));
        assert!(registry_lock_path(&reg_path).exists());
    }

    /// (#496) A closure that returns `Err` leaves the on-disk registry
    /// unchanged — a rejected register doesn't clobber prior state.
    #[test]
    fn with_locked_does_not_save_on_closure_error() {
        let tmp = TempDir::new().unwrap();
        let fixture_dir = tmp.path().join("fx");
        std::fs::create_dir_all(&fixture_dir).unwrap();
        write_fixture(&fixture_dir, "demo", None);
        let reg_path = tmp.path().join("registry.json");

        // First register succeeds.
        LabRegistry::with_locked(&reg_path, |reg| {
            reg.register(&fixture_dir, None, false).map(|_| ())
        })
        .unwrap();
        // Second (duplicate, no force) fails inside the closure.
        let err = LabRegistry::with_locked(&reg_path, |reg| {
            reg.register(&fixture_dir, None, false).map(|_| ())
        })
        .unwrap_err();
        assert!(err.to_string().contains("already registered"), "got: {err}");

        // Registry still has exactly the one entry from the first call.
        let loaded = LabRegistry::load(&reg_path).unwrap();
        assert_eq!(loaded.fixtures.len(), 1);
    }

    /// (#496) Concurrent `with_locked` transactions from multiple
    /// threads must not lose entries to a last-write-wins race — each
    /// thread opens its own fd on the sidecar lock, so `flock(LOCK_EX)`
    /// serializes the load→modify→save cycles. Mirrors the process-level
    /// `tests/lab_concurrent_register_no_lost_writes.rs`.
    #[cfg(unix)]
    #[test]
    fn with_locked_concurrent_register_keeps_every_entry() {
        const N: usize = 12;
        let tmp = TempDir::new().unwrap();
        let fixture_dir = tmp.path().join("fx");
        std::fs::create_dir_all(&fixture_dir).unwrap();
        write_fixture(&fixture_dir, "shared", None);
        let reg_path = tmp.path().join("registry.json");

        let mut handles = Vec::with_capacity(N);
        for i in 0..N {
            let reg_path = reg_path.clone();
            let fixture_dir = fixture_dir.clone();
            handles.push(std::thread::spawn(move || {
                LabRegistry::with_locked(&reg_path, |reg| {
                    reg.register(&fixture_dir, Some(format!("fix-{i:02}")), false)
                        .map(|_| ())
                })
                .expect("locked register");
            }));
        }
        for h in handles {
            h.join().expect("join register thread");
        }

        let loaded = LabRegistry::load(&reg_path).unwrap();
        let missing: Vec<String> = (0..N)
            .map(|i| format!("fix-{i:02}"))
            .filter(|k| !loaded.fixtures.contains_key(k))
            .collect();
        assert!(
            missing.is_empty(),
            "concurrent register lost entries: {missing:?}; have {} of {N}",
            loaded.fixtures.len()
        );
    }
}
