//! (#1939) Conformance: the model-host binary is never spawned by a
//! hardcoded `"lms"` literal — every spawn resolves the name through
//! `darkmux_types::config_access::lms_bin()` (directly, or via one of this
//! workspace's thin wrappers around it: `darkmux_profiles::lms::lms_bin()`,
//! `LmsHost::new()`'s `bin: lms_bin()`, `model_ledger::gather()`'s
//! `crate::lms::lms_bin()`), which resolves
//! `env(DARKMUX_LMS_BIN) > config.lms_bin > "lms"` (#661 Slice 4).
//!
//! `crates/darkmux-lab/src/lab/scores.rs` (the bench fingerprint's
//! `engine_version` probe) and `crates/darkmux-crew/src/dispatch_internal.rs`
//! (`probe_loaded_model_list`, on the live dispatch path) each spawned
//! `Command::new("lms")` directly — silently ignoring an operator's
//! `lms_bin` override, either producing a fingerprint/probe result from the
//! WRONG binary or nothing at all if bare `lms` isn't on `PATH`. That is the
//! bug #1939 fixes. This module is the scan that keeps a THIRD hardcoded
//! spawn from landing unnoticed.
//!
//! **What this is: a lint, not an enumeration.** Same shape and the same
//! honest limits as `pin_cwd_conformance.rs` (read that module's doc before
//! extending this one) — this walks every `.rs` file under the WORKSPACE's
//! production `src/` trees (broader than that module's 3-crate sweep,
//! since #1939 was found by a whole-tree grep, not a scoped one) and flags
//! the one literal shape this codebase's antipattern actually takes:
//! `Command::new("lms")`. It is a **source scan**, not an enumeration — see
//! #2572, which draws exactly this distinction for the sibling cwd-pin scan
//! and proposes the durable fix (a typed constructor that makes the
//! hardcoded form unrepresentable). That fix is not built here; this scan
//! is the interim guard, with the same ceiling #2572 already named.
//!
//! **What this scan cannot see:**
//!   - a binary name bound to a local variable first, then spawned via
//!     `Command::new(that_local)` — indistinguishable, by text alone, from
//!     a local bound to some unrelated string. The escape template already
//!     lives in this very crate (`model_ledger.rs`'s `bounded_stdout`,
//!     `Command::new(bin)` — legitimate there because `bin` is bound to
//!     `lms_bin()` a few lines up via `gather()`), which is exactly why a
//!     text scan cannot be trusted to rule this class out in general —
//!     only to catch the ONE spelling of the mistake this codebase has
//!     actually made twice.
//!   - a hardcoded literal reached through a wrapper this scan doesn't know
//!     the name of (a new crate-local `fn lms_path() -> &'static str {
//!     "lms" }` that some future spawn calls through would read as clean).
//!   - anything not spelled `Command::new("lms")` verbatim — a
//!     `format!`-built command line, a shell string (`sh -c "lms ..."`),
//!     `Command::new("lms".to_string())`.
//!   - `plugins/darkmux-bundler-rust` and `tools/darkmux-mock-model` — both
//!     `[workspace] exclude`d in the root `Cargo.toml` (see its comment),
//!     so `sweep_roots()`'s `crates_dir` listing never reaches either one.
//!     Neither plausibly spawns `lms` (a diff-bundler plugin and a mock
//!     chat-completions server), which is why this is a defensible gap
//!     rather than a fix — but it is a real one: planting the literal in
//!     either tree leaves this scan green. Any other future
//!     workspace-excluded crate has the same blind spot by construction.
//!   - any `build.rs` — `sweep_roots()` walks each member's `src/` tree
//!     only, and a build script lives at the crate root beside it, so a
//!     literal there is invisible to the same degree as the two trees
//!     above.
//!
//! Also out of scope, per #2572's own accounting: making the hardcoded form
//! unrepresentable rather than detectable. That is the durable fix; this
//! is the lint that holds until it lands.

use std::path::{Path, PathBuf};

/// The one literal shape this codebase's `lms`-binary antipattern has
/// actually taken (verified against the two #1939 sites before the fix).
const ANTIPATTERN: &str = "Command::new(\"lms\")";

fn manifest_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

/// The workspace root — `crates/darkmux-profiles` sits two levels under it.
fn workspace_root() -> PathBuf {
    manifest_dir()
        .parent()
        .and_then(Path::parent)
        .expect("crates/darkmux-profiles has two parent directories")
        .to_path_buf()
}

/// The CODE half of a line — everything before its first `//`. Without
/// this, this very module's own doc comments (which quote the antipattern
/// verbatim, deliberately, to document it) would trip the scan on
/// themselves. Same rationale as `pin_cwd_conformance.rs::code_only`, and
/// the same known limit: a `//` inside an earlier string literal on the
/// same line blinds the rest of that line, which can only cause a MISSED
/// finding, never a false one.
fn code_only(line: &str) -> &str {
    match line.find("//") {
        Some(i) => &line[..i],
        None => line,
    }
}

/// Every `.rs` file under `root`, recursively. Panics on a missing root — a
/// typo'd sweep path must not silently sweep zero files and pass vacuously.
fn rust_files(root: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    assert!(root.is_dir(), "conformance sweep root does not exist: {}", root.display());
    while let Some(dir) = stack.pop() {
        let entries = std::fs::read_dir(&dir)
            .unwrap_or_else(|e| panic!("reading {}: {e}", dir.display()));
        for entry in entries {
            let path = entry.unwrap_or_else(|e| panic!("reading an entry in {}: {e}", dir.display())).path();
            if path.is_dir() {
                stack.push(path);
            } else if path.extension().and_then(|e| e.to_str()) == Some("rs") {
                out.push(path);
            }
        }
    }
    out
}

/// Every `src/` tree that ships production code: the CLI crate's own
/// `src/`, every library member's `crates/<name>/src/`, and the internal
/// runtime's `runtime/src/` (not a workspace member, but it links
/// `darkmux_profiles`/`darkmux_types` the same way and is worth sweeping on
/// the same terms). Deliberately NOT any `tests/` directory anywhere — a
/// test fixture is allowed to construct the literal antipattern on purpose
/// (see `pin_cwd_conformance.rs`'s own synthetic fixtures), and this scan's
/// job is production spawns only.
fn sweep_roots() -> Vec<PathBuf> {
    let root = workspace_root();
    let mut roots = vec![root.join("src"), root.join("runtime/src")];
    let crates_dir = root.join("crates");
    let mut member_srcs: Vec<PathBuf> = std::fs::read_dir(&crates_dir)
        .unwrap_or_else(|e| panic!("reading {}: {e}", crates_dir.display()))
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.is_dir())
        .map(|p| p.join("src"))
        .filter(|p| p.is_dir())
        .collect();
    member_srcs.sort();
    roots.extend(member_srcs);
    roots
}

struct Finding {
    file: PathBuf,
    line: usize,
    text: String,
}

fn scan(root: &Path) -> Vec<Finding> {
    let mut findings = Vec::new();
    for file in rust_files(root) {
        let src = std::fs::read_to_string(&file).unwrap_or_else(|e| panic!("reading {}: {e}", file.display()));
        for (idx, line) in src.lines().enumerate() {
            if code_only(line).contains(ANTIPATTERN) {
                findings.push(Finding {
                    file: file.clone(),
                    line: idx + 1,
                    text: line.trim().to_string(),
                });
            }
        }
    }
    findings
}

#[test]
fn no_production_spawn_hardcodes_the_lms_binary_name() {
    let mut findings = Vec::new();
    for root in sweep_roots() {
        findings.extend(scan(&root));
    }
    assert!(
        findings.is_empty(),
        "found {} hardcoded `Command::new(\"lms\")` spawn(s) — an operator's `lms_bin` \
         override (env `DARKMUX_LMS_BIN` or `config.lms_bin`) is silently ignored by these \
         (#1939):\n{}\n\n\
         Fix: `Command::new(darkmux_types::config_access::lms_bin())` (or, inside \
         darkmux-profiles itself, the crate-local `crate::lms::lms_bin()` wrapper).",
        findings.len(),
        findings
            .iter()
            .map(|f| format!("  {}:{}: {}", f.file.display(), f.line, f.text))
            .collect::<Vec<_>>()
            .join("\n"),
    );
}

/// Negative control: a scanner whose sweep roots resolved to nothing (a
/// typo'd `workspace_root()`, an empty `crates/` listing) would report the
/// assertion above as vacuously green — a probe that passes without testing
/// anything, worse than no probe. Assert the scan actually walks a
/// non-trivial number of files AND can still see the two #1939 fix sites
/// resolving through the accessor, proving this test reads current source
/// rather than a stale assumption baked into the assertion above.
#[test]
fn the_scan_sees_real_files_and_the_1939_fix_sites() {
    let roots = sweep_roots();
    assert!(roots.len() >= 5, "sweep_roots() returned suspiciously few roots: {roots:?}");
    let total_files: usize = roots.iter().map(|r| rust_files(r).len()).sum();
    assert!(
        total_files > 50,
        "swept only {total_files} .rs files across {} roots — sweep_roots() is probably wrong",
        roots.len()
    );

    let scores_path = workspace_root().join("crates/darkmux-lab/src/lab/scores.rs");
    let scores = std::fs::read_to_string(&scores_path).unwrap_or_else(|e| panic!("reading {}: {e}", scores_path.display()));
    assert!(
        scores.contains("config_access::lms_bin()"),
        "scores.rs no longer resolves its engine-version probe through the accessor — did #1939 regress?"
    );

    let dispatch_path = workspace_root().join("crates/darkmux-crew/src/dispatch_internal.rs");
    let dispatch = std::fs::read_to_string(&dispatch_path).unwrap_or_else(|e| panic!("reading {}: {e}", dispatch_path.display()));
    assert!(
        dispatch.contains("config_access::lms_bin()"),
        "dispatch_internal.rs's probe_loaded_model_list no longer resolves through the accessor — did #1939 regress?"
    );
}

/// Third control: this module's own doc comment quotes the antipattern
/// verbatim (deliberately, to document it) — proving a COMMENT mentioning
/// the text does not trip the scan, the same failure mode #2534's
/// adversarial review found in the cwd-pin scan.
#[test]
fn a_comment_naming_the_antipattern_does_not_trip_the_scan() {
    let src = "fn spawner() {\n    // do not write: Command::new(\"lms\")\n}\n";
    let findings: Vec<_> = src
        .lines()
        .filter(|l| code_only(l).contains(ANTIPATTERN))
        .collect();
    assert!(
        findings.is_empty(),
        "a comment mentioning the antipattern text tripped the scan: {findings:?}"
    );
}
