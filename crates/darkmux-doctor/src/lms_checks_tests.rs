//! (#3198) Characterization tests for the two doctor rows that read LM
//! Studio's state against the profile registry: `check_profile_loaded_match`
//! and `check_ram_headroom_load_projection`. Ahead of the engine facade
//! (docs/architecture/ENGINES.md, stage 0) each pins the row an operator sees,
//! with the registry (`DARKMUX_PROFILES`), `lms` (`DARKMUX_LMS_BIN`) and the
//! memory probes (`vm_stat`, `pagesize` on `PATH`) all faked. Nothing here
//! reaches LM Studio.

use super::*;
use std::ffi::OsString;
use std::path::{Path, PathBuf};

const PROFILES: &str = r#"{
    "profiles": {
        "p": {"models": [{"id": "big", "n_ctx": 8192}, {"id": "helper", "n_ctx": 4096}, {"id": "extra", "n_ctx": 4096}]},
        "q": {"models": [{"id": "other", "n_ctx": 8192}]}
    },
    "default_profile": "p"
}"#;

/// One test's fakes. `lms ps --json` prints `ps.json` and `lms ls --json`
/// prints `ls.json`; either one missing makes that call fail. `vm_stat`
/// reports `free_pages` free 16 KiB pages and fails when there is no
/// `vm_stat.txt`.
struct Fakes {
    dir: tempfile::TempDir,
    saved: Vec<(&'static str, Option<OsString>)>,
}

fn exe(path: &Path, body: &str) {
    std::fs::write(path, body).unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
}

impl Fakes {
    fn new(profiles: Option<&str>) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path().display();
        let bin = dir.path().join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        exe(
            &bin.join("lms"),
            &format!(
                "#!/bin/sh\ncase \"$1\" in\n  ps) f=\"{d}/ps.json\" ;;\n  ls) f=\"{d}/ls.json\" ;;\n  *) exit 1 ;;\nesac\n[ -f \"$f\" ] || exit 1\ncat \"$f\"\n"
            ),
        );
        exe(&bin.join("vm_stat"), &format!("#!/bin/sh\n[ -f \"{d}/vm_stat.txt\" ] || exit 1\ncat \"{d}/vm_stat.txt\"\n"));
        exe(&bin.join("pagesize"), "#!/bin/sh\necho 16384\n");
        let registry: PathBuf = dir.path().join("profiles.json");
        if let Some(text) = profiles {
            std::fs::write(&registry, text).unwrap();
        }
        let path = format!("{}:{}", bin.display(), std::env::var("PATH").unwrap_or_default());
        let vars: [(&'static str, OsString); 3] = [
            ("DARKMUX_PROFILES", registry.into_os_string()),
            ("DARKMUX_LMS_BIN", bin.join("lms").into_os_string()),
            ("PATH", path.into()),
        ];
        let saved = vars.iter().map(|(k, _)| (*k, std::env::var_os(k))).collect();
        // SAFETY: every test using `Fakes` is #[serial].
        unsafe {
            for (k, v) in &vars {
                std::env::set_var(k, v);
            }
        }
        Fakes { dir, saved }
    }

    fn loaded(self, ps: &str) -> Self {
        std::fs::write(self.dir.path().join("ps.json"), ps).unwrap();
        self
    }

    fn catalog(self, ls: &str) -> Self {
        std::fs::write(self.dir.path().join("ls.json"), ls).unwrap();
        self
    }

    /// `free_gb` GiB reclaimable, in 16 KiB pages.
    fn free_gb(self, free_gb: u64) -> Self {
        let pages = free_gb * 1024 * 1024 * 1024 / 16_384;
        std::fs::write(self.dir.path().join("vm_stat.txt"), format!("Pages free: {pages}.\nPages inactive: 0.\n")).unwrap();
        self
    }
}

impl Drop for Fakes {
    fn drop(&mut self) {
        // SAFETY: every test using `Fakes` is #[serial].
        unsafe {
            for (k, v) in &self.saved {
                match v {
                    Some(v) => std::env::set_var(k, v),
                    None => std::env::remove_var(k),
                }
            }
        }
    }
}

fn row(identifier: &str, model: &str) -> String {
    format!(r#"{{"identifier":"{identifier}","modelKey":"{model}","status":"idle","contextLength":8192}}"#)
}

fn ps(rows: &[(&str, &str)]) -> String {
    format!("[{}]", rows.iter().map(|(i, m)| row(i, m)).collect::<Vec<_>>().join(","))
}

// ─── check_profile_loaded_match ─────────────────────────────────────

#[serial_test::serial]
#[test]
fn profile_match_warns_when_the_registry_does_not_load() {
    let _f = Fakes::new(None).loaded(&ps(&[("big", "big")]));
    let c = check_profile_loaded_match();
    assert_eq!((c.name.as_str(), c.status), ("profile match", Status::Warn));
    assert!(c.message.contains("the profile registry did not load"), "{}", c.message);
}

#[serial_test::serial]
#[test]
fn profile_match_warns_when_lms_cannot_list_loads() {
    let _f = Fakes::new(Some(PROFILES));
    let c = check_profile_loaded_match();
    assert_eq!(c.status, Status::Warn);
    assert_eq!(c.message, "could not enumerate loaded models");
}

#[serial_test::serial]
#[test]
fn profile_match_warns_when_nothing_is_loaded() {
    let _f = Fakes::new(Some(PROFILES)).loaded("[]");
    let c = check_profile_loaded_match();
    assert_eq!(c.status, Status::Warn);
    assert_eq!(c.message, "no models loaded — nothing to match against");
}

#[serial_test::serial]
#[test]
fn profile_match_passes_naming_every_profile_whose_default_model_is_loaded() {
    // `big` under its namespaced identifier, `other` under its plain one.
    let _f = Fakes::new(Some(PROFILES)).loaded(&ps(&[("darkmux:big", "big"), ("other", "other")]));
    let c = check_profile_loaded_match();
    assert_eq!(c.status, Status::Pass, "{}", c.message);
    assert_eq!(c.message, "loaded state matches profile(s): p, q");
    assert!(c.hint.is_none());
}

#[serial_test::serial]
#[test]
fn profile_match_warns_when_only_a_non_default_model_is_loaded() {
    let _f = Fakes::new(Some(PROFILES)).loaded(&ps(&[("darkmux:helper", "helper")]));
    let c = check_profile_loaded_match();
    assert_eq!(c.status, Status::Warn, "a profile matches on its default model only: {}", c.message);
    assert_eq!(c.message, "loaded models don't match any profile");
    assert!(c.hint.as_deref().is_some_and(|h| h.contains("primary model id matches")), "{:?}", c.hint);
}

// ─── check_ram_headroom_load_projection ─────────────────────────────

const PROJECTION: &str = "RAM headroom (load projection)";

fn skipped(c: &Check) -> &str {
    assert_eq!((c.name.as_str(), c.status), (PROJECTION, Status::Pass), "{}", c.message);
    c.message.strip_prefix("(skipped: ").and_then(|m| m.strip_suffix(')')).unwrap_or_else(|| panic!("not a skip: {}", c.message))
}

#[serial_test::serial]
#[test]
fn projection_skips_when_the_registry_does_not_load() {
    let _f = Fakes::new(None).loaded(&ps(&[("big", "big")])).free_gb(64);
    let c = check_ram_headroom_load_projection();
    assert!(skipped(&c).starts_with("the profile registry did not load"), "{}", c.message);
}

#[serial_test::serial]
#[test]
fn projection_skips_when_lms_cannot_list_loads() {
    let _f = Fakes::new(Some(PROFILES)).free_gb(64);
    assert_eq!(skipped(&check_ram_headroom_load_projection()), "could not query lms");
}

#[serial_test::serial]
#[test]
fn projection_skips_when_nothing_is_loaded() {
    let _f = Fakes::new(Some(PROFILES)).loaded("[]").free_gb(64);
    assert_eq!(skipped(&check_ram_headroom_load_projection()), "no models loaded — nothing to project against");
}

#[serial_test::serial]
#[test]
fn projection_skips_when_no_profile_matches() {
    let _f = Fakes::new(Some(PROFILES)).loaded(&ps(&[("someone-elses", "someone-elses")])).free_gb(64);
    assert_eq!(skipped(&check_ram_headroom_load_projection()), "no profile matches loaded state");
}

#[serial_test::serial]
#[test]
fn projection_passes_when_the_active_profile_is_fully_resident_whichever_way_each_model_is_named() {
    // `big` by its plain identifier (under another model key), `helper` by its
    // model key under a custom identifier, `extra` by its namespaced
    // identifier: each counts as loaded.
    let _f = Fakes::new(Some(PROFILES))
        .loaded(&ps(&[("big", "vendor/big-gguf"), ("my-helper", "helper"), ("darkmux:extra", "x-key")]))
        .free_gb(64);
    let c = check_ram_headroom_load_projection();
    assert_eq!(c.status, Status::Pass, "{}", c.message);
    assert_eq!(c.message, "active profile `p` fully resident");
}

#[serial_test::serial]
#[test]
fn projection_counts_each_unloaded_model_by_its_catalog_size_or_says_unknown() {
    let _f = Fakes::new(Some(PROFILES))
        .loaded(&ps(&[("darkmux:big", "big")]))
        .catalog(r#"[{"modelKey":"helper","type":"llm","sizeBytes":5000000000}]"#)
        .free_gb(64);
    let c = check_ram_headroom_load_projection();
    assert_eq!(c.status, Status::Pass, "{}", c.message);
    assert!(c.message.contains("loading rest of profile `p` would consume ~5.0 GB (helper ~5.0 GB, extra (size unknown))"), "{}", c.message);
}

#[serial_test::serial]
#[test]
fn projection_reports_every_size_unknown_when_the_catalog_fails() {
    let _f = Fakes::new(Some(PROFILES)).loaded(&ps(&[("darkmux:big", "big")])).free_gb(64);
    let c = check_ram_headroom_load_projection();
    assert_eq!(c.status, Status::Pass, "a failed catalog is a partial picture, not a missing row: {}", c.message);
    assert!(c.message.contains("~0.0 GB (helper (size unknown), extra (size unknown))"), "{}", c.message);
}

#[serial_test::serial]
#[test]
fn projection_fails_when_the_rest_of_the_profile_would_not_fit() {
    let _f = Fakes::new(Some(PROFILES))
        .loaded(&ps(&[("darkmux:big", "big")]))
        .catalog(r#"[{"modelKey":"helper","type":"llm","sizeBytes":40000000000}]"#)
        .free_gb(16);
    let c = check_ram_headroom_load_projection();
    assert_eq!(c.status, Status::Fail, "{}", c.message);
    assert!(c.message.ends_with("— load would swap or OOM"), "{}", c.message);
}

#[serial_test::serial]
#[test]
fn projection_skips_when_free_memory_cannot_be_read() {
    let _f = Fakes::new(Some(PROFILES)).loaded(&ps(&[("darkmux:big", "big")]));
    assert_eq!(skipped(&check_ram_headroom_load_projection()), "could not read vm_stat (non-macOS?)");
}
